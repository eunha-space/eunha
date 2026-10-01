//! Fetching remote ActivityPub objects into local rows: dereferencing a remote
//! status (following in-reply-to/quote references up to a bounded depth) and
//! resolving-or-fetching a remote account. These are the shared entry points
//! the inbound activity handlers use to materialise objects they reference.

use serde_json::Value;

use crate::{
    error::{AppError, AppResult},
    state::AppState,
};

use super::attachment::{ap_attachment_file_meta, classify_attachment_type};
use super::{as_string_vec, json_uri, sync_remote_poll};

/// Resolve a status by URI, fetching and storing it from its origin server if
/// not already known locally. Returns the local status id.
///
/// This stores the core of the Note (text, audience/visibility, in-reply-to and
/// quote linkage when the referenced posts are already local, and media); it
/// does not recurse into referenced posts. Returns `Ok(None)` if the object
/// can't be fetched or isn't a storable Note.
pub async fn fetch_remote_status(state: &AppState, uri: &str) -> AppResult<Option<i64>> {
    fetch_remote_status_depth(state, uri, None, 0).await
}

/// As [`fetch_remote_status`], for an object already in hand: the caller has
/// fetched it from the server that owns its id (Mastodon's `prefetched_body`),
/// so storing it needs no second request.
pub async fn fetch_remote_status_prefetched(
    state: &AppState,
    uri: &str,
    json: Value,
) -> AppResult<Option<i64>> {
    fetch_remote_status_depth(state, uri, Some(json), 0).await
}

/// Largest depth to which `fetch_remote_status` follows references (in-reply-to
/// and quoted posts), to avoid unbounded fetch chains.
const MAX_FETCH_DEPTH: u8 = 2;

/// Whether two URIs name the same HTTP(S) host, which is how Mastodon decides
/// whether an object's attribution can be believed; for portable ids, the
/// same DID, whose proof is what vouches for them.
fn same_host(a: &str, b: &str) -> bool {
    crate::federation::portable::same_authority(a, b)
}

async fn fetch_remote_status_depth(
    state: &AppState,
    uri: &str,
    prefetched: Option<Value>,
    depth: u8,
) -> AppResult<Option<i64>> {
    if uri.is_empty() {
        return Ok(None);
    }
    // Fetched by the id as given, hints and all, and stored and looked up by
    // its canonical form.
    let fetch_uri = uri;
    let canonical = crate::federation::portable::canonical(uri);
    let uri = canonical.as_str();
    if let Some(id) = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
        uri,
    )
    .fetch_optional(&state.db)
    .await?
    {
        return Ok(Some(id));
    }

    let fetched: Value = match prefetched {
        Some(json) => json,
        None => match crate::federation::fetch::signed_get_json(state, fetch_uri).await {
            Ok(v) => v,
            Err(_) => return Ok(None),
        },
    };

    let nested_fetched;
    let object = match fetched.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "Create" | "Update" => match fetched.get("object") {
            Some(o) if o.is_object() => o,
            Some(o) if o.is_string() => {
                let Some(object_uri) = o.as_str() else {
                    return Ok(None);
                };
                nested_fetched =
                    match crate::federation::fetch::signed_get_json(state, object_uri).await {
                        Ok(v) => v,
                        Err(_) => return Ok(None),
                    };
                &nested_fetched
            }
            _ => return Ok(None),
        },
        _ => &fetched,
    };

    // Only store Note-like objects.
    let obj_type = object.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if !matches!(obj_type, "Note" | "Article" | "Question") {
        return Ok(None);
    }
    let note_uri = object.get("id").and_then(|v| v.as_str()).unwrap_or(uri);

    let attributed_to = json_uri(object.get("attributedTo"));
    if attributed_to.is_empty() {
        return Ok(None);
    }
    // `FetchRemoteStatusService#trustworthy_attribution?`: a server may only
    // attribute a status to an account on its own host. Without this, anyone
    // who can get us to dereference a URL of theirs — a search for it is
    // enough — can hang a status off any account on the network.
    if !same_host(note_uri, attributed_to) {
        return Ok(None);
    }
    let Ok(account_id) = resolve_or_fetch_remote_account(state, attributed_to).await else {
        return Ok(None);
    };

    let text = object
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let spoiler_text = object
        .get("summary")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    // `@account.sensitized? || @status_parser.sensitive`.
    let sensitive = object
        .get("sensitive")
        .and_then(|s| s.as_bool())
        .unwrap_or(false)
        || sqlx::query_scalar!(
            r#"SELECT (sensitized_at IS NOT NULL) AS "s!" FROM accounts WHERE id = $1"#,
            account_id,
        )
        .fetch_optional(&state.db)
        .await?
        .unwrap_or(false);
    let url = object
        .get("url")
        .and_then(|u| u.as_str())
        .map(str::to_owned);
    let created_at = object
        .get("published")
        .and_then(|p| p.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc).naive_utc())
        .unwrap_or_else(|| chrono::Utc::now().naive_utc());

    let note_to = as_string_vec(object.get("to"));
    let note_cc = as_string_vec(object.get("cc"));
    let visibility = crate::db::models::vis::from_audience(&note_to, &note_cc);
    let language = object
        .get("contentMap")
        .and_then(|m| m.as_object())
        .and_then(|m| m.keys().next())
        .map(|s| s.to_string())
        .filter(|s| ["ko", "en"].contains(&s.as_str()));

    // Link in-reply-to: use the local copy if present, otherwise fetch it once.
    let in_reply_to_uri = object.get("inReplyTo").and_then(|v| v.as_str());
    let (in_reply_to_id, in_reply_to_account_id): (Option<i64>, Option<i64>) =
        if let Some(irt) = in_reply_to_uri {
            let mut found: Option<(i64, i64)> = sqlx::query!(
                "SELECT id, account_id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
                irt,
            )
            .fetch_optional(&state.db)
            .await?
            .map(|r| (r.id, r.account_id));
            if found.is_none() && depth < MAX_FETCH_DEPTH {
                if let Some(pid) =
                    Box::pin(fetch_remote_status_depth(state, irt, None, depth + 1)).await?
                {
                    found = sqlx::query!("SELECT id, account_id FROM statuses WHERE id = $1", pid)
                        .fetch_optional(&state.db)
                        .await?
                        .map(|r| (r.id, r.account_id));
                }
            }
            found
                .map(|(id, aid)| (Some(id), Some(aid)))
                .unwrap_or((None, None))
        } else {
            (None, None)
        };

    let status_id = crate::snowflake::next_id();
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO statuses
             (id, account_id, text, spoiler_text, visibility, sensitive,
              uri, url, in_reply_to_id, in_reply_to_account_id, reply,
              language, local, created_at, updated_at, quote_approval_policy)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12, false, $13, now(), $14)
           ON CONFLICT (uri) WHERE uri IS NOT NULL AND uri != '' DO NOTHING
           RETURNING id"#,
        status_id,
        account_id,
        text,
        spoiler_text,
        visibility,
        sensitive,
        note_uri,
        url,
        in_reply_to_id,
        in_reply_to_account_id,
        // A status with an inReplyTo is a reply even if its parent isn't local.
        in_reply_to_uri.is_some(),
        language,
        created_at,
        super::remote_quote_policy(state, account_id, object).await,
    )
    .fetch_optional(&state.db)
    .await?;

    // Lost an insert race — return the existing row.
    let Some(new_id) = inserted else {
        return Ok(sqlx::query_scalar!(
            "SELECT id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
            note_uri,
        )
        .fetch_optional(&state.db)
        .await?);
    };

    // Quote linkage (only if the quoted post is already local).
    let quote_uri = object
        .get("quote")
        .and_then(|v| v.as_str())
        .or_else(|| object.get("quoteUrl").and_then(|v| v.as_str()))
        .or_else(|| object.get("quoteUri").and_then(|v| v.as_str()))
        .or_else(|| object.get("_misskey_quote").and_then(|v| v.as_str()));
    if let Some(q) = quote_uri {
        let mut quoted: Option<(i64, i64)> = sqlx::query!(
            "SELECT id, account_id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
            q,
        )
        .fetch_optional(&state.db)
        .await?
        .map(|r| (r.id, r.account_id));
        if quoted.is_none() && depth < MAX_FETCH_DEPTH {
            if let Some(qid) =
                Box::pin(fetch_remote_status_depth(state, q, None, depth + 1)).await?
            {
                quoted = sqlx::query!("SELECT id, account_id FROM statuses WHERE id = $1", qid)
                    .fetch_optional(&state.db)
                    .await?
                    .map(|r| (r.id, r.account_id));
            }
        }
        if let Some((quoted_id, quoted_account_id)) = quoted {
            let _ = sqlx::query!(
                r#"INSERT INTO quotes
                     (id, status_id, quoted_status_id, account_id, quoted_account_id, state, created_at, updated_at)
                   VALUES ($1, $2, $3, $4, $5, 1, now(), now())
                   ON CONFLICT (status_id) DO NOTHING"#,
                crate::snowflake::next_id(),
                new_id,
                quoted_id,
                account_id,
                quoted_account_id,
            )
            .execute(&state.db)
            .await;
        }
    }

    // Media attachments.
    for att in object
        .get("attachment")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
    {
        let media_type_str = att.get("mediaType").and_then(|v| v.as_str()).unwrap_or("");
        let att_type = classify_attachment_type(
            att.get("type").and_then(|v| v.as_str()).unwrap_or(""),
            media_type_str,
        );
        let Some(remote_url) = att
            .get("url")
            .and_then(|v| v.as_str())
            .filter(|u| !u.is_empty())
        else {
            continue;
        };
        let description = att.get("name").and_then(|v| v.as_str()).map(str::to_owned);
        let blurhash = att
            .get("blurhash")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let file_content_type = (!media_type_str.is_empty()).then(|| media_type_str.to_owned());
        let file_meta = ap_attachment_file_meta(att);
        let _ = sqlx::query!(
            r#"INSERT INTO media_attachments
                 (id, account_id, status_id, remote_url, description, blurhash, type, file_content_type, file_meta, created_at, updated_at)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9, now(), now())"#,
            crate::snowflake::next_id(),
            account_id,
            new_id,
            remote_url,
            description,
            blurhash,
            att_type,
            file_content_type,
            file_meta,
        )
        .execute(&state.db)
        .await;
    }

    sync_remote_poll(state, new_id, account_id, object).await?;

    Ok(Some(new_id))
}

/// Looks up a remote account by URI, fetching it from the remote server if unknown.
pub async fn resolve_or_fetch_remote_account(state: &AppState, actor_uri: &str) -> AppResult<i64> {
    resolve_or_fetch_remote_account_inner(state, actor_uri, None).await
}

/// As [`resolve_or_fetch_remote_account`], for an actor document already in
/// hand (Mastodon's `prefetched_body`).
pub async fn resolve_or_fetch_remote_account_prefetched(
    state: &AppState,
    actor_uri: &str,
    json: Value,
) -> AppResult<i64> {
    resolve_or_fetch_remote_account_inner(state, actor_uri, Some(json)).await
}

async fn resolve_or_fetch_remote_account_inner(
    state: &AppState,
    actor_uri: &str,
    prefetched: Option<Value>,
) -> AppResult<i64> {
    // A portable actor is fetched by the id as given, whose location hints
    // say where, and known by its canonical id.
    let fetch_uri = actor_uri;
    let canonical = crate::federation::portable::canonical(actor_uri);
    let actor_uri = canonical.as_str();
    // An actor URI on our own domain is a *local* account, not a remote one.
    // Resolve it directly (local accounts store an empty `uri`, so the lookup
    // below would miss it) rather than signed-fetching our own actor endpoint,
    // which would mint a remote-looking duplicate with domain = our own domain.
    // Such duplicates break every `domain IS NULL` local check — e.g. a mention
    // resolving to the duplicate never fires the local mention notification.
    if let Ok(parsed) = url::Url::parse(actor_uri) {
        if parsed
            .host_str()
            .is_some_and(|h| h.eq_ignore_ascii_case(&state.instance.domain))
        {
            let segments: Vec<&str> = parsed
                .path_segments()
                .map(|s| s.collect())
                .unwrap_or_default();
            let local_id = match segments.as_slice() {
                // https://{domain}/users/{username}
                ["users", username] => {
                    sqlx::query_scalar!(
                        "SELECT id FROM accounts WHERE username = $1 AND domain IS NULL",
                        username,
                    )
                    .fetch_optional(&state.db)
                    .await?
                }
                // https://{domain}/ap/users/{id}
                ["ap", "users", id] => match id.parse::<i64>() {
                    Ok(numeric) => {
                        sqlx::query_scalar!(
                            "SELECT id FROM accounts WHERE id = $1 AND domain IS NULL",
                            numeric,
                        )
                        .fetch_optional(&state.db)
                        .await?
                    }
                    Err(_) => None,
                },
                _ => None,
            };
            // On our own domain, never fall through to a remote fetch: either we
            // found the local account or there is no such account.
            return local_id.ok_or(AppError::NotFound);
        }
    }

    if let Some(id) = sqlx::query_scalar!("SELECT id FROM accounts WHERE uri = $1", actor_uri)
        .fetch_optional(&state.db)
        .await?
    {
        return Ok(id);
    }

    let actor: Value = match prefetched {
        Some(json) => json,
        None => crate::federation::fetch::signed_get_json(state, fetch_uri)
            .await
            .map_err(AppError::Internal)?,
    };
    let portable = crate::federation::portable::reach(&actor);

    let username = actor
        .get("preferredUsername")
        .and_then(|u| u.as_str())
        .unwrap_or("unknown");
    let domain = match &portable {
        Some(reach) => reach.domain.clone(),
        None => url::Url::parse(actor_uri)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default(),
    };
    let display_name = actor
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let note = actor
        .get("summary")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let url = actor
        .get("url")
        .and_then(|u| u.as_str())
        .unwrap_or(actor_uri)
        .to_string();
    let inbox_url = actor
        .get("inbox")
        .and_then(|i| i.as_str())
        .unwrap_or("")
        .to_string();
    let outbox_url = actor
        .get("outbox")
        .and_then(|o| o.as_str())
        .unwrap_or("")
        .to_string();
    // As Mastodon's ProcessAccountService reads it, and stored as '' when
    // absent: the column is NOT NULL, and plenty of actors have no shared inbox.
    let shared_inbox_url = match actor.get("endpoints") {
        Some(endpoints) if endpoints.is_object() => endpoints.get("sharedInbox"),
        _ => actor.get("sharedInbox"),
    }
    .and_then(|s| s.as_str())
    .unwrap_or("")
    .to_string();
    // A portable actor's endpoints are `ap` ids too; it is reached at its
    // first gateway.
    let (inbox_url, outbox_url, shared_inbox_url) = match portable {
        Some(reach) => (reach.inbox, reach.outbox, reach.shared_inbox),
        None => (inbox_url, outbox_url, shared_inbox_url),
    };
    let public_key = actor
        .get("publicKey")
        .and_then(|k| k.get("publicKeyPem"))
        .and_then(|p| p.as_str())
        .unwrap_or("")
        .to_string();
    let avatar_remote_url = actor
        .get("icon")
        .and_then(|i| if i.is_object() { i.get("url") } else { None })
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let header_remote_url = actor
        .get("image")
        .and_then(|i| if i.is_object() { i.get("url") } else { None })
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if let Some(id) = sqlx::query_scalar!(
        r#"UPDATE accounts
           SET display_name = $2,
               note = $3,
               inbox_url = $4,
               shared_inbox_url = $5,
               public_key = $6,
               avatar_remote_url = COALESCE($7, avatar_remote_url),
               header_remote_url = CASE WHEN $8 != '' THEN $8 ELSE header_remote_url END,
               updated_at = now()
           WHERE uri = $1 AND uri != ''
           RETURNING id"#,
        actor_uri,
        display_name,
        note,
        inbox_url,
        shared_inbox_url,
        public_key,
        avatar_remote_url,
        header_remote_url,
    )
    .fetch_optional(&state.db)
    .await?
    {
        return Ok(id);
    }

    // `followers_url` and `following_url`, which a post's quote policy is read
    // against.
    let followers_url = actor
        .get("followers")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let following_url = actor
        .get("following")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let new_id = crate::snowflake::next_id();
    let id = sqlx::query_scalar!(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri,
              inbox_url, outbox_url, shared_inbox_url, public_key,
              avatar_remote_url, header_remote_url, followers_url, following_url,
              created_at, updated_at)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15, now(), now())
           RETURNING id"#,
        new_id,
        username,
        domain,
        display_name,
        note,
        url,
        actor_uri,
        inbox_url,
        outbox_url,
        shared_inbox_url,
        public_key,
        avatar_remote_url,
        header_remote_url,
        followers_url,
        following_url,
    )
    .fetch_one(&state.db)
    .await?;

    // `create_account`: a blocked domain's account starts out suspended or
    // limited, from the time of the block.
    crate::moderation::domain_block::apply_to_new_account(state, id, &domain)
        .await
        .map_err(AppError::Internal)?;

    Ok(id)
}
