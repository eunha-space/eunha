//! Inbound status-lifecycle activities: `Delete` (tombstone a remote status),
//! `Announce` (boost), `Like` (favourite), and `Update` (edit a remote status,
//! sync its poll). Includes notify_status_author, shared by Announce and Like.

use serde_json::Value;

use crate::{error::AppResult, state::AppState};

use super::attachment::{ap_attachment_file_meta, classify_attachment_type, preview_card_link};
use super::{
    acquire_create_lock, delete_arrived_first, delete_later, fetch_remote_status, mirror_item_into,
    refresh_collection_item_count, resolve_or_fetch_remote_account, same_host, sync_remote_poll,
    upsert_remote_collection,
};
use ojak_vocab::json_ld_helper::{ids, type_is};

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

            // `case @object['type']`: a `QuoteAuthorization` is a stamp taken
            // back (`revoke_quote`), a `Note` or `Question` a status, and
            // anything else whichever of the two it turns out to be.
            let object_type = activity
                .get("object")
                .and_then(|o| o.get("type"))
                .and_then(|t| t.as_str());
            let may_be_stamp = !matches!(object_type, Some("Note" | "Question"));
            if object_type == Some("QuoteAuthorization") {
                if same_host(actor_uri, uri) {
                    delete_later(state, actor_uri, uri).await;
                }
                super::quote::revoke_by_stamp(state, activity, actor_uri, uri).await?;
                return Ok(());
            }

            // Reject if the actor's domain doesn't match the object's domain —
            // prevents one server from deleting another server's content.
            if !same_host(actor_uri, uri) {
                if may_be_stamp
                    && super::quote::revoke_by_stamp(state, activity, actor_uri, uri).await?
                {
                    return Ok(());
                }
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
                r#"SELECT id, reblog_of_id, account_id, in_reply_to_id, visibility FROM statuses
                   WHERE uri = $1 AND deleted_at IS NULL"#,
                uri,
            )
            .fetch_optional(&state.db)
            .await?;
            // `forwarder.forward! if forwarder.forwardable?`, before the
            // status goes, to the followers of the local accounts that shared
            // it.
            if let Some(row) = &deleted_reply {
                let sender: Option<i64> = sqlx::query_scalar!(
                    "SELECT id FROM accounts WHERE uri = $1 AND domain IS NOT NULL",
                    actor_uri,
                )
                .fetch_optional(&state.db)
                .await?;
                if sender == Some(row.account_id)
                    && crate::federation::forwarder::forwardable(state, activity, row.id).await
                {
                    crate::federation::forwarder::forward(state, row.account_id, activity, row.id)
                        .await;
                }
            }
            let deleted =
                sqlx::query!("UPDATE statuses SET deleted_at = now() WHERE uri = $1", uri,)
                    .execute(&state.db)
                    .await?;
            if let Some(row) = &deleted_reply {
                // `RemoveStatusService`.
                crate::streaming::fan_out::remove(state, row.id).await;
                crate::fasp::events::status_deleted(state, row.id).await;
                // `RemoveStatusService`: the quote the status made.
                crate::quotes::status_removed(state, row.id).await;
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
                crate::search::elasticsearch::indexing::status(
                    state,
                    row.reblog_of_id.unwrap_or(row.id),
                )
                .await;
                crate::search::elasticsearch::indexing::account(state, row.account_id).await;
            }
            // If the status isn't known yet (out-of-order delivery), remember the
            // Delete so a late Create with this URI is skipped.
            if deleted.rows_affected() == 0 {
                delete_later(state, actor_uri, uri).await;
                // `delete_status || revoke_quote`
                if may_be_stamp {
                    super::quote::revoke_by_stamp(state, activity, actor_uri, uri).await?;
                }
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

/// `RemoveStatusService` for a remote status whose server says it is gone:
/// it is deleted as a `Delete` from its author deletes it.
pub(super) async fn remove_remote_status(state: &AppState, status_id: i64) -> AppResult<()> {
    let Some(row) = sqlx::query!(
        r#"UPDATE statuses SET deleted_at = now()
           WHERE id = $1 AND deleted_at IS NULL
           RETURNING reblog_of_id, account_id, in_reply_to_id, visibility"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    crate::streaming::fan_out::remove(state, status_id).await;
    crate::fasp::events::status_deleted(state, status_id).await;
    crate::quotes::status_removed(state, status_id).await;
    if let Err(e) = crate::counters::on_status_deleted(
        &state.db,
        row.account_id,
        row.visibility,
        row.in_reply_to_id,
    )
    .await
    {
        tracing::error!(error = %e, "failed to uncount a removed status");
    }
    crate::search::elasticsearch::indexing::status(state, row.reblog_of_id.unwrap_or(status_id))
        .await;
    crate::search::elasticsearch::indexing::account(state, row.account_id).await;
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

    // `Announce#requested_through_relay?`: relayed to us by an enabled
    // relay, or sent by one.
    let booster_inbox: Option<String> =
        sqlx::query_scalar!("SELECT inbox_url FROM accounts WHERE id = $1", booster_id)
            .fetch_optional(&state.db)
            .await?;
    let requested_through_relay = activity
        .get(super::THROUGH_RELAY)
        .is_some_and(|flag| flag == &Value::Bool(true))
        || crate::relays::is_enabled_relay_inbox(state, booster_inbox.as_deref().unwrap_or(""))
            .await;

    // Find the boosted status in our database.
    let mut original_id = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
        boosted_uri,
    )
    .fetch_optional(&state.db)
    .await?;

    // `Announce#related_to_local_activity?`: the booster has followers here,
    // came through a relay, or boosted a local post.
    let followed_by_local_accounts = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM follows WHERE target_account_id = $1) AS "e!""#,
        booster_id,
    )
    .fetch_one(&state.db)
    .await?;
    let reblog_of_local_status = match original_id {
        Some(id) => sqlx::query_scalar!(
            r#"SELECT (a.domain IS NULL) AS "local!" FROM statuses s
               JOIN accounts a ON a.id = s.account_id WHERE s.id = $1"#,
            id,
        )
        .fetch_optional(&state.db)
        .await?
        .unwrap_or(false),
        None => false,
    };
    if !(followed_by_local_accounts || requested_through_relay || reblog_of_local_status) {
        tracing::debug!(announce_uri, "Announce: not related to local activity");
        return Ok(());
    }

    // `status_from_object`: an embedded self-boost is taken as the `Create`
    // it is; anything else is fetched, on behalf of a local follower of the
    // booster (`fetch_remote_original_status`).
    if original_id.is_none() {
        let embedded_self_boost = object
            .filter(|o| o.is_object() && super::status_parser::is_status_type(o))
            .filter(|o| {
                let attributed = match o.get("attributedTo") {
                    Some(Value::Array(items)) => items.first(),
                    other => other,
                };
                attributed
                    .and_then(crate::federation::json_ld::value_or_id)
                    .is_some_and(|a| a == actor_uri)
            });
        if let Some(embedded) = embedded_self_boost {
            let virtual_create = serde_json::json!({
                "type": "Create",
                "actor": actor_uri,
                "object": embedded,
            });
            Box::pin(super::create::create(
                state,
                &virtual_create,
                &super::create::CreateOptions {
                    fetched: true,
                    ..Default::default()
                },
            ))
            .await?;
            original_id = sqlx::query_scalar!(
                "SELECT id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
                boosted_uri,
            )
            .fetch_optional(&state.db)
            .await?;
        } else if boosted_uri.starts_with("http") {
            if crate::federation::local_uri::is_local(state, boosted_uri) {
                return Ok(());
            }
            original_id = super::fetch_remote_status_with(
                state,
                boosted_uri,
                super::FetchOptions {
                    on_behalf_of: crate::federation::json_ld::local_follower(state, booster_id)
                        .await,
                    ..Default::default()
                },
            )
            .await?
            .map(|(id, _)| id);
        } else if let Some(url) = object
            .and_then(|o| o.get("url"))
            .and_then(Value::as_str)
            .filter(|url| !url.trim().is_empty())
        {
            original_id = fetch_remote_status(state, url).await?;
        }
    }

    let Some(mut original_id) = original_id else {
        return Ok(());
    };
    // `announceable?`: the booster's own post, or a public or unlisted one.
    let announceable = sqlx::query_scalar!(
        r#"SELECT (account_id = $2 OR visibility IN (0, 1)) AS "ok!" FROM statuses WHERE id = $1"#,
        original_id,
        booster_id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    if !announceable {
        return Ok(());
    }
    // `return if requested_through_relay?`: an enabled relay's Announce brings
    // the post here, and is not a boost.
    if requested_through_relay {
        return Ok(());
    }
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
    // `Status.find_by(account: @account, reblog: original_status)`: a boost
    // the booster already made stands.
    let already_boosted = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM statuses
             WHERE account_id = $1 AND reblog_of_id = $2 AND deleted_at IS NULL) AS "e!""#,
        booster_id,
        original_id,
    )
    .fetch_one(&state.db)
    .await?;
    if already_boosted {
        return Ok(());
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
    let announce_to = ids(activity.get("to"));
    let announce_cc = ids(activity.get("cc"));
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
        crate::search::elasticsearch::indexing::status(state, original_id).await;
        crate::search::elasticsearch::indexing::account(state, booster_id).await;
    }

    // Update the original status's reblogs_count
    let _ = sqlx::query!(
        r#"INSERT INTO status_stats (status_id, reblogs_count, created_at, updated_at)
           VALUES ($1, 1, now(), now())
           ON CONFLICT (status_id) DO UPDATE
             SET reblogs_count = (SELECT COUNT(*) FROM statuses
                                  WHERE reblog_of_id = $1 AND deleted_at IS NULL),
                 untrusted_reblogs_count = CASE WHEN status_stats.untrusted_reblogs_count IS NULL THEN NULL ELSE LEAST(GREATEST(status_stats.untrusted_reblogs_count + (SELECT COUNT(*) FROM statuses
                                  WHERE reblog_of_id = $1 AND deleted_at IS NULL) - status_stats.reblogs_count, 0), 100000000) END,
                 updated_at = now()"#,
        original_id,
    )
    .execute(&state.db)
    .await;

    // `ActivityPub::Activity::Announce`: `Trends.register!`.
    if let Some(boost_id) = inserted {
        crate::trends::register(state, boost_id).await;
        crate::fasp::events::status_created(state, boost_id).await;
    }

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
        crate::feed::distribute_later(state, boost_id).await;
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

    // `status_from_uri(object_uri)`, fetching nothing: only a favourite of a
    // local post is recorded (`return if original_status.nil? ||
    // !original_status.account.local?`).
    let Some(status_id) = crate::federation::local_uri::status(state, object_uri).await else {
        return Ok(());
    };
    let local_author = sqlx::query_scalar!(
        r#"SELECT (a.domain IS NULL) AS "local!" FROM statuses s
           JOIN accounts a ON a.id = s.account_id WHERE s.id = $1"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    if !local_author {
        return Ok(());
    }

    let account_id = match resolve_or_fetch_remote_account(state, actor_uri).await {
        Ok(id) => id,
        Err(_) => return Ok(()),
    };

    let favourited = sqlx::query!(
        "INSERT INTO favourites (account_id, status_id, created_at, updated_at) VALUES ($1,$2, now(), now()) ON CONFLICT DO NOTHING",
        account_id,
        status_id
    )
    .execute(&state.db)
    .await?
    .rows_affected()
        > 0;
    if favourited {
        crate::fasp::events::favourite_created(state, status_id).await;
    }
    crate::search::elasticsearch::indexing::status_interaction(state, status_id).await;

    sqlx::query!(
        r#"INSERT INTO status_stats (status_id, favourites_count, created_at, updated_at)
           VALUES ($1, (SELECT COUNT(*) FROM favourites WHERE status_id = $1), now(), now())
           ON CONFLICT (status_id) DO UPDATE
             SET favourites_count = (SELECT COUNT(*) FROM favourites WHERE status_id = $1),
                 untrusted_favourites_count = CASE WHEN status_stats.untrusted_favourites_count IS NULL THEN NULL ELSE LEAST(GREATEST(status_stats.untrusted_favourites_count + (SELECT COUNT(*) FROM favourites WHERE status_id = $1) - status_stats.favourites_count, 0), 100000000) END,
                 updated_at = now()"#,
        status_id
    )
    .execute(&state.db)
    .await?;
    // `ActivityPub::Activity::Like`: `Trends.statuses.register`.
    if favourited {
        crate::trends::register_status(state, status_id).await;
    }

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
    // `@account.schedule_refresh_if_stale!`.
    if let Some(sender) = activity.get("actor").and_then(|a| a.as_str()) {
        if let Some(id) = sqlx::query_scalar!(
            "SELECT id FROM accounts WHERE uri = $1 AND domain IS NOT NULL ORDER BY id LIMIT 1",
            sender
        )
        .fetch_optional(&state.db)
        .await?
        {
            crate::federation::process_account::schedule_refresh_if_stale(state, id).await;
        }
    }
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

    // `equals_or_includes_any?(@object['type'], %w(Application Group …))`:
    // an actor may have several types.
    let obj_type = if crate::federation::fetch_resource::type_matches(
        object,
        &crate::federation::fetch_resource::ACTOR_TYPES,
    ) {
        "Person"
    } else if super::status_parser::is_status_type(object) {
        // `supported_object_type? || converted_object_type?`.
        "Note"
    } else {
        object.get("type").and_then(|t| t.as_str()).unwrap_or("")
    };
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

            update_remote_actor(state, object).await?;
        }
        "Note" => {
            let note_uri = object.get("id").and_then(|i| i.as_str()).unwrap_or("");
            if note_uri.is_empty() {
                return Ok(());
            }

            let text = super::status_parser::processed_text(state, object);
            let spoiler_text = super::status_parser::processed_spoiler_text(object);
            // `@account.sensitized? || @status_parser.sensitive`.
            let sensitive = object
                .get("sensitive")
                .and_then(|s| s.as_bool())
                .unwrap_or(false)
                || sqlx::query_scalar!(
                    r#"SELECT (sensitized_at IS NOT NULL) AS "s!" FROM accounts
                       WHERE uri = $1 AND domain IS NOT NULL"#,
                    activity
                        .get("actor")
                        .and_then(|a| a.as_str())
                        .unwrap_or_default(),
                )
                .fetch_optional(&state.db)
                .await?
                .unwrap_or(false);
            // `StatusParser#language`.
            let language = super::status_parser::language(object);
            let edited_at = object
                .get("updated")
                .and_then(|p| p.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&chrono::Utc).naive_utc());

            // `ProcessStatusUpdateService`: the post's quote policy as it now is.
            let actor_account: Option<i64> = sqlx::query_scalar!(
                "SELECT id FROM accounts WHERE uri = $1 AND domain IS NOT NULL",
                activity
                    .get("actor")
                    .and_then(|a| a.as_str())
                    .unwrap_or_default(),
            )
            .fetch_optional(&state.db)
            .await?;
            let quote_policy = match actor_account {
                Some(id) => Some(super::remote_quote_policy(state, id, object).await),
                None => None,
            };
            let previous = sqlx::query!(
                "SELECT text, spoiler_text, edited_at FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
                note_uri
            )
            .fetch_optional(&state.db)
            .await?;
            let previous_text: Option<String> = previous.as_ref().map(|p| p.text.clone());
            // `handle_explicit_update!` when the post says it was edited
            // since we last had it, `handle_implicit_update!` otherwise.
            let explicit = match (edited_at, previous.as_ref().and_then(|p| p.edited_at)) {
                (Some(new), Some(old)) => new > old,
                (Some(_), None) => true,
                (None, _) => false,
            };
            // `already_updated_more_recently?`, and an update older than the
            // edit we hold, are both left alone.
            if let Some(old) = previous.as_ref().and_then(|p| p.edited_at) {
                if edited_at.is_none_or(|new| new < old) {
                    return Ok(());
                }
            }
            if previous.is_some() && !explicit {
                return implicit_status_update(state, activity, object, note_uri, quote_policy)
                    .await;
            }
            let text_changed = previous
                .as_ref()
                .is_some_and(|p| p.text != text || p.spoiler_text != spoiler_text);
            let updated = sqlx::query!(
                r#"UPDATE statuses
                   SET text = $2, spoiler_text = $3, sensitive = $4, language = $5,
                       edited_at = COALESCE($6, edited_at), updated_at = now(),
                       quote_approval_policy = COALESCE($8, quote_approval_policy)
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
                quote_policy,
            )
            .fetch_optional(&state.db)
            .await?;

            if updated.is_none() {
                return create_from_update(state, activity, object).await;
            }

            let Some(row) = updated else {
                return Ok(());
            };
            crate::fasp::events::status_updated(state, row.id).await;

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

            // `reset_preview_card!`, when the text changed: the card is
            // fetched again, from the `Link` attachment if there is one.
            if previous_text.as_deref() != Some(text.as_str()) {
                crate::preview_card::reset(state, row.id).await;
                crate::preview_card::crawl_later(
                    state,
                    row.id,
                    preview_card_link(&attachments).map(str::to_owned),
                )
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
            for tag in tags_arr.iter().filter(|t| type_is(t, "Hashtag")) {
                let name = match tag
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|n| n.trim_start_matches('#').to_lowercase())
                    .filter(|n| !n.is_empty())
                {
                    Some(n) => n,
                    None => continue,
                };
                if let Ok(Some(tid)) = crate::tags::find_or_create(&state.db, &name).await {
                    let _ = sqlx::query!(
                        "UPDATE tags SET last_status_at = now(), updated_at = now() WHERE id = $1",
                        tid
                    )
                    .execute(&state.db)
                    .await;
                    let _ = sqlx::query!("INSERT INTO statuses_tags (status_id, tag_id) VALUES ($1,$2) ON CONFLICT DO NOTHING", row.id, tid)
                        .execute(&state.db).await;
                    crate::search::elasticsearch::indexing::tags(state, &[tid]).await;
                }
            }
            // An edit: `update_index('statuses', :proper)`.
            crate::search::elasticsearch::indexing::status(state, row.id).await;

            sync_remote_poll(state, row.id, row.account_id, object).await?;
            // `update_counts!`.
            super::status_parser::store_untrusted_counts(state, row.id, object).await?;

            // `update_quote!` or `update_quote_approval!`, then
            // `broadcast_updates!`: for an edit that changed something, and
            // for any update that moved the quote.
            let quote_moved = super::quote::update_quote(
                state,
                row.id,
                row.account_id,
                object,
                activity.get("@context"),
                explicit,
            )
            .await?;
            if quote_moved || (explicit && text_changed) {
                crate::quotes::distribute_update(state, row.id, false).await;
            }
            // `forward_activity! if significant_changes? &&
            // @status_parser.edited_at > last_edit_date`.
            if explicit
                && (text_changed || quote_moved)
                && crate::federation::forwarder::forwardable(state, activity, row.id).await
            {
                crate::federation::forwarder::forward(state, row.account_id, activity, row.id)
                    .await;
            }
        }
        _ => {}
    }

    Ok(())
}

/// `Update#update_status` for a status we do not hold from the sender:
/// `return if @status.nil? && (@account.suspended? || object_too_old?)`,
/// then `Create.new(@json, @account, **@options).perform` — the updated
/// object taken as the `Create` it would have come in, delivered like the
/// `Update` was.
async fn create_from_update(state: &AppState, activity: &Value, object: &Value) -> AppResult<()> {
    let sender = activity
        .get("actor")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let suspended = sqlx::query_scalar!(
        r#"SELECT (suspended_at IS NOT NULL) AS "s!" FROM accounts
           WHERE uri = $1 AND domain IS NOT NULL ORDER BY id LIMIT 1"#,
        sender,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    // `object_too_old?`: published more than a day ago (`OBJECT_AGE_THRESHOLD`).
    let too_old = object
        .get("published")
        .and_then(Value::as_str)
        .and_then(|p| chrono::DateTime::parse_from_rfc3339(p).ok())
        .is_some_and(|p| {
            p.with_timezone(&chrono::Utc) < chrono::Utc::now() - chrono::Duration::days(1)
        });
    if suspended || too_old {
        return Ok(());
    }
    let mut create = activity.clone();
    create["type"] = Value::String("Create".into());
    Box::pin(super::create::create(
        state,
        &create,
        &super::create::CreateOptions::default(),
    ))
    .await
}

/// `ProcessStatusUpdateService#handle_implicit_update!`: an update that does
/// not say the status was edited since we last had it — a status fetched
/// again, say — leaves its text, media and tags as they are, and refreshes
/// only its quote policy, its poll's tallies and its quote's approval.
async fn implicit_status_update(
    state: &AppState,
    activity: &Value,
    object: &Value,
    note_uri: &str,
    quote_policy: Option<i32>,
) -> AppResult<()> {
    let sender = activity
        .get("actor")
        .and_then(|a| a.as_str())
        .unwrap_or_default();
    // `update_interaction_policies!`, which moves `updated_at` only when the
    // policy changed.
    let row = sqlx::query!(
        r#"UPDATE statuses
           SET quote_approval_policy = COALESCE($2, quote_approval_policy),
               updated_at = CASE
                   WHEN quote_approval_policy IS DISTINCT FROM COALESCE($2, quote_approval_policy)
                   THEN now() ELSE updated_at END
           WHERE uri = $1 AND deleted_at IS NULL
             AND account_id = (
                 SELECT id FROM accounts WHERE uri = $3 AND domain IS NOT NULL
             )
           RETURNING id, account_id"#,
        note_uri,
        quote_policy,
        sender,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return create_from_update(state, activity, object).await;
    };
    sync_remote_poll(state, row.id, row.account_id, object).await?;
    // `update_counts!`.
    super::status_parser::store_untrusted_counts(state, row.id, object).await?;
    // `update_quote_approval!`, then `broadcast_updates!` if the quote
    // changed state.
    if super::quote::update_quote(
        state,
        row.id,
        row.account_id,
        object,
        activity.get("@context"),
        false,
    )
    .await?
    {
        crate::quotes::distribute_update(state, row.id, false).await;
    }
    Ok(())
}

/// Mastodon's `ActivityPub::Activity::Update#update_account`: the sender
/// describes itself anew, and `ProcessAccountService` stores what it says.
/// The caller has made sure the document is the sender's own.
pub(crate) async fn update_remote_actor(state: &AppState, object: &Value) -> AppResult<()> {
    let actor_uri = object.get("id").and_then(|i| i.as_str()).unwrap_or("");
    if actor_uri.is_empty() {
        return Ok(());
    }
    let Some(account) = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE uri = $1 AND domain IS NOT NULL ORDER BY id LIMIT 1",
        actor_uri,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    if let Err(error) = crate::federation::process_account::process(
        state,
        object,
        crate::federation::process_account::Options {
            account: Some(&account),
            signed_with_known_key: true,
            ..Default::default()
        },
    )
    .await
    {
        tracing::debug!(actor_uri, %error, "Update of an actor not stored");
    }
    Ok(())
}
