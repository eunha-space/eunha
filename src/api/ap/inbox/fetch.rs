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
    Ok(fetch_remote_status_depth(state, uri, None, 0)
        .await?
        .map(|(id, _)| id))
}

/// As [`fetch_remote_status`], for an object already in hand: the caller has
/// fetched it from the server that owns its id (Mastodon's `prefetched_body`),
/// so storing it needs no second request.
pub async fn fetch_remote_status_prefetched(
    state: &AppState,
    uri: &str,
    json: Value,
) -> AppResult<Option<i64>> {
    Ok(fetch_remote_status_depth(state, uri, Some(json), 0)
        .await?
        .map(|(id, _)| id))
}

/// As [`fetch_remote_status_prefetched`], saying too whether the status is new
/// — FetchReplyWorker's `previously_new_record?`, which is what an async
/// refresh's `result_count` counts.
pub async fn store_remote_status_prefetched(
    state: &AppState,
    uri: &str,
    json: Value,
) -> AppResult<Option<(i64, bool)>> {
    fetch_remote_status_depth(state, uri, Some(json), 0).await
}

/// Largest depth to which `fetch_remote_status` follows references (in-reply-to
/// and quoted posts), to avoid unbounded fetch chains.
pub(super) const MAX_FETCH_DEPTH: u8 = 2;

/// [`fetch_remote_status`] at `depth` in a chain of references, with the
/// object already in hand when `prefetched`.
pub(super) async fn fetch_remote_status_at_depth(
    state: &AppState,
    uri: &str,
    prefetched: Option<Value>,
    depth: u8,
) -> AppResult<Option<i64>> {
    Ok(
        Box::pin(fetch_remote_status_depth(state, uri, prefetched, depth))
            .await?
            .map(|(id, _)| id),
    )
}

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
) -> AppResult<Option<(i64, bool)>> {
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
        return Ok(Some((id, false)));
    }
    // `FetchRemoteStatusService`: `return if domain_not_allowed?(uri)`.
    if crate::federation::moderation::domain_not_allowed(state, fetch_uri).await {
        return Ok(None);
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
                if let Some((pid, _)) =
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
        .await?
        .map(|id| (id, false)));
    };
    crate::fasp::events::status_created(state, new_id).await;

    // `process_quote` and `fetch_and_verify_quote`.
    Box::pin(super::quote::process_quote(
        state,
        new_id,
        account_id,
        object,
        fetched.get("@context"),
        depth,
    ))
    .await?;

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

    Ok(Some((new_id, true)))
}

/// Looks up a remote account by URI, fetching it from the remote server if unknown.
pub async fn resolve_or_fetch_remote_account(state: &AppState, actor_uri: &str) -> AppResult<i64> {
    resolve_or_fetch_remote_account_inner(state, actor_uri, None).await
}

/// Mastodon's `ActivityPub::FetchRemoteAccountService`: the account at
/// `actor_uri` as its server now describes it, fetched even when known, so
/// that what is read off it (a Move's `alsoKnownAs`) is current. A URI on
/// this instance is the local account, without a fetch.
pub async fn fetch_remote_account(state: &AppState, actor_uri: &str) -> AppResult<i64> {
    let ours = crate::federation::moderation::domain_of(actor_uri).is_some_and(|host| {
        host.eq_ignore_ascii_case(&state.instance.domain)
            || state
                .instance
                .aliases
                .iter()
                .any(|alias| host.eq_ignore_ascii_case(alias))
    });
    if ours {
        return crate::federation::local_uri::account(state, actor_uri)
            .await
            .ok_or(AppError::NotFound);
    }
    // `FetchRemoteActorService`: `return if domain_not_allowed?(uri)`.
    if crate::federation::moderation::domain_not_allowed(state, actor_uri).await {
        return Err(AppError::NotFound);
    }
    let actor = crate::federation::fetch::signed_get_json(state, actor_uri)
        .await
        .map_err(AppError::Internal)?;
    // The document has to be the actor asked for: one naming another would
    // rewrite that other account.
    match crate::federation::process_account::process_fetched_actor(
        state, actor_uri, &actor, false, false, None,
    )
    .await
    {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err(AppError::NotFound),
        Err(error) => {
            tracing::debug!(actor_uri, %error, "actor not stored");
            Err(AppError::NotFound)
        }
    }
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

/// The actor document ojak fetched to verify a signature, which the account
/// is stored from: in full for an actor not known yet or not refreshed for a
/// day, and otherwise only its keys, as Mastodon's signature verification
/// refreshes a key that no longer verifies (`keypair_refresh_key!`).
pub async fn store_key_fetched_actor(state: &AppState, actor: Value) -> AppResult<Option<i64>> {
    let Some(id) = actor.get("id").and_then(Value::as_str).map(str::to_owned) else {
        return Ok(None);
    };
    let canonical = crate::federation::portable::canonical(&id);
    let known = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE uri = $1 AND domain IS NOT NULL ORDER BY id LIMIT 1",
        canonical,
    )
    .fetch_optional(&state.db)
    .await?;
    let only_key = known
        .as_ref()
        .is_some_and(|account| !crate::federation::process_account::possibly_stale(account));
    crate::federation::process_account::process_fetched_actor(
        state, &id, &actor, false, only_key, None,
    )
    .await
    .map_err(AppError::Internal)
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

    let known = || async {
        sqlx::query_scalar!(
            "SELECT id FROM accounts WHERE uri = $1 ORDER BY id LIMIT 1",
            actor_uri
        )
        .fetch_optional(&state.db)
        .await
    };
    if let Some(id) = known().await? {
        return Ok(id);
    }
    // `FetchRemoteActorService` and `ProcessAccountService`: an account this
    // instance does not know is neither fetched nor created from a domain it
    // does not federate with.
    if crate::federation::moderation::domain_not_allowed(state, actor_uri).await {
        return Err(AppError::NotFound);
    }

    let actor: Value = match prefetched {
        Some(json) => json,
        None => crate::federation::fetch::signed_get_json(state, fetch_uri)
            .await
            .map_err(AppError::Internal)?,
    };
    match crate::federation::process_account::process_fetched_actor(
        state, actor_uri, &actor, false, false, None,
    )
    .await
    {
        Ok(Some(id)) => Ok(id),
        outcome => {
            if let Err(error) = outcome {
                tracing::debug!(actor_uri, %error, "actor not stored");
            }
            // Another request may have stored it meanwhile.
            known().await?.ok_or(AppError::NotFound)
        }
    }
}
