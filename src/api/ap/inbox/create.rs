//! Inbound `Create` activities: ingesting a remote Note/Article/Question into a
//! local status (with media, mentions, tags, polls and quote/reply linkage),
//! fanning it out to timelines, and the `Create`-carried poll-vote path.

use serde_json::Value;

use crate::{error::AppResult, state::AppState};

use super::attachment::{
    ap_attachment_file_meta, attachment_url, classify_attachment_type, preview_card_link,
};
use super::{
    acquire_create_lock, as_string_vec, delete_arrived_first, fetch_remote_status,
    resolve_or_fetch_remote_account, tag_type_is,
};

pub(super) async fn handle_create(
    state: &AppState,
    _instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let object = match activity.get("object") {
        Some(o) if o.is_object() => o,
        Some(o) if o.is_string() => {
            if let Some(uri) = o.as_str() {
                let _ = fetch_remote_status(state, uri).await?;
            }
            return Ok(());
        }
        _ => return Ok(()),
    };
    let obj_type = object.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if obj_type != "Note" {
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

    // Parse tag array once: mentions, hashtags, emojis.
    // ActivityPub allows "tag" to be either a single object or an array.
    let tags_arr: Vec<Value> = match object.get("tag") {
        Some(Value::Array(arr)) => arr.clone(),
        Some(obj @ Value::Object(_)) => vec![obj.clone()],
        _ => vec![],
    };

    let mention_hrefs: Vec<String> = tags_arr
        .iter()
        .filter(|t| tag_type_is(t, "Mention"))
        .filter_map(|t| t.get("href").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();

    // Collect to/cc from both the activity wrapper and the Note object (Mastodon merges both).
    // Both fields may be a string or an array.
    let audience: Vec<String> = {
        let mut a = as_string_vec(activity.get("to"));
        a.extend(as_string_vec(activity.get("cc")));
        a.extend(as_string_vec(object.get("to")));
        a.extend(as_string_vec(object.get("cc")));
        a.sort_unstable();
        a.dedup();
        a
    };

    // Look up inReplyTo status (id + account_id + whether account is local)
    let in_reply_to_uri = object.get("inReplyTo").and_then(|v| v.as_str());
    let in_reply_to_row = if let Some(uri) = in_reply_to_uri {
        sqlx::query!(
            r#"SELECT s.id, s.account_id, (a.domain IS NULL) AS "is_local!"
               FROM statuses s JOIN accounts a ON a.id = s.account_id
               WHERE s.uri = $1"#,
            uri,
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    let in_reply_to_id = in_reply_to_row.as_ref().map(|r| r.id);
    let in_reply_to_account_id = in_reply_to_row.as_ref().map(|r| r.account_id);
    let in_reply_to_local = in_reply_to_row.as_ref().is_some_and(|r| r.is_local);

    // Mastodon serializes poll votes as Create(Note) where the Note's
    // `inReplyTo` is the poll status and `name` is the selected option. Store
    // these as poll_votes instead of creating a visible status.
    if let (Some(parent_id), Some(choice_name)) = (
        in_reply_to_id,
        object
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty()),
    ) {
        if handle_poll_vote_note(state, account_id, parent_id, choice_name, note_uri).await? {
            return Ok(());
        }
    }

    // Acceptance filter: only process if related to local activity (mirrors Mastodon's
    // related_to_local_activity? / addresses_local_accounts? checks).
    let is_followed_locally = sqlx::query_scalar!(
        r#"SELECT EXISTS(
            SELECT 1 FROM follows f
            JOIN accounts a ON a.id = f.account_id
            WHERE f.target_account_id = $1 AND a.domain IS NULL
        )"#,
        account_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false);

    // Any URI in to/cc (from either the activity or the Note) that is a local account.
    let addresses_local = if !audience.is_empty() {
        sqlx::query_scalar!(
            "SELECT EXISTS(SELECT 1 FROM accounts WHERE uri = ANY($1) AND domain IS NULL)",
            &audience as &[String],
        )
        .fetch_one(&state.db)
        .await?
        .unwrap_or(false)
    } else {
        false
    };

    if !is_followed_locally && !addresses_local && !in_reply_to_local {
        tracing::debug!(
            note_uri,
            "Create(Note): ignoring, not related to local activity"
        );
        return Ok(());
    }

    // Field extraction
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
    let published = object
        .get("published")
        .and_then(|p| p.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc).naive_utc());
    let edited_at = object
        .get("updated")
        .and_then(|p| p.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc).naive_utc());

    // Visibility is determined from the Note object's own to/cc fields.
    let note_to = as_string_vec(object.get("to"));
    let note_cc = as_string_vec(object.get("cc"));
    let visibility = crate::db::models::vis::from_audience(&note_to, &note_cc);

    let language = object
        .get("contentMap")
        .and_then(|m| m.as_object())
        .and_then(|m| m.keys().next())
        .map(|s| s.to_string())
        .filter(|s| ["ko", "en"].contains(&s.as_str()));

    // FEP-044f quote linkage. Resolved after the status is inserted (below) so a
    // quoted post that quotes back can't recurse forever.
    let quote_uri = object
        .get("quote")
        .and_then(|v| v.as_str())
        .or_else(|| object.get("quoteUrl").and_then(|v| v.as_str()))
        .or_else(|| object.get("quoteUri").and_then(|v| v.as_str()))
        .or_else(|| object.get("_misskey_quote").and_then(|v| v.as_str()));

    let status_id = crate::snowflake::next_id();
    let created_at = published.unwrap_or_else(|| chrono::Utc::now().naive_utc());

    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO statuses
             (id, account_id, text, spoiler_text, visibility, sensitive,
              uri, url, in_reply_to_id, in_reply_to_account_id, reply,
              language, local, created_at, edited_at, updated_at, quote_approval_policy)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12, false, $13,$14, now(), $15)
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
    )
    .fetch_optional(&state.db)
    .await?;

    let Some(inserted_id) = inserted else {
        return Ok(()); // duplicate
    };

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

    // Record the FEP-044f quote. Matching Mastodon, fetch the quoted post when
    // it isn't cached locally so the quote serializes instead of being silently
    // dropped; the fetch is bounded by fetch_remote_status's depth limit.
    if let Some(q) = quote_uri {
        let mut quoted: Option<(i64, i64)> = sqlx::query!(
            "SELECT id, account_id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
            q,
        )
        .fetch_optional(&state.db)
        .await?
        .map(|r| (r.id, r.account_id));
        if quoted.is_none() {
            if let Some(qid) = fetch_remote_status(state, q).await? {
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
                inserted_id,
                quoted_id,
                account_id,
                quoted_account_id,
            )
            .execute(&state.db)
            .await;
        }
    }

    // Media attachments. Domains blocked with `reject_media` (or fully
    // suspended) federate text but not media, so skip storing attachments.
    let attachments: Vec<Value> =
        if crate::federation::moderation::actor_media_rejected(state, actor_uri).await {
            Vec::new()
        } else {
            object
                .get("attachment")
                .and_then(|a| a.as_array())
                .cloned()
                .unwrap_or_default()
        };
    let mut media_ids: Vec<i64> = Vec::new();
    for att in &attachments {
        // Mastodon caps a status at MEDIA_ATTACHMENTS_LIMIT (4).
        if media_ids.len() >= 4 {
            break;
        }
        let att_type_str = att.get("type").and_then(|v| v.as_str()).unwrap_or("");
        // `url` may be a string, a Link object (`{href, mediaType}`), or an
        // array of links — Mastodon resolves all of these.
        let Some((remote_url, link_media_type)) = att.get("url").and_then(attachment_url) else {
            continue;
        };
        // mediaType: explicit, else from the chosen Link, else guessed from the
        // URL's extension (matches Mastodon's `mediaType || url_to_media_type`).
        let media_type_str = att
            .get("mediaType")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .or(link_media_type)
            .or_else(|| {
                let path = remote_url.split(['?', '#']).next().unwrap_or(&remote_url);
                mime_guess::from_path(path).first_raw().map(str::to_owned)
            })
            .unwrap_or_default();
        // Classify from mediaType — Mastodon serializes `type: "Document"` for
        // everything — falling back to the AP `type` hint for odd peers.
        let att_type = classify_attachment_type(att_type_str, &media_type_str);
        let description = att.get("name").and_then(|v| v.as_str()).map(str::to_owned);
        let blurhash = att
            .get("blurhash")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let thumbnail_remote_url = att
            .get("icon")
            .and_then(|i| if i.is_object() { i.get("url") } else { None })
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let file_content_type = if media_type_str.is_empty() {
            None
        } else {
            Some(media_type_str.clone())
        };
        let file_meta = ap_attachment_file_meta(att);

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
            remote_url,
            description,
            blurhash,
            att_type,
            thumbnail_remote_url,
            file_content_type,
            file_meta,
        )
        .fetch_one(&state.db)
        .await
        {
            Ok(id) => media_ids.push(id),
            Err(e) => tracing::warn!(error = %e, "failed to insert media attachment"),
        }
    }
    if !media_ids.is_empty() {
        let _ = sqlx::query!(
            "UPDATE statuses SET ordered_media_attachment_ids = $1 WHERE id = $2",
            &media_ids,
            inserted_id,
        )
        .execute(&state.db)
        .await;
    }

    // FEP-8967: a `Link` attachment names the status's preview card outright,
    // which is the only way a remote status gets one here — eunha builds cards
    // for local posts by scanning their content, but does not scrape remote
    // ones. Mastodon 4.7.0 likewise takes the first `Link` it finds.
    if let Some(card_url) = preview_card_link(&attachments).map(str::to_owned) {
        let state = state.clone();
        crate::tenants::spawn(async move {
            let Some(card_id) =
                crate::preview_card::fetch_and_store(&state.db, &state.fetch, &card_url).await
            else {
                return;
            };
            let _ = sqlx::query!(
                "INSERT INTO preview_cards_statuses (status_id, preview_card_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                inserted_id,
                card_id,
            )
            .execute(&state.db)
            .await;
            // `FetchLinkCardService`: `Trends.links.register`.
            crate::trends::register_links(&state, inserted_id).await;
        });
    }

    // Hashtags
    let hashtag_names: Vec<String> = {
        let mut seen = std::collections::HashSet::new();
        tags_arr
            .iter()
            .filter(|t| tag_type_is(t, "Hashtag"))
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
        let tag_id = crate::snowflake::next_id();
        match sqlx::query_scalar!(
            r#"INSERT INTO tags (id, name, last_status_at, created_at, updated_at)
               VALUES ($1, $2, now(), now(), now())
               ON CONFLICT (lower(name)) DO UPDATE SET last_status_at = now(), updated_at = now()
               RETURNING id"#,
            tag_id,
            name,
        )
        .fetch_optional(&state.db)
        .await
        {
            Ok(Some(id)) => {
                tag_ids.push(id);
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

    // Mentions — resolve accounts and notify local ones
    let actor_info = sqlx::query!(
        "SELECT display_name, username, domain, avatar_remote_url FROM accounts WHERE id = $1",
        account_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    // Store every mention before notifying anyone. Mastodon writes the whole
    // set as part of creating the status and notifies afterwards, and the order
    // matters: a mention is dropped when the status also mentions someone the
    // recipient blocks, which cannot be seen while the rest are still unwritten.
    let mut mentioned: Vec<(i64, bool)> = Vec::new();
    for href in &mention_hrefs {
        let mentioned_id = match resolve_or_fetch_remote_account(state, href).await {
            Ok(id) => id,
            Err(_) => continue,
        };
        let _ = sqlx::query!(
            "INSERT INTO mentions (status_id, account_id, created_at, updated_at) VALUES ($1,$2, now(), now()) ON CONFLICT DO NOTHING",
            inserted_id,
            mentioned_id,
        )
        .execute(&state.db)
        .await;

        let is_local = sqlx::query_scalar!(
            r#"SELECT (domain IS NULL) AS "v!" FROM accounts WHERE id = $1"#,
            mentioned_id,
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);
        mentioned.push((mentioned_id, is_local));
    }

    for (mentioned_id, is_local) in mentioned {
        if !is_local {
            continue;
        }
        if let Some(ref info) = actor_info {
            let acct = match &info.domain {
                Some(d) => format!("{}@{}", info.username, d),
                None => info.username.clone(),
            };
            crate::push::create_and_push(
                state,
                mentioned_id,
                account_id,
                "mention",
                Some(inserted_id),
                format!("New mention from {}", info.display_name),
                acct,
                info.avatar_remote_url.clone().unwrap_or_default(),
            )
            .await;
        }
    }

    // Conversation management for direct messages.
    // Mirrors Mastodon: participants = sender + status.active_mentions (explicit Mention tags).
    if visibility == crate::db::models::vis::DIRECT {
        // Reuse the parent status's conversation if this is a reply, otherwise create a new one.
        let conversation_id: i64 = if let Some(parent_id) = in_reply_to_id {
            let parent_conv = sqlx::query_scalar!(
                "SELECT conversation_id FROM statuses WHERE id = $1",
                parent_id,
            )
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .flatten();
            if let Some(cid) = parent_conv {
                cid
            } else {
                sqlx::query_scalar!(
                    "INSERT INTO conversations (created_at, updated_at) VALUES (now(), now()) RETURNING id"
                )
                .fetch_one(&state.db)
                .await?
            }
        } else {
            sqlx::query_scalar!(
                "INSERT INTO conversations (created_at, updated_at) VALUES (now(), now()) RETURNING id"
            )
            .fetch_one(&state.db)
            .await?
        };

        let _ = sqlx::query!(
            "UPDATE statuses SET conversation_id = $1 WHERE id = $2",
            conversation_id,
            inserted_id,
        )
        .execute(&state.db)
        .await;

        // Participants = sender + explicitly mentioned accounts (mirrors Mastodon's active_mentions).
        let mentioned_local_ids: Vec<i64> = sqlx::query_scalar!(
            r#"SELECT m.account_id FROM mentions m
               JOIN accounts a ON a.id = m.account_id
               WHERE m.status_id = $1 AND a.domain IS NULL"#,
            inserted_id,
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();

        let mut all_participant_ids: Vec<i64> = std::iter::once(account_id)
            .chain(mentioned_local_ids.iter().copied())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        all_participant_ids.sort_unstable();

        // For each local recipient, upsert account_conversations.
        // participant_account_ids = everyone else in the conversation (not this recipient).
        for &local_id in &mentioned_local_ids {
            let mut others: Vec<i64> = all_participant_ids
                .iter()
                .copied()
                .filter(|&id| id != local_id)
                .collect();
            others.sort_unstable();
            let _ = sqlx::query!(
                r#"INSERT INTO account_conversations
                     (account_id, conversation_id, participant_account_ids, status_ids, last_status_id, unread)
                   VALUES ($1, $2, $3, ARRAY[$4::bigint], $4, true)
                   ON CONFLICT (account_id, conversation_id, participant_account_ids) DO UPDATE
                     SET status_ids = array_append(account_conversations.status_ids, $4),
                         last_status_id = $4,
                         unread = true"#,
                local_id,
                conversation_id,
                &others,
                inserted_id,
            )
            .execute(&state.db)
            .await;
        }
    }

    // Custom emojis
    let actor_domain = url::Url::parse(actor_uri)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned));
    for tag in tags_arr.iter().filter(|t| tag_type_is(t, "Emoji")) {
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

    // Poll
    let poll_items = object.get("oneOf").or_else(|| object.get("anyOf"));
    if let Some(items) = poll_items.and_then(|v| v.as_array()) {
        let multiple = object.get("anyOf").is_some();
        let options: Vec<String> = items
            .iter()
            .filter_map(|item| item.get("name").and_then(|v| v.as_str()).map(str::to_owned))
            .collect();
        let cached_tallies: Vec<i64> = items
            .iter()
            .map(|item| {
                item.get("replies")
                    .and_then(|r| r.get("totalItems"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0)
            })
            .collect();
        let votes_count: i64 = cached_tallies.iter().sum();
        let expires_at = object
            .get("endTime")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc).naive_utc());
        let poll_id = crate::snowflake::next_id();
        if let Ok(Some(_)) = sqlx::query_scalar!(
            r#"INSERT INTO polls
                 (id, status_id, account_id, options, cached_tallies, votes_count,
                  multiple, expires_at, created_at, updated_at)
               SELECT $1,$2,$3,$4,$5,$6,$7,$8,now(),now()
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

    // Thread resolution: store the unknown parent if it is dereferenceable.
    if let (Some(uri), None) = (in_reply_to_uri, in_reply_to_id) {
        let state = state.clone();
        let uri = uri.to_owned();
        let child_id = inserted_id;
        let child_author = account_id;
        crate::tenants::spawn(async move {
            tracing::debug!(uri, "fetching unknown parent status for thread resolution");
            if let Err(e) = fetch_remote_status(&state, &uri).await {
                tracing::debug!(uri, error = %e, "failed to store fetched parent status");
                return;
            }
            // Link the now-known parent onto the child and re-run home fan-out, so
            // a reply to an account the viewer follows (whose post we only just
            // learned about) reaches the right followers instead of staying hidden
            // as an orphan reply.
            if let Ok(Some(parent)) =
                sqlx::query!("SELECT id, account_id FROM statuses WHERE uri = $1", uri,)
                    .fetch_optional(&state.db)
                    .await
            {
                let updated = sqlx::query!(
                    "UPDATE statuses SET in_reply_to_id = $2, in_reply_to_account_id = $3, updated_at = now() WHERE id = $1 AND in_reply_to_id IS NULL",
                    child_id, parent.id, parent.account_id,
                )
                .execute(&state.db)
                .await;
                if updated.map(|r| r.rows_affected() > 0).unwrap_or(false) {
                    let mut redis = state.redis.clone();
                    let db = state.db.clone();
                    crate::feed::fanout_new_status(
                        &mut redis,
                        &state.redis_keys,
                        &db,
                        child_author,
                        child_id,
                        &[],
                    )
                    .await;
                }
            }
        });
    }

    // Fanout to home and list feeds
    let vis_str = crate::db::models::vis::to_str(visibility);
    let mut redis = state.redis.clone();
    let redis_keys = state.redis_keys.clone();
    let db = state.db.clone();
    if crate::feed::sync_fanout() {
        crate::feed::fanout_new_status(
            &mut redis,
            &redis_keys,
            &db,
            account_id,
            inserted_id,
            &tag_ids,
        )
        .await;
        crate::feed::fanout_to_lists(
            &mut redis,
            &redis_keys,
            &db,
            account_id,
            inserted_id,
            in_reply_to_account_id,
            vis_str,
        )
        .await;
    } else {
        crate::tenants::spawn(async move {
            crate::feed::fanout_new_status(
                &mut redis,
                &redis_keys,
                &db,
                account_id,
                inserted_id,
                &tag_ids,
            )
            .await;
            crate::feed::fanout_to_lists(
                &mut redis,
                &redis_keys,
                &db,
                account_id,
                inserted_id,
                in_reply_to_account_id,
                vis_str,
            )
            .await;
        });
    }

    Ok(())
}

pub(super) async fn handle_poll_vote_note(
    state: &AppState,
    voter_id: i64,
    status_id: i64,
    choice_name: &str,
    vote_uri: &str,
) -> AppResult<bool> {
    let Some(poll) = sqlx::query!(
        "SELECT id, options, multiple, expires_at FROM polls WHERE status_id = $1",
        status_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(false);
    };

    if poll
        .expires_at
        .map(|e| e < chrono::Utc::now().naive_utc())
        .unwrap_or(false)
    {
        return Ok(true);
    }

    let Some(choice) = poll.options.iter().position(|option| option == choice_name) else {
        return Ok(true);
    };
    let choice = choice as i32;

    let already_voted = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM poll_votes WHERE poll_id = $1 AND account_id = $2)",
        poll.id,
        voter_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false);

    if !poll.multiple && already_voted {
        return Ok(true);
    }

    sqlx::query!(
        r#"INSERT INTO poll_votes (account_id, poll_id, choice, uri, created_at, updated_at)
           VALUES ($1, $2, $3, $4, now(), now())
           ON CONFLICT DO NOTHING"#,
        voter_id,
        poll.id,
        choice,
        vote_uri,
    )
    .execute(&state.db)
    .await?;

    sqlx::query!(
        "UPDATE polls SET votes_count = (SELECT COUNT(*) FROM poll_votes WHERE poll_id = $1), updated_at = now() WHERE id = $1",
        poll.id,
    )
    .execute(&state.db)
    .await?;

    if poll.multiple && !already_voted {
        sqlx::query!(
            "UPDATE polls SET voters_count = COALESCE(voters_count, 0) + 1, updated_at = now() WHERE id = $1",
            poll.id,
        )
        .execute(&state.db)
        .await?;
    }

    Ok(true)
}
