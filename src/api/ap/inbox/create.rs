//! Inbound `Create` activities: ingesting a remote Note/Article/Question into a
//! local status (with media, mentions, tags, polls and quote/reply linkage),
//! fanning it out to timelines, and the `Create`-carried poll-vote path.

use serde_json::Value;

use crate::{error::AppResult, state::AppState};

use super::attachment::preview_card_link;
use super::{
    acquire_create_lock, delete_arrived_first, fetch_remote_status, resolve_or_fetch_remote_account,
};
use ojak_vocab::json_ld_helper::{ids, type_is};

/// How a `Create` reached us: the options `ActivityPub::Activity` is given.
#[derive(Debug, Default, Clone)]
pub(crate) struct CreateOptions {
    /// Fetched by us rather than delivered (`fetch?`, which is `!@options
    /// [:delivery]`): what we asked for is taken whether or not it concerns
    /// anyone here.
    pub fetched: bool,
    /// `@options[:request_id]`, which the replies it leads us to fetch carry
    /// on, so that they count against one budget of discoveries.
    pub request_id: Option<String>,
    /// `@options[:depth]`: how deep in a chain of quotes this status was
    /// fetched, which bounds verifying its own.
    pub depth: u8,
}

pub(super) async fn handle_create(
    state: &AppState,
    _instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    create(state, activity, &CreateOptions::default()).await
}

/// `ActivityPub::Activity::Create#perform`, delivered or fetched.
pub(super) async fn create(
    state: &AppState,
    activity: &Value,
    create_options: &CreateOptions,
) -> AppResult<()> {
    let object = match activity.get("object") {
        Some(o) if o.is_object() => o,
        Some(Value::String(uri)) => {
            // `dereference_object!` (`ActivityPub::Dereferencer`): the object
            // fetched from the sender's host, and taken if it says it is
            // what was named; a temporary failure raises, for the activity
            // to be retried. What cannot be had leaves a `Create` of a bare
            // IRI, which is not a status.
            let actor_uri = activity.get("actor").and_then(Value::as_str).unwrap_or("");
            if crate::federation::json_ld::non_matching_uri_hosts(actor_uri, uri) {
                return Ok(());
            }
            let fetched = crate::federation::json_ld::fetch_resource_without_id_validation(
                state,
                uri,
                None,
                crate::federation::json_ld::RaiseOn::Temporary,
            )
            .await?
            .filter(|json| {
                crate::federation::json_ld::is_present(json)
                    && json.get("id").and_then(Value::as_str) == Some(uri.as_str())
            });
            let Some(object) = fetched else {
                return Ok(());
            };
            let mut dereferenced = activity.clone();
            dereferenced["object"] = object;
            return Box::pin(create(state, &dereferenced, create_options)).await;
        }
        _ => return Ok(()),
    };
    // `unsupported_object_type?`: a `Note` or `Question`, or one of the
    // kinds that are converted into a status (`Article`, `Video`, …).
    if !super::status_parser::is_status_type(object) {
        return Ok(());
    }

    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let note_uri = object.get("id").and_then(|i| i.as_str()).unwrap_or("");
    if note_uri.is_empty() || actor_uri.is_empty() {
        return Ok(());
    }

    // An embedded note is its sender's to vouch for only when it is on the
    // sender's origin and says the sender wrote it. Anything else is fetched
    // from where its id says it lives, and trusted as that server serves it:
    // otherwise any server could store a note under someone else's URI, which
    // the real one could then never take.
    let attributed: Vec<&str> = match object.get("attributedTo") {
        Some(Value::String(uri)) => vec![uri.as_str()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.as_str().or_else(|| item.get("id")?.as_str()))
            .collect(),
        Some(item @ Value::Object(_)) => {
            item.get("id").and_then(Value::as_str).into_iter().collect()
        }
        _ => Vec::new(),
    };
    // `return reject_payload! if non_matching_uri_hosts?(@account.uri,
    // object_uri)`.
    if crate::federation::json_ld::non_matching_uri_hosts(actor_uri, note_uri) {
        return Ok(());
    }
    // On the sender's host but on another origin, or attributed to someone
    // else: fetched from where its id says it lives
    // (`embedded-note-attribution-fetched`).
    if !ojak::origin::same_origin(note_uri, actor_uri)
        || (!attributed.is_empty() && !attributed.contains(&actor_uri))
    {
        let _ = fetch_remote_status(state, note_uri).await?;
        return Ok(());
    }

    // Serialize against a concurrent Delete for this uri so its `delete_later`
    // can't slip in between the check below and our insert. Held for the whole
    // creation (released when this guard drops on return).
    let _create_lock = acquire_create_lock(state, note_uri).await;

    // Skip a Create whose Delete already arrived out of order (Redis tombstone),
    // in addition to the persistent tombstone check below.
    if delete_arrived_first(state, actor_uri, note_uri).await {
        return Ok(());
    }

    // Tombstone check
    let tombstoned = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM tombstones WHERE uri = $1)",
        note_uri,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false);
    if tombstoned {
        return Ok(());
    }

    let account_id = match resolve_or_fetch_remote_account(state, actor_uri).await {
        Ok(id) => id,
        Err(_) => return Ok(()),
    };
    // `@account.schedule_refresh_if_stale!`.
    crate::federation::process_account::schedule_refresh_if_stale(state, account_id).await;

    // Parse tag array once: mentions, hashtags, emojis.
    // ActivityPub allows "tag" to be either a single object or an array.
    let tags_arr: Vec<Value> = match object.get("tag") {
        Some(Value::Array(arr)) => arr.clone(),
        Some(obj @ Value::Object(_)) => vec![obj.clone()],
        _ => vec![],
    };

    let mention_hrefs: Vec<String> = tags_arr
        .iter()
        .filter(|t| type_is(t, "Mention"))
        .filter_map(|t| t.get("href").and_then(|v| v.as_str()))
        .filter(|href| !href.trim().is_empty())
        .map(str::to_owned)
        .collect();

    // `StatusParser#audience_to` and `#audience_cc`: the object's, or, when
    // it has none, the activity's; each an IRI or an embedded object's id.
    let audience_of = |key: &str| -> Vec<String> {
        ids(object
            .get(key)
            .filter(|v| !v.is_null())
            .or_else(|| activity.get(key)))
        .into_iter()
        .map(str::to_owned)
        .collect()
    };
    let audience_to = audience_of("to");
    let audience_cc = audience_of("cc");
    let audience: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        audience_to
            .iter()
            .chain(&audience_cc)
            .filter(|uri| seen.insert(uri.as_str()))
            .cloned()
            .collect()
    };
    // `@options[:delivered_to_account_id]`: the local account whose inbox it
    // was delivered to.
    let delivered_to = super::delivered_to(state, activity).await;

    // `replied_to_status`: by `inReplyTo`, or its `inReplyToAtomUri`.
    let in_reply_to_uri = object
        .get("inReplyTo")
        .and_then(crate::federation::json_ld::value_or_id)
        .filter(|uri| !uri.trim().is_empty());
    let mut replied_to_id = match in_reply_to_uri {
        Some(uri) => super::kept_status(state, uri).await?,
        None => None,
    };
    if let (Some(_), None) = (in_reply_to_uri, replied_to_id) {
        if let Some(atom_uri) = object
            .get("inReplyToAtomUri")
            .and_then(Value::as_str)
            .filter(|uri| !uri.trim().is_empty())
        {
            replied_to_id = super::kept_status(state, atom_uri).await?;
        }
    }
    // `responds_to_followed_account?` reads the replied-to author: local, or
    // followed by someone.
    let replied_to = match replied_to_id {
        Some(id) => sqlx::query!(
            r#"SELECT s.id, (a.domain IS NULL) AS "is_local!",
                      EXISTS (SELECT 1 FROM follows f WHERE f.target_account_id = s.account_id) AS "followed!"
               FROM statuses s JOIN accounts a ON a.id = s.account_id
               WHERE s.id = $1"#,
            id,
        )
        .fetch_optional(&state.db)
        .await?,
        None => None,
    };
    // `Status#thread`, and `carried_over_reply_to_account_id`.
    let thread = match &replied_to {
        Some(r) => crate::conversation::thread(&state.db, r.id).await?,
        None => None,
    };
    let in_reply_to_id = thread.map(|t| t.id);
    let in_reply_to_account_id = thread.and_then(|t| t.reply_to_account_id(account_id));

    // Mastodon serializes poll votes as Create(Note) where the Note's
    // `inReplyTo` is the poll status and `name` is the selected option. Store
    // these as poll_votes instead of creating a visible status.
    if let (Some(parent_id), Some(choice_name)) = (
        replied_to.as_ref().map(|r| r.id),
        object
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty()),
    ) {
        if handle_poll_vote_note(state, account_id, parent_id, choice_name, note_uri).await? {
            return Ok(());
        }
    }

    // `StatusParser#visibility`, against the author's followers collection.
    let followers_url: String = sqlx::query_scalar!(
        "SELECT followers_url FROM accounts WHERE id = $1",
        account_id
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or_default();
    let mut visibility =
        crate::db::models::vis::of_remote(&audience_to, &audience_cc, &followers_url);

    // `related_to_local_activity?`, which whatever we fetched ourselves is
    // (`fetch?`).
    if !create_options.fetched {
        // `followed_by_local_accounts?`.
        let followed_by_local_accounts =
            super::followed_by_local_accounts(state, activity, account_id).await?;
        // `addresses_local_accounts?`: delivered to a local inbox, or
        // addressed to a local account.
        let mut addresses_local_accounts = delivered_to.is_some();
        for uri in &audience {
            if addresses_local_accounts {
                break;
            }
            addresses_local_accounts = crate::federation::local_uri::is_local(state, uri)
                && crate::federation::local_uri::account(state, uri)
                    .await
                    .is_some();
        }
        // `requested_through_relay?`.
        let requested_through_relay = activity
            .get(super::THROUGH_RELAY)
            .is_some_and(|flag| flag == &Value::Bool(true));
        // `responds_to_followed_account?`.
        let responds_to_followed_account = replied_to
            .as_ref()
            .is_some_and(|r| r.is_local || r.followed);
        let related = match visibility {
            crate::db::models::vis::PUBLIC | crate::db::models::vis::UNLISTED => {
                followed_by_local_accounts
                    || requested_through_relay
                    || responds_to_followed_account
                    || addresses_local_accounts
            }
            crate::db::models::vis::PRIVATE => {
                followed_by_local_accounts || addresses_local_accounts
            }
            _ => addresses_local_accounts,
        };
        if !related {
            tracing::debug!(
                note_uri,
                "Create(Note): ignoring, not related to local activity"
            );
            return Ok(());
        }
    }

    // `find_existing_status`: by its id, or by its `atomUri`. One stored
    // under another author stays theirs ("authorship change is not
    // supported"); one of the sender's own, delivered again to a local
    // inbox, is given to that inbox's owner (`postprocess_audience_and_deliver`).
    let mut existing = super::kept_status(state, note_uri).await?;
    if existing.is_none() {
        if let Some(atom_uri) = object
            .get("atomUri")
            .and_then(Value::as_str)
            .filter(|uri| !uri.trim().is_empty())
        {
            existing = super::kept_status(state, atom_uri).await?;
        }
    }
    if let Some(existing_id) = existing {
        let author: Option<i64> =
            sqlx::query_scalar!("SELECT account_id FROM statuses WHERE id = $1", existing_id)
                .fetch_optional(&state.db)
                .await?;
        if author == Some(account_id) {
            if let Some(recipient) = delivered_to {
                postprocess_audience_and_deliver(state, existing_id, account_id, recipient).await?;
            }
        }
        return Ok(());
    }

    // `process_mention`: an account we know, or fetch. One that cannot be
    // fetched for now is tried again later (`@unresolved_mentions`); one that
    // is not there is left out.
    let mut mentions: Vec<Mention> = Vec::new();
    let mut unresolved_mentions: Vec<String> = Vec::new();
    for href in &mention_hrefs {
        match resolve_or_fetch_remote_account(state, href).await {
            Ok(id) => {
                if !mentions.iter().any(|m| m.account_id == id) {
                    mentions.push(Mention {
                        account_id: id,
                        silent: false,
                    });
                }
            }
            Err(error) if fetch_failed_for_now(&error) => {
                if !unresolved_mentions.contains(href) {
                    unresolved_mentions.push(href.clone());
                }
            }
            Err(_) => {}
        }
    }

    // `process_audience`: every account we already know in `to` and `cc`,
    // and the owner of the inbox it was delivered to, can see it. Those not
    // tagged are mentioned silently, which makes a direct message a
    // limited one; a tagged local account outside the audience is mentioned
    // but not told (`@silenced_account_ids`).
    let mut accounts_in_audience: Vec<i64> = Vec::new();
    for uri in &audience {
        if ojak_vocab::is_public_collection(uri) {
            continue;
        }
        if let Some(id) = crate::federation::local_uri::account(state, uri).await {
            if !accounts_in_audience.contains(&id) {
                accounts_in_audience.push(id);
            }
        }
    }
    if let Some(recipient) = delivered_to {
        if !accounts_in_audience.contains(&recipient) {
            accounts_in_audience.push(recipient);
        }
    }
    for &id in &accounts_in_audience {
        if mentions.iter().any(|m| m.account_id == id) {
            continue;
        }
        mentions.push(Mention {
            account_id: id,
            silent: true,
        });
        if visibility == crate::db::models::vis::DIRECT {
            visibility = crate::db::models::vis::LIMITED;
        }
    }
    let mentioned_ids: Vec<i64> = mentions.iter().map(|m| m.account_id).collect();
    let local_mentioned: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE id = ANY($1) AND domain IS NULL",
        &mentioned_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let silenced_account_ids: Vec<i64> = local_mentioned
        .iter()
        .copied()
        .filter(|id| !accounts_in_audience.contains(id))
        .collect();

    // Field extraction: `processed_text` and `processed_spoiler_text`, which
    // for a converted object are its title, summary and a link to it.
    let text = super::status_parser::processed_text(state, object);
    let spoiler_text = super::status_parser::processed_spoiler_text(object);
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
    // `@status_parser.url || @status_parser.uri`.
    let url = super::status_parser::url(object).or_else(|| Some(note_uri.to_owned()));
    let published = object
        .get("published")
        .and_then(|p| p.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc).naive_utc());
    // `edited_at`, unless it is the same moment as `created_at`.
    let edited_at = object
        .get("updated")
        .and_then(|p| p.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc).naive_utc())
        .filter(|edited| Some(*edited) != published);

    // `conversation_from_uri(@object['conversation'])`.
    let conversation_id = match object.get("conversation").and_then(Value::as_str) {
        Some(uri) => crate::conversation::from_uri(state, uri).await?,
        None => None,
    };

    // `StatusParser#language`.
    let language = super::status_parser::language(object);

    let status_id = crate::snowflake::next_id();
    let created_at = published.unwrap_or_else(|| chrono::Utc::now().naive_utc());

    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO statuses
             (id, account_id, text, spoiler_text, visibility, sensitive,
              uri, url, in_reply_to_id, in_reply_to_account_id, reply,
              language, local, created_at, edited_at, updated_at, quote_approval_policy,
              conversation_id)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12, false, $13,$14, now(), $15, $16)
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
        // A status with an inReplyTo is a reply even when its parent isn't known
        // locally; marking it so lets the home-feed reply filter treat an
        // unresolved-parent reply as an orphan (hidden) instead of a top-level post.
        in_reply_to_uri.is_some(),
        language,
        created_at,
        edited_at,
        super::remote_quote_policy(state, account_id, object).await,
        conversation_id,
    )
    .fetch_optional(&state.db)
    .await?;

    let Some(inserted_id) = inserted else {
        return Ok(()); // duplicate
    };
    // `set_conversation` and `update_conversation`.
    crate::conversation::assign(&state.db, inserted_id).await?;

    // The same call the API path makes: a status is a status however it
    // arrived, and the conditions belong to `counters`, not here. `inserted_id`
    // is only `Some` for a row that was new, so a redelivered Create counts once.
    if let Err(e) = crate::counters::on_status_created(
        &state.db,
        account_id,
        visibility,
        in_reply_to_id,
        created_at,
    )
    .await
    {
        tracing::error!(account_id, error = %e, "failed to count a federated status");
    }
    crate::fasp::events::status_created(state, inserted_id).await;
    crate::search::elasticsearch::indexing::status(state, inserted_id).await;
    crate::search::elasticsearch::indexing::account(state, account_id).await;

    // `process_quote` and `fetch_and_verify_quote`: the quote is recorded
    // pending and verified against its stamp, after the status is inserted so
    // that a quoted post that quotes back cannot recurse forever.
    super::quote::process_quote(
        state,
        inserted_id,
        account_id,
        object,
        activity.get("@context"),
        create_options.depth,
    )
    .await?;

    // `attach_counts`: the counts the status's server reports.
    super::status_parser::store_untrusted_counts(state, inserted_id, object).await?;

    // Media attachments. Domains blocked with `reject_media` (or fully
    // suspended) federate text but not media, so skip storing attachments.
    let attachments: Vec<Value> =
        if crate::federation::moderation::actor_media_rejected(state, actor_uri).await {
            Vec::new()
        } else {
            super::attachment::attachments_of(object)
        };
    let mut media_ids: Vec<i64> = Vec::new();
    for att in &attachments {
        // Mastodon caps a status at MEDIA_ATTACHMENTS_LIMIT (4).
        if media_ids.len() >= 4 {
            break;
        }
        let Some(media) = super::attachment::remote_media(att) else {
            continue;
        };
        let media_id = crate::snowflake::next_id();
        match sqlx::query_scalar!(
            r#"INSERT INTO media_attachments
                 (id, account_id, status_id, remote_url, description, blurhash,
                  type, thumbnail_remote_url, file_content_type, file_meta, created_at, updated_at)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10, now(), now())
               RETURNING id"#,
            media_id,
            account_id,
            inserted_id,
            media.remote_url,
            media.description,
            media.blurhash,
            media.kind,
            media.thumbnail_remote_url,
            media.file_content_type,
            media.file_meta,
        )
        .fetch_one(&state.db)
        .await
        {
            Ok(id) => media_ids.push(id),
            Err(e) => tracing::warn!(error = %e, "failed to insert media attachment"),
        }
    }
    // `ordered_media_attachment_ids: attachment_ids`, empty when there are
    // none: the attachments in the order the object lists them.
    sqlx::query!(
        "UPDATE statuses SET ordered_media_attachment_ids = $1 WHERE id = $2",
        &media_ids,
        inserted_id,
    )
    .execute(&state.db)
    .await?;

    // `LinkCrawlWorker.perform_in(rand(DISTRIBUTE_DELAY), @status.id,
    // @links.first)`: the card named by the first FEP-8967 `Link`
    // attachment, or else the first link in the content.
    crate::preview_card::crawl_later(
        state,
        inserted_id,
        preview_card_link(&attachments).map(str::to_owned),
    )
    .await;

    // Hashtags
    let hashtag_names: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        tags_arr
            .iter()
            .filter(|t| type_is(t, "Hashtag"))
            .filter_map(|t| {
                t.get("name")
                    .and_then(|v| v.as_str())
                    .map(|n| n.trim_start_matches('#').to_lowercase())
            })
            .filter(|n| !n.is_empty() && seen.insert(n.clone()))
            .collect()
    };
    let mut tag_ids: Vec<i64> = Vec::new();
    for name in &hashtag_names {
        let found = match crate::tags::find_or_create(&state.db, name).await {
            Ok(Some(id)) => sqlx::query_scalar!(
                "UPDATE tags SET last_status_at = now(), updated_at = now() WHERE id = $1 RETURNING id",
                id
            )
            .fetch_optional(&state.db)
            .await,
            other => other,
        };
        match found {
            Ok(Some(id)) => {
                tag_ids.push(id);
                crate::search::elasticsearch::indexing::tags(state, &[id]).await;
                let _ = sqlx::query!(
                    "INSERT INTO statuses_tags (status_id, tag_id) VALUES ($1,$2) ON CONFLICT DO NOTHING",
                    inserted_id,
                    id,
                )
                .execute(&state.db)
                .await;
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(tag = name, error = %e, "failed to upsert hashtag"),
        }
    }

    // `ActivityPub::Activity::Create#process_status`: `Trends.tags.register`.
    crate::trends::register_tags(state, inserted_id).await;
    // `# Update featured tags`: a public or unlisted post's tags counted in.
    if let Ok(Some(row)) = sqlx::query!(
        "SELECT visibility, created_at FROM statuses WHERE id = $1",
        inserted_id
    )
    .fetch_optional(&state.db)
    .await
    {
        if let Err(error) = crate::featured_tags::update_for_status(
            &state.db,
            account_id,
            inserted_id,
            row.visibility,
            row.created_at,
            &[],
            &tag_ids,
        )
        .await
        {
            tracing::warn!(%error, "could not count a status into its featured tags");
        }
    }

    // `attach_mentions`: every mention is stored before anyone is notified.
    // The order matters: a mention is dropped when the status also mentions
    // someone the recipient blocks, which cannot be seen while the rest are
    // still unwritten.
    for mention in &mentions {
        sqlx::query!(
            r#"INSERT INTO mentions (status_id, account_id, silent, created_at, updated_at)
               VALUES ($1, $2, $3, now(), now()) ON CONFLICT DO NOTHING"#,
            inserted_id,
            mention.account_id,
            mention.silent,
        )
        .execute(&state.db)
        .await?;
    }
    // `resolve_unresolved_mentions`.
    for uri in unresolved_mentions {
        resolve_mention_later(state, inserted_id, uri, create_options.request_id.clone()).await;
    }

    // `DistributionWorker` runs only for a status within the real-time
    // window (`Status#within_realtime_window?`), and with it the mention
    // notifications and the home and list feeds: a status fetched long after
    // it was written is stored, not announced.
    let within_realtime_window =
        chrono::Utc::now().naive_utc() - created_at <= chrono::Duration::hours(6);
    let actor_info = sqlx::query!(
        "SELECT display_name, username, domain, avatar_remote_url FROM accounts WHERE id = $1",
        account_id,
    )
    .fetch_optional(&state.db)
    .await?;
    // `notify_mentioned_accounts!`: the local accounts of `active_mentions`,
    // those outside the audience as if the sender were limited.
    if let (true, Some(info)) = (within_realtime_window, &actor_info) {
        let acct = match &info.domain {
            Some(d) => format!("{}@{}", info.username, d),
            None => info.username.clone(),
        };
        for mention in mentions.iter().filter(|m| !m.silent) {
            if !local_mentioned.contains(&mention.account_id) {
                continue;
            }
            crate::push::create_and_push_with(
                state,
                mention.account_id,
                account_id,
                "mention",
                Some(inserted_id),
                format!("New mention from {}", info.display_name),
                acct.clone(),
                info.avatar_remote_url.clone().unwrap_or_default(),
                silenced_account_ids.contains(&mention.account_id),
            )
            .await;
        }
    }

    // Custom emojis
    let actor_domain = url::Url::parse(actor_uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned));
    for tag in tags_arr.iter().filter(|t| type_is(t, "Emoji")) {
        let shortcode = match tag.get("name").and_then(|v| v.as_str()) {
            Some(n) => n.trim_matches(':').to_string(),
            None => continue,
        };
        let image_remote_url = match tag
            .get("icon")
            .and_then(|i| i.get("url"))
            .and_then(|v| v.as_str())
        {
            Some(u) => u.to_string(),
            None => continue,
        };
        let uri = tag.get("id").and_then(|v| v.as_str()).map(str::to_owned);
        let emoji_id = crate::snowflake::next_id();
        let _ = sqlx::query!(
            r#"INSERT INTO custom_emojis
                 (id, shortcode, domain, image_remote_url, uri, created_at, updated_at)
               VALUES ($1,$2,$3,$4,$5,now(),now())
               ON CONFLICT (shortcode, domain)
               DO UPDATE SET image_remote_url = EXCLUDED.image_remote_url, updated_at = now()"#,
            emoji_id,
            shortcode,
            actor_domain,
            image_remote_url,
            uri,
        )
        .execute(&state.db)
        .await;
    }

    // `process_tagged_collection` and `attach_tagged_objects`: the
    // collections its `FeaturedCollection` tags name, those not reached
    // resolved later (`TaggedCollectionResolveWorker`).
    crate::federation::tagged_collections::attach(state, inserted_id, object).await;

    // `process_poll`.
    if let Some(poll) = super::poll_parser::PollParser::parse(object) {
        let votes_count = poll.votes_count();
        let super::poll_parser::PollParser {
            multiple,
            options,
            cached_tallies,
            expires_at,
            voters_count,
        } = poll;
        let poll_id = crate::snowflake::next_id();
        if let Ok(Some(_)) = sqlx::query_scalar!(
            r#"INSERT INTO polls
                 (id, status_id, account_id, options, cached_tallies, votes_count,
                  multiple, expires_at, voters_count, created_at, updated_at)
               SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,now(),now()
               WHERE NOT EXISTS (SELECT 1 FROM polls WHERE status_id = $2)
               RETURNING id"#,
            poll_id,
            inserted_id,
            account_id,
            &options as &[String],
            &cached_tallies as &[i64],
            votes_count,
            multiple,
            expires_at,
            voters_count,
        )
        .fetch_optional(&state.db)
        .await
        {
            state.queues.polls.notify_one();
            let _ = sqlx::query!(
                "UPDATE statuses SET poll_id = $1 WHERE id = $2",
                poll_id,
                inserted_id,
            )
            .execute(&state.db)
            .await;
        }
    }

    // `fetch_replies`: the first page of the new status's replies, from its
    // author's server.
    if let Some(collection) = object.get("replies").filter(|r| !r.is_null()) {
        crate::federation::replies::fetch_replies_on_create(
            state,
            actor_uri.to_owned(),
            collection.clone(),
            create_options.request_id.clone(),
        )
        .await;
    }

    // Thread resolution: store the unknown parent if it is dereferenceable
    // (`ThreadResolveWorker.perform_async(status.id, in_reply_to_uri)`).
    if let (Some(uri), None) = (in_reply_to_uri, in_reply_to_id) {
        crate::jobs::push(
            state,
            ThreadResolveWorker {
                child_status_id: inserted_id,
                parent_url: uri.to_owned(),
                request_id: create_options.request_id.clone(),
            },
        )
        .await;
    }

    // Fanout to home and list feeds, then stream it (`DistributionWorker`).
    if !within_realtime_window {
        return Ok(());
    }
    crate::feed::distribute_later(state, inserted_id).await;

    Ok(())
}

/// A mention the status is stored with: `Mention.new(account:, silent:)`.
struct Mention {
    account_id: i64,
    silent: bool,
}

/// `ActivityPub::Activity::Create::PROCESSING_DELAY`, in seconds: when an
/// unresolved mention is first tried again.
const PROCESSING_DELAY: std::ops::RangeInclusive<u64> = 30..=600;

/// Whether resolving an account failed for now rather than for good: the
/// server did not answer (`*Mastodon::HTTP_CONNECTION_ERRORS`). An answer,
/// whatever it said, leaves the account out.
pub(super) fn fetch_failed_for_now(error: &crate::error::AppError) -> bool {
    match error {
        crate::error::AppError::Internal(error) => error
            .downcast_ref::<ojak::fetch::FetchError>()
            .is_some_and(|error| matches!(error, ojak::fetch::FetchError::Request(_))),
        _ => false,
    }
}

/// `MentionResolveWorker.perform_in(rand(PROCESSING_DELAY), status_id, uri,
/// { 'request_id' => … })`.
pub(super) async fn resolve_mention_later(
    state: &AppState,
    status_id: i64,
    uri: String,
    request_id: Option<String>,
) {
    let delay = std::time::Duration::from_secs(rand::random_range(PROCESSING_DELAY));
    crate::jobs::push_in(
        state,
        delay,
        MentionResolveWorker {
            status_id,
            uri,
            request_id,
        },
    )
    .await;
}

/// `postprocess_audience_and_deliver`: a status we hold, delivered again to
/// a local inbox whose owner it does not mention, is shared with them by a
/// silent mention, which makes a direct message a limited one, and goes
/// into their home feed if they follow its author.
async fn postprocess_audience_and_deliver(
    state: &AppState,
    status_id: i64,
    author_id: i64,
    recipient_id: i64,
) -> AppResult<()> {
    let inserted = sqlx::query!(
        r#"INSERT INTO mentions (status_id, account_id, silent, created_at, updated_at)
           VALUES ($1, $2, true, now(), now()) ON CONFLICT DO NOTHING"#,
        status_id,
        recipient_id,
    )
    .execute(&state.db)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Ok(());
    }
    sqlx::query!(
        "UPDATE statuses SET visibility = $2, updated_at = now() WHERE id = $1 AND visibility = $3",
        status_id,
        crate::db::models::vis::LIMITED,
        crate::db::models::vis::DIRECT,
    )
    .execute(&state.db)
    .await?;
    let following = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2) AS "e!""#,
        recipient_id,
        author_id,
    )
    .fetch_one(&state.db)
    .await?;
    if following {
        // `FeedInsertWorker.perform_async(@status.id, recipient, 'home')`.
        crate::feed::insert_into_home(state, status_id, recipient_id).await;
    }
    Ok(())
}

/// `MentionResolveWorker`: a mention whose account could not be fetched
/// when its status arrived, tried again with `ExponentialBackoff` on the
/// `pull` queue, seven times. Found, it is stored as a mention that is not
/// silent; an account that is not there leaves nothing to do, and the
/// status is not distributed again, as upstream does not.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct MentionResolveWorker {
    pub status_id: i64,
    pub uri: String,
    #[serde(default)]
    pub request_id: Option<String>,
}

impl crate::jobs::Job for MentionResolveWorker {
    const KIND: &'static str = "MentionResolveWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Pull)
        .retry(7);

    fn retry_in(count: u32) -> Option<std::time::Duration> {
        crate::jobs::exponential_backoff(count)
    }

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM statuses WHERE id = $1 AND deleted_at IS NULL) AS "e!""#,
            self.status_id
        )
        .fetch_one(&state.db)
        .await?;
        if !exists {
            return Ok(());
        }
        let account_id = match resolve_or_fetch_remote_account(state, &self.uri).await {
            Ok(id) => id,
            Err(error) if fetch_failed_for_now(&error) => {
                anyhow::bail!("could not fetch mentioned account {}: {error:?}", self.uri)
            }
            Err(_) => return Ok(()),
        };
        // `status.mentions.upsert({ account_id:, silent: false })`.
        sqlx::query!(
            r#"INSERT INTO mentions (status_id, account_id, silent, created_at, updated_at)
               VALUES ($1, $2, false, now(), now())
               ON CONFLICT (account_id, status_id) DO UPDATE SET silent = false, updated_at = now()"#,
            self.status_id,
            account_id,
        )
        .execute(&state.db)
        .await?;
        Ok(())
    }
}

/// `poll_vote?` and `poll_vote!`: a reply named after an option of a local
/// post's poll is a vote, stored unless the poll has ended, and not a
/// status. Says whether it was one. A vote `PollVote`'s validations refuse
/// (`VoteValidator`: the voter's own poll, or a vote already cast) is taken
/// and dropped.
pub(super) async fn handle_poll_vote_note(
    state: &AppState,
    voter_id: i64,
    status_id: i64,
    choice_name: &str,
    vote_uri: &str,
) -> AppResult<bool> {
    // `replied_to_status.preloadable_poll`, of a `local?` status, with the
    // option named.
    let Some(poll) = sqlx::query!(
        r#"SELECT p.id, p.account_id, p.options, p.multiple, p.expires_at,
                  COALESCE(p.hide_totals, false) AS "hide_totals!"
           FROM polls p
           JOIN statuses s ON s.id = p.status_id
           WHERE p.status_id = $1 AND (s.local OR s.uri IS NULL)"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(false);
    };
    let Some(choice) = poll.options.iter().position(|option| option == choice_name) else {
        return Ok(false);
    };
    let choice = choice as i32;

    // `poll_vote! unless replied_to_status.preloadable_poll.expired?`.
    if poll
        .expires_at
        .is_some_and(|e| e <= chrono::Utc::now().naive_utc())
    {
        return Ok(true);
    }
    // `VoteValidator#self_vote?`.
    if poll.account_id == voter_id {
        return Ok(true);
    }

    let previous: Vec<i32> = sqlx::query_scalar!(
        "SELECT choice FROM poll_votes WHERE poll_id = $1 AND account_id = $2",
        poll.id,
        voter_id,
    )
    .fetch_all(&state.db)
    .await?;
    let already_voted = !previous.is_empty();
    // `additional_voting_not_allowed?`: any vote on a single-choice poll, the
    // same choice again on a multiple-choice one.
    if (!poll.multiple && already_voted) || previous.contains(&choice) {
        return Ok(true);
    }

    sqlx::query!(
        r#"INSERT INTO poll_votes (account_id, poll_id, choice, uri, created_at, updated_at)
           VALUES ($1, $2, $3, $4, now(), now())"#,
        voter_id,
        poll.id,
        choice,
        vote_uri,
    )
    .execute(&state.db)
    .await?;

    // `PollVote#increment_counter_cache`, and `increment_voters_count!`
    // unless the voter had voted already.
    crate::api::mastodon::polls::count_vote(&state.db, poll.id, choice, !already_voted).await?;

    // `ActivityPub::DistributePollUpdateWorker.perform_in(3.minutes, …)
    // unless replied_to_status.preloadable_poll.hide_totals?`.
    if !poll.hide_totals {
        crate::jobs::push_in(
            state,
            std::time::Duration::from_secs(3 * 60),
            DistributePollUpdateWorker { status_id },
        )
        .await;
    }

    Ok(true)
}

/// `ActivityPub::DistributePollUpdateWorker`: a local poll's tallies, sent
/// to those who have seen it a while after a vote, one at a time per status
/// (`lock: :until_executed`), on the `push` queue and never retried.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct DistributePollUpdateWorker {
    pub status_id: i64,
}

impl crate::jobs::Job for DistributePollUpdateWorker {
    const KIND: &'static str = "ActivityPub::DistributePollUpdateWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Push)
        .retry(0)
        .lock(crate::jobs::Lock::UntilExecuted(
            crate::jobs::DEFAULT_LOCK_TTL,
        ));

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        crate::api::mastodon::polls::federate_poll_update(state, self.status_id).await
    }
}

/// `ThreadResolveWorker`: fetch a reply's unknown parent, link it onto the
/// reply, and run the reply's home fan-out again, so that a reply to an
/// account the viewer follows (whose post was only just learned about)
/// reaches the right followers instead of staying hidden as an orphan.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ThreadResolveWorker {
    pub child_status_id: i64,
    pub parent_url: String,
    #[serde(default)]
    pub request_id: Option<String>,
}

impl crate::jobs::Job for ThreadResolveWorker {
    const KIND: &'static str = "ThreadResolveWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Pull)
        .retry(3);

    fn retry_in(count: u32) -> Option<std::time::Duration> {
        crate::jobs::exponential_backoff(count)
    }

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        // `return if child_status.in_reply_to_id.present?`
        let Some(child) = sqlx::query!(
            "SELECT account_id, in_reply_to_id FROM statuses WHERE id = $1",
            self.child_status_id
        )
        .fetch_optional(&state.db)
        .await?
        else {
            return Ok(());
        };
        if child.in_reply_to_id.is_some() {
            return Ok(());
        }
        let uri = &self.parent_url;
        tracing::debug!(uri, "fetching unknown parent status for thread resolution");
        // `FetchRemoteStatusService.new.call(parent_url, request_id:)`, which
        // gives nothing for a parent it cannot reach.
        super::fetch_remote_status_by_url(state, uri, None, self.request_id.clone())
            .await
            .map_err(|e| anyhow::anyhow!("could not fetch parent {uri}: {e:?}"))?;
        let Some(parent) = sqlx::query!("SELECT id, account_id FROM statuses WHERE uri = $1", uri)
            .fetch_optional(&state.db)
            .await?
        else {
            return Ok(());
        };
        let updated = sqlx::query!(
            "UPDATE statuses SET in_reply_to_id = $2, in_reply_to_account_id = $3, updated_at = now()
             WHERE id = $1 AND in_reply_to_id IS NULL",
            self.child_status_id,
            parent.id,
            parent.account_id,
        )
        .execute(&state.db)
        .await?;
        if updated.rows_affected() > 0 {
            let mut redis = state.redis.clone();
            crate::feed::fanout_new_status(
                &mut redis,
                &state.redis_keys,
                &state.db,
                self.child_status_id,
            )
            .await;
        }
        Ok(())
    }
}
