//! Inbound status-lifecycle activities: `Delete` (tombstone a remote status),
//! `Announce` (boost), `Like` (favourite), and `Update` (edit a remote status,
//! sync its poll). Includes notify_status_author, shared by Announce and Like.

use serde_json::Value;

use crate::{error::AppResult, state::AppState};

use super::attachment::{ap_attachment_file_meta, classify_attachment_type};
use super::{
    acquire_create_lock, as_string_vec, delete_arrived_first, delete_later, fetch_remote_status,
    mirror_item_into, refresh_collection_item_count, resolve_or_fetch_remote_account, same_host,
    sync_remote_poll, tag_type_is, upsert_remote_collection,
};

pub(super) async fn handle_delete(
    state: &AppState,
    _instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let object_uri = activity.get("object").and_then(|o| {
        if o.is_string() {
            o.as_str()
        } else {
            o.get("id").and_then(|i| i.as_str())
        }
    });

    if let Some(uri) = object_uri {
        // Delete(actor) — remote account deleted itself. Mastodon's
        // `ActivityPub::Activity::Delete#delete_person`: purge it outright,
        // without announcing anything back over ActivityPub.
        if uri == actor_uri {
            let account_id = sqlx::query_scalar!(
                "SELECT id FROM accounts WHERE uri = $1 AND domain IS NOT NULL",
                uri,
            )
            .fetch_optional(&state.db)
            .await?;
            if let Some(account_id) = account_id {
                crate::delete_account::call(
                    state,
                    account_id,
                    crate::delete_account::Options {
                        reserve_username: false,
                        skip_activitypub: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(crate::error::AppError::Internal)?;
                tracing::debug!(actor_uri, "purged remote account on Delete(actor)");
            }
        } else {
            // Delete(FeatureAuthorization) — a featured account revoked consent;
            // revoke the matching item (matched by the authorization URI we stored).
            let revoked = sqlx::query!(
                r#"UPDATE collection_items SET state = 3, updated_at = now()
                   WHERE approval_uri = $1 AND state = 1
                   RETURNING collection_id"#,
                uri,
            )
            .fetch_optional(&state.db)
            .await?;
            if let Some(r) = revoked {
                refresh_collection_item_count(state, r.collection_id).await?;
                return Ok(());
            }

            // Reject if the actor's domain doesn't match the object's domain —
            // prevents one server from deleting another server's content.
            if !same_host(actor_uri, uri) {
                tracing::warn!(
                    actor_uri,
                    uri,
                    "Delete: actor domain does not match object domain, ignoring"
                );
                return Ok(());
            }

            // Delete(Note/Tombstone) — soft-delete the status. Serialize against
            // a concurrent Create for this uri (same `create:{uri}` lock) so we
            // observe its committed status and it observes our tombstone.
            let _create_lock = acquire_create_lock(state, uri).await;
            // Read what is about to be deleted, so the parent's reply count can
            // be put back. Only a reply that was counted is subtracted, matching
            // what `Create` counted on the way in.
            let deleted_reply = sqlx::query!(
                r#"SELECT account_id, in_reply_to_id, visibility FROM statuses
                   WHERE uri = $1 AND deleted_at IS NULL"#,
                uri,
            )
            .fetch_optional(&state.db)
            .await?;
            let deleted =
                sqlx::query!("UPDATE statuses SET deleted_at = now() WHERE uri = $1", uri,)
                    .execute(&state.db)
                    .await?;
            if let Some(row) = &deleted_reply {
                if let Err(e) = crate::counters::on_status_deleted(
                    &state.db,
                    row.account_id,
                    row.visibility,
                    row.in_reply_to_id,
                )
                .await
                {
                    tracing::error!(error = %e, "failed to uncount a deleted federated status");
                }
            }
            // If the status isn't known yet (out-of-order delivery), remember the
            // Delete so a late Create with this URI is skipped.
            if deleted.rows_affected() == 0 {
                delete_later(state, actor_uri, uri).await;
            }

            // Create a tombstone so that a subsequent Create with the same URI is rejected.
            let actor_id =
                sqlx::query_scalar!("SELECT id FROM accounts WHERE uri = $1", actor_uri,)
                    .fetch_optional(&state.db)
                    .await?;
            if let Some(actor_id) = actor_id {
                let tombstone_id = crate::snowflake::next_id();
                let _ = sqlx::query!(
                    r#"INSERT INTO tombstones (id, account_id, uri, created_at, updated_at)
                       SELECT $1, $2, $3::text, now(), now()
                       WHERE NOT EXISTS (SELECT 1 FROM tombstones WHERE uri = $3::text)"#,
                    tombstone_id,
                    actor_id,
                    uri,
                )
                .execute(&state.db)
                .await;
            }
        }
    }

    Ok(())
}

pub(super) async fn handle_announce(
    state: &AppState,
    _instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let object = activity.get("object");
    let announce_uri = activity.get("id").and_then(|i| i.as_str()).unwrap_or("");

    // Skip an Announce whose Undo already arrived out of order.
    if delete_arrived_first(state, actor_uri, announce_uri).await {
        return Ok(());
    }

    // object can be a URI string or an embedded object
    let boosted_uri = object.and_then(|o| {
        if o.is_string() {
            o.as_str()
        } else {
            o.get("id").and_then(|i| i.as_str())
        }
    });

    let Some(boosted_uri) = boosted_uri else {
        return Ok(());
    };
    if actor_uri.is_empty() || announce_uri.is_empty() {
        return Ok(());
    }

    let booster_id = match resolve_or_fetch_remote_account(state, actor_uri).await {
        Ok(id) => id,
        Err(_) => return Ok(()),
    };

    // Find the boosted status in our database, fetching URI-only boosted
    // objects on demand like Mastodon's dereferencer path.
    let mut original_id = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
        boosted_uri,
    )
    .fetch_optional(&state.db)
    .await?;

    if original_id.is_none() {
        original_id = fetch_remote_status(state, boosted_uri).await?;
    }

    let Some(mut original_id) = original_id else {
        return Ok(());
    };
    if let Some(unwrapped_id) = sqlx::query_scalar!(
        "SELECT reblog_of_id FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        original_id,
    )
    .fetch_optional(&state.db)
    .await?
    .flatten()
    {
        original_id = unwrapped_id;
    }

    let published = activity
        .get("published")
        .and_then(|p| p.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc).naive_utc())
        .unwrap_or_else(|| chrono::Utc::now().naive_utc());

    // Derive the boost's visibility from the Announce's own `to`/`cc` audience,
    // mirroring Mastodon's ActivityPub::Activity::Announce#visibility_from_audience
    // (public collection in `to` → public, in `cc` → unlisted, a followers
    // collection → private, otherwise direct) instead of assuming public.
    let announce_to = as_string_vec(activity.get("to"));
    let announce_cc = as_string_vec(activity.get("cc"));
    let visibility = crate::db::models::vis::from_audience(&announce_to, &announce_cc);

    let boost_id = crate::snowflake::next_id();
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO statuses
             (id, account_id, reblog_of_id, visibility, uri, url, local, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $5, false, $6, now())
           ON CONFLICT (uri) WHERE uri IS NOT NULL AND uri != '' DO NOTHING
           RETURNING id"#,
        boost_id,
        booster_id,
        original_id,
        visibility,
        announce_uri,
        published,
    )
    .fetch_optional(&state.db)
    .await?;

    // A boost is a status of the booster's. `inserted` is `None` when the
    // announce was already recorded, which is how a redelivery counts once.
    if inserted.is_some() {
        if let Err(e) =
            crate::counters::on_status_created(&state.db, booster_id, visibility, None, published)
                .await
        {
            tracing::error!(booster_id, error = %e, "failed to count a federated boost");
        }
    }

    // Update the original status's reblogs_count
    let _ = sqlx::query!(
        r#"INSERT INTO status_stats (status_id, reblogs_count, created_at, updated_at)
           VALUES ($1, 1, now(), now())
           ON CONFLICT (status_id) DO UPDATE
             SET reblogs_count = (SELECT COUNT(*) FROM statuses
                                  WHERE reblog_of_id = $1 AND deleted_at IS NULL),
                 updated_at = now()"#,
        original_id,
    )
    .execute(&state.db)
    .await;

    // Notify the local author that a remote account boosted their post
    // (Mastodon notifies via LocalNotificationWorker on an incoming Announce).
    notify_status_author(
        state,
        original_id,
        booster_id,
        "reblog",
        "boosted your post",
    )
    .await;

    // Fan the boost into followers' home and list feeds so it appears
    // immediately, not only after a feed repopulate. Mirrors the local reblog
    // path (mastodon::statuses::reblog_status) and the incoming-post path
    // (handle_create). Skipped when the Announce was a duplicate (no row
    // inserted) so we never push a non-existent status id, and — like
    // Mastodon's ActivityPub::Activity::Announce#distribute, which only
    // enqueues DistributionWorker when the reblog is within_realtime_window? —
    // skipped for boosts older than the 6h real-time window so backfilled
    // announces don't resurface at the top of feeds.
    let within_realtime_window =
        chrono::Utc::now().naive_utc() - published < chrono::Duration::hours(6);
    if let (Some(boost_id), true) = (inserted, within_realtime_window) {
        let mut redis = state.redis.clone();
        let redis_keys = state.redis_keys.clone();
        let db = state.db.clone();
        let vis_str = crate::db::models::vis::to_str(visibility);
        if crate::feed::sync_fanout() {
            crate::feed::fanout_new_status(&mut redis, &redis_keys, &db, booster_id, boost_id, &[])
                .await;
            crate::feed::fanout_to_lists(
                &mut redis,
                &redis_keys,
                &db,
                booster_id,
                boost_id,
                None,
                vis_str,
            )
            .await;
        } else {
            crate::tenants::spawn(async move {
                crate::feed::fanout_new_status(
                    &mut redis,
                    &redis_keys,
                    &db,
                    booster_id,
                    boost_id,
                    &[],
                )
                .await;
                crate::feed::fanout_to_lists(
                    &mut redis,
                    &redis_keys,
                    &db,
                    booster_id,
                    boost_id,
                    None,
                    vis_str,
                )
                .await;
            });
        }
    }

    Ok(())
}

pub(super) async fn handle_like(
    state: &AppState,
    _instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let activity_uri = activity.get("id").and_then(|i| i.as_str()).unwrap_or("");
    let object_uri = activity
        .get("object")
        .and_then(|o| o.as_str())
        .unwrap_or("");

    // Skip a Like whose Undo already arrived out of order.
    if delete_arrived_first(state, actor_uri, activity_uri).await {
        return Ok(());
    }

    let mut status_id = sqlx::query_scalar!("SELECT id FROM statuses WHERE uri = $1", object_uri)
        .fetch_optional(&state.db)
        .await?;

    if status_id.is_none() {
        status_id = fetch_remote_status(state, object_uri).await?;
    }

    let Some(status_id) = status_id else {
        return Ok(());
    };

    let account_id = match resolve_or_fetch_remote_account(state, actor_uri).await {
        Ok(id) => id,
        Err(_) => return Ok(()),
    };

    sqlx::query!(
        "INSERT INTO favourites (account_id, status_id, created_at, updated_at) VALUES ($1,$2, now(), now()) ON CONFLICT DO NOTHING",
        account_id,
        status_id
    )
    .execute(&state.db)
    .await?;

    sqlx::query!(
        r#"INSERT INTO status_stats (status_id, favourites_count, created_at, updated_at)
           VALUES ($1, (SELECT COUNT(*) FROM favourites WHERE status_id = $1), now(), now())
           ON CONFLICT (status_id) DO UPDATE
             SET favourites_count = (SELECT COUNT(*) FROM favourites WHERE status_id = $1),
                 updated_at = now()"#,
        status_id
    )
    .execute(&state.db)
    .await?;

    // Notify the local author that a remote account favourited their post
    // (Mastodon notifies the author via LocalNotificationWorker on an incoming
    // Like). create_and_push no-ops for a remote recipient and dedups.
    notify_status_author(
        state,
        status_id,
        account_id,
        "favourite",
        "favourited your post",
    )
    .await;

    Ok(())
}

/// Notify a status's author that `actor_id` interacted with it (favourite or
/// reblog from a remote account). No-ops if the author is remote.
async fn notify_status_author(
    state: &AppState,
    status_id: i64,
    actor_id: i64,
    notification_type: &'static str,
    verb: &str,
) {
    let Ok(Some(author_id)) = sqlx::query_scalar!(
        "SELECT account_id FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        status_id,
    )
    .fetch_optional(&state.db)
    .await
    else {
        return;
    };
    let Ok(Some(actor)) = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        actor_id,
    )
    .fetch_optional(&state.db)
    .await
    else {
        return;
    };
    crate::push::create_and_push(
        state,
        author_id,
        actor_id,
        notification_type,
        Some(status_id),
        format!("{} {}", actor.display_name, verb),
        actor.acct(),
        crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &actor),
    )
    .await;
}

pub(super) async fn handle_update(
    state: &AppState,
    _instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let fetched_object;
    let object = match activity.get("object") {
        Some(o) if o.is_object() => o,
        Some(o) if o.is_string() => {
            let Some(uri) = o.as_str() else {
                return Ok(());
            };
            fetched_object = match crate::federation::fetch::signed_get_json(state, uri).await {
                Ok(v) => v,
                Err(_) => return Ok(()),
            };
            &fetched_object
        }
        _ => return Ok(()),
    };

    let obj_type = object.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match obj_type {
        "FeaturedCollection" => {
            // Mirror an updated remote collection.
            let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
            if actor_uri.is_empty() {
                return Ok(());
            }
            if let Ok(owner_id) = resolve_or_fetch_remote_account(state, actor_uri).await {
                if let Some(cid) = upsert_remote_collection(state, owner_id, object).await? {
                    if let Some(items) = object.get("orderedItems").and_then(|v| v.as_array()) {
                        for it in items {
                            let _ = mirror_item_into(state, cid, it).await;
                        }
                    }
                }
            }
        }
        "Person" | "Service" | "Application" | "Group" | "Organization" => {
            let actor_uri = object.get("id").and_then(|i| i.as_str()).unwrap_or("");
            if actor_uri.is_empty() {
                return Ok(());
            }
            // An actor updates itself and nobody else. Without this, any server
            // could rewrite any account's profile and public key, and then sign
            // as that account.
            if Some(actor_uri) != activity.get("actor").and_then(|a| a.as_str()) {
                tracing::warn!(
                    object = actor_uri,
                    "refused an Update of an actor other than its sender"
                );
                return Ok(());
            }

            let display_name = object
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let note = object
                .get("summary")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            let inbox_url = object
                .get("inbox")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            let shared_inbox_url = object
                .get("endpoints")
                .and_then(|e| e.get("sharedInbox"))
                .and_then(|s| s.as_str())
                .map(str::to_owned);
            let public_key = object
                .get("publicKey")
                .and_then(|k| k.get("publicKeyPem"))
                .and_then(|p| p.as_str())
                .unwrap_or("")
                .to_string();
            let locked = object
                .get("manuallyApprovesFollowers")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let avatar_remote_url = object
                .get("icon")
                .and_then(|i| if i.is_object() { i.get("url") } else { None })
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let header_remote_url = object
                .get("image")
                .and_then(|i| if i.is_object() { i.get("url") } else { None })
                .and_then(|v| v.as_str())
                .map(str::to_owned);

            // `set_suspension!`: follow the actor's `suspended` flag, and while
            // the account is suspended take only its protocol attributes
            // (`set_immediate_attributes! unless @account.suspended?`).
            if let Some(existing) = sqlx::query_as!(
                crate::db::models::Account,
                "SELECT * FROM accounts WHERE uri = $1 AND domain IS NOT NULL",
                actor_uri,
            )
            .fetch_optional(&state.db)
            .await?
            {
                let flagged = object
                    .get("suspended")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let suspended =
                    crate::moderation::remote::set_suspension(state, &existing, flagged)
                        .await
                        .map_err(crate::error::AppError::Internal)?;
                if suspended {
                    // `set_fetchable_key! unless suspended locally`.
                    let local_suspension = existing.suspension_origin
                        != Some(crate::delete_account::suspension_origin::REMOTE);
                    sqlx::query!(
                        r#"UPDATE accounts
                           SET inbox_url = CASE WHEN $2 != '' THEN $2 ELSE inbox_url END,
                               shared_inbox_url = COALESCE($3, shared_inbox_url),
                               public_key = CASE WHEN $4 != '' AND NOT $5 THEN $4 ELSE public_key END,
                               updated_at = now()
                           WHERE id = $1"#,
                        existing.id,
                        inbox_url,
                        shared_inbox_url,
                        public_key,
                        local_suspension,
                    )
                    .execute(&state.db)
                    .await?;
                    return Ok(());
                }
            }

            // Don't clear inbox_url or public_key if the update omits them (sparse update guard)
            sqlx::query!(
                r#"UPDATE accounts
                   SET display_name = $2,
                       note = $3,
                       inbox_url = CASE WHEN $4 != '' THEN $4 ELSE inbox_url END,
                       shared_inbox_url = COALESCE($5, shared_inbox_url),
                       public_key = CASE WHEN $6 != '' THEN $6 ELSE public_key END,
                       locked = $7,
                       avatar_remote_url = COALESCE($8, avatar_remote_url),
                       header_remote_url = COALESCE($9, header_remote_url),
                       updated_at = now()
                   WHERE uri = $1 AND domain IS NOT NULL"#,
                actor_uri,
                display_name,
                note,
                inbox_url,
                shared_inbox_url,
                public_key,
                locked,
                avatar_remote_url,
                header_remote_url,
            )
            .execute(&state.db)
            .await?;

            // An actor that renamed itself carries the new handle here.
            let claimed_username = object
                .get("preferredUsername")
                .and_then(|u| u.as_str())
                .unwrap_or_default();
            crate::federation::handle::rename_if_handle_changed(state, actor_uri, claimed_username)
                .await?;
        }
        "Note" => {
            let note_uri = object.get("id").and_then(|i| i.as_str()).unwrap_or("");
            if note_uri.is_empty() {
                return Ok(());
            }

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
            let sensitive = object
                .get("sensitive")
                .and_then(|s| s.as_bool())
                .unwrap_or(false);
            let language = object
                .get("contentMap")
                .and_then(|m| m.as_object())
                .and_then(|m| m.keys().next())
                .map(|s| s.to_string())
                .filter(|s| ["ko", "en"].contains(&s.as_str()));
            let edited_at = object
                .get("updated")
                .and_then(|p| p.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&chrono::Utc).naive_utc());

            let updated = sqlx::query!(
                r#"UPDATE statuses
                   SET text = $2, spoiler_text = $3, sensitive = $4, language = $5,
                       edited_at = COALESCE($6, edited_at), updated_at = now()
                   WHERE uri = $1 AND deleted_at IS NULL
                     -- Only the sender's own status: any server could
                     -- otherwise rewrite any status it named.
                     AND account_id = (
                         SELECT id FROM accounts WHERE uri = $7 AND domain IS NOT NULL
                     )
                   RETURNING id, account_id"#,
                note_uri,
                text,
                spoiler_text,
                sensitive,
                language,
                edited_at,
                activity
                    .get("actor")
                    .and_then(|a| a.as_str())
                    .unwrap_or_default(),
            )
            .fetch_optional(&state.db)
            .await?;

            if updated.is_none() {
                let _ = fetch_remote_status(state, note_uri).await?;
                return Ok(());
            }

            let Some(row) = updated else {
                return Ok(());
            };

            // Replace media attachments
            sqlx::query!("DELETE FROM media_attachments WHERE status_id = $1", row.id)
                .execute(&state.db)
                .await?;
            let attachments: Vec<Value> = object
                .get("attachment")
                .and_then(|a| a.as_array())
                .cloned()
                .unwrap_or_default();
            let mut media_ids: Vec<i64> = Vec::new();
            for att in &attachments {
                let att_type_str = att.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let media_type_str = att.get("mediaType").and_then(|v| v.as_str()).unwrap_or("");
                let att_type = classify_attachment_type(att_type_str, media_type_str);
                let remote_url = match att.get("url").and_then(|v| v.as_str()) {
                    Some(u) if !u.is_empty() => u,
                    _ => continue,
                };
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
                    Some(media_type_str.to_owned())
                };
                let file_meta = ap_attachment_file_meta(att);
                let media_id = crate::snowflake::next_id();
                if let Ok(id) = sqlx::query_scalar!(
                    r#"INSERT INTO media_attachments (id, account_id, status_id, remote_url, description, blurhash, type, thumbnail_remote_url, file_content_type, file_meta, created_at, updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10, now(), now()) RETURNING id"#,
                    media_id, row.account_id, row.id, remote_url, description, blurhash, att_type, thumbnail_remote_url, file_content_type, file_meta,
                ).fetch_one(&state.db).await { media_ids.push(id); }
            }
            if !media_ids.is_empty() {
                let _ = sqlx::query!(
                    "UPDATE statuses SET ordered_media_attachment_ids = $1 WHERE id = $2",
                    &media_ids,
                    row.id
                )
                .execute(&state.db)
                .await;
            }

            // Replace hashtags
            sqlx::query!("DELETE FROM statuses_tags WHERE status_id = $1", row.id)
                .execute(&state.db)
                .await?;
            let tags_arr: Vec<Value> = match object.get("tag") {
                Some(Value::Array(arr)) => arr.clone(),
                Some(obj @ Value::Object(_)) => vec![obj.clone()],
                _ => vec![],
            };
            for tag in tags_arr.iter().filter(|t| tag_type_is(t, "Hashtag")) {
                let name = match tag
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|n| n.trim_start_matches('#').to_lowercase())
                    .filter(|n| !n.is_empty())
                {
                    Some(n) => n,
                    None => continue,
                };
                let tag_id = crate::snowflake::next_id();
                if let Ok(Some(tid)) = sqlx::query_scalar!(
                    r#"INSERT INTO tags (id, name, last_status_at, created_at, updated_at) VALUES ($1,$2,now(),now(),now()) ON CONFLICT (lower(name)) DO UPDATE SET last_status_at = now(), updated_at = now() RETURNING id"#,
                    tag_id, name,
                ).fetch_optional(&state.db).await {
                    let _ = sqlx::query!("INSERT INTO statuses_tags (status_id, tag_id) VALUES ($1,$2) ON CONFLICT DO NOTHING", row.id, tid)
                        .execute(&state.db).await;
                }
            }

            sync_remote_poll(state, row.id, row.account_id, object).await?;
        }
        _ => {}
    }

    Ok(())
}
