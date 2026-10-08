//! Inbound status-lifecycle activities: `Delete` (tombstone a remote status),
//! `Announce` (boost), `Like` (favourite), and `Update` (edit a remote status,
//! sync its poll). Includes notify_status_author, shared by Announce and Like.

use serde_json::Value;

use crate::{error::AppResult, state::AppState};

use super::attachment::preview_card_link;
use super::{
    acquire_create_lock, delete_arrived_first, delete_later, fetch_remote_status,
    resolve_or_fetch_remote_account, same_host, sync_remote_poll,
};
use ojak_vocab::json_ld_helper::{ids, type_is};

/// `ActivityPub::Activity::Delete#perform`: the sender itself
/// (`delete_person`), an authorization to feature it that it gave
/// (`delete_feature_authorization!`), or else one of its statuses or a quote
/// stamp it gave (`delete_object`).
pub(super) async fn handle_delete(
    state: &AppState,
    _instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let Some(uri) = activity
        .get("object")
        .and_then(crate::federation::json_ld::value_or_id)
    else {
        return Ok(());
    };
    // `@account`, the sender, whom the inbox knows.
    let Some(account_id) = sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE uri = $1 AND domain IS NOT NULL ORDER BY id LIMIT 1",
        actor_uri,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };

    // `delete_person`, once at a time (`delete_in_progress:#{@account.id}`,
    // held two hours, and skipped by whoever finds it held): purged
    // outright, without announcing anything back over ActivityPub.
    if uri == actor_uri {
        let Some(lock) = crate::redis_lock::try_acquire_lockable(
            state,
            &format!("delete_in_progress:{account_id}"),
            2 * 60 * 60 * 1000,
        )
        .await
        else {
            return Ok(());
        };
        let deleted = crate::delete_account::call(
            state,
            account_id,
            crate::delete_account::Options {
                reserve_username: false,
                skip_activitypub: true,
                ..Default::default()
            },
        )
        .await;
        lock.release().await;
        deleted.map_err(crate::error::AppError::Internal)?;
        tracing::debug!(actor_uri, "purged remote account on Delete(actor)");
        return Ok(());
    }

    // `delete_feature_authorization!`: the item of a local collection
    // featuring the sender, by the authorization it gave
    // (`CollectionItem.local.find_by(approval_uri:, account_id: @account.id)`),
    // revoked (`DeleteCollectionItemService` with `revoke: true`).
    if let Some(item) = sqlx::query!(
        r#"SELECT ci.id
           FROM collection_items ci JOIN collections c ON c.id = ci.collection_id
           WHERE c.local AND ci.approval_uri = $1 AND ci.account_id = $2
           ORDER BY ci.id LIMIT 1"#,
        uri,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?
    {
        crate::api::mastodon::collections::delete_item(state, item.id, true).await?;
        return Ok(());
    }

    // `delete_object`, once at a time for a URI
    // (`delete_status_in_progress:#{object_uri}`, skipped when held).
    let Some(lock) = crate::redis_lock::try_acquire_lockable(
        state,
        &format!("delete_status_in_progress:{uri}"),
        crate::redis_lock::DEFAULT_TTL_MS,
    )
    .await
    else {
        return Ok(());
    };
    let result = delete_object(state, activity, actor_uri, account_id, uri).await;
    // Released as the block `with_redis_lock` runs ends.
    lock.release().await;
    result
}

/// `Delete#delete_object`, under its lock.
async fn delete_object(
    state: &AppState,
    activity: &Value,
    actor_uri: &str,
    account_id: i64,
    uri: &str,
) -> AppResult<()> {
    // On the sender's own host, the URI is remembered as deleted, so that a
    // `Create` of it arriving late is skipped (`delete_later!`, under the
    // `create:` lock a concurrent `Create` holds while it stores the status),
    // and tombstoned.
    if same_host(actor_uri, uri) {
        let create_lock = acquire_create_lock(state, uri).await;
        delete_later(state, actor_uri, uri).await;
        if let Some(lock) = create_lock {
            lock.release().await;
        }
        // `Tombstone.find_or_create_by(uri:, account: @account)`.
        sqlx::query!(
            r#"INSERT INTO tombstones (id, account_id, uri, created_at, updated_at)
               SELECT $1, $2, $3::text, now(), now()
               WHERE NOT EXISTS (SELECT 1 FROM tombstones WHERE uri = $3::text AND account_id = $2)"#,
            crate::snowflake::next_id(),
            account_id,
            uri,
        )
        .execute(&state.db)
        .await?;
    }

    // `case @object['type']`: a `QuoteAuthorization` is a stamp taken back
    // (`revoke_quote`), a `Note` or `Question` a status, and anything else
    // whichever of the two it turns out to be.
    let object_type = activity
        .get("object")
        .and_then(|o| o.get("type"))
        .and_then(|t| t.as_str());
    match object_type {
        Some("QuoteAuthorization") => {
            super::quote::revoke_by_stamp(state, activity, actor_uri, uri).await?;
        }
        Some("Note" | "Question") => {
            delete_status(state, activity, account_id, uri).await?;
        }
        _ => {
            if !delete_status(state, activity, account_id, uri).await? {
                super::quote::revoke_by_stamp(state, activity, actor_uri, uri).await?;
            }
        }
    }
    Ok(())
}

/// `delete_status`: the sender's own status, by the URI or the object's
/// `atomUri` (`Status.find_by(uri:, account: @account)`), forwarded to the
/// followers of the local accounts that shared it and then removed
/// (`RemoveStatusService` with `redraft: false`, which takes it off every
/// feed and removes the boosts of it). Says whether there was one.
async fn delete_status(
    state: &AppState,
    activity: &Value,
    account_id: i64,
    uri: &str,
) -> AppResult<bool> {
    let atom_uri = activity
        .get("object")
        .filter(|o| o.is_object())
        .and_then(|o| o.get("atomUri"))
        .and_then(|u| u.as_str())
        .filter(|u| !u.trim().is_empty());
    let mut target = None;
    for candidate in std::iter::once(uri).chain(atom_uri) {
        target = sqlx::query_scalar!(
            "SELECT id FROM statuses
             WHERE uri = $1 AND account_id = $2 AND deleted_at IS NULL",
            candidate,
            account_id,
        )
        .fetch_optional(&state.db)
        .await?;
        if target.is_some() {
            break;
        }
    }
    let Some(status_id) = target else {
        return Ok(false);
    };
    // `forwarder.forward! if forwarder.forwardable?`, before the status goes.
    if crate::federation::forwarder::forwardable(state, activity, status_id).await {
        crate::federation::forwarder::forward(state, account_id, activity, status_id).await;
    }
    crate::remove_status::call(
        state,
        status_id,
        crate::remove_status::Options {
            redraft: false,
            ..crate::remove_status::Options::default()
        },
    )
    .await?;
    Ok(true)
}

/// `RemoveStatusService` with `redraft: false` for a remote status whose
/// server says it is gone (`FetchRemoteStatusService#fetch_status`).
pub(super) async fn remove_remote_status(state: &AppState, status_id: i64) -> AppResult<()> {
    crate::remove_status::call(state, status_id, crate::remove_status::Options::default()).await?;
    Ok(())
}

pub(super) async fn handle_announce(
    state: &AppState,
    instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    // `with_redis_lock("announce:#{value_or_id(@object)}")`, so that two
    // copies of one boost are not both stored. Eunha takes it before the
    // checks Mastodon makes first, which only read.
    let Some(object_id) = activity
        .get("object")
        .and_then(crate::federation::json_ld::value_or_id)
        .map(str::to_owned)
    else {
        return announce(state, instance, activity).await;
    };
    let lock = super::acquire_lockable_or_retry(state, &format!("announce:{object_id}")).await?;
    let result = Box::pin(announce(state, instance, activity)).await;
    lock.release().await;
    result
}

/// `ActivityPub::Activity::Announce#perform`, under its lock.
async fn announce(
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
    let followed_by_local_accounts =
        super::followed_by_local_accounts(state, activity, booster_id).await?;
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
    let booster_followers: String = sqlx::query_scalar!(
        "SELECT followers_url FROM accounts WHERE id = $1",
        booster_id
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or_default();
    let visibility =
        crate::db::models::vis::of_remote(&announce_to, &announce_cc, &booster_followers);

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
    if let Some(boost_id) = inserted {
        // `set_conversation`: a boost starts a conversation of its own.
        crate::conversation::assign(&state.db, boost_id).await?;
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
    notify_status_author(state, original_id, booster_id, "reblog").await;

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
        .and_then(crate::federation::json_ld::value_or_id)
        .unwrap_or("");

    // Skip a Like whose Undo already arrived out of order.
    if delete_arrived_first(state, actor_uri, activity_uri).await {
        return Ok(());
    }

    // `status_from_uri(object_uri)`, fetching nothing: only a favourite of a
    // local post is recorded (`return if original_status.nil? ||
    // !original_status.account.local?`).
    let Some(status_id) = super::kept_status(state, object_uri).await? else {
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
    notify_status_author(state, status_id, account_id, "favourite").await;

    Ok(())
}

/// Notify a status's author that `actor_id` interacted with it (favourite or
/// reblog from a remote account). No-ops if the author is remote.
async fn notify_status_author(
    state: &AppState,
    status_id: i64,
    actor_id: i64,
    notification_type: &'static str,
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
    crate::push::create_and_push(
        state,
        author_id,
        actor_id,
        notification_type,
        Some(status_id),
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
            // `update_collection`: one of the sender's own, from its own
            // host (`ProcessFeaturedCollectionService`).
            let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
            let object_uri = object.get("id").and_then(Value::as_str).unwrap_or("");
            if actor_uri.is_empty() || !same_host(actor_uri, object_uri) {
                tracing::debug!(
                    object_uri,
                    "refused an Update of a collection off its sender's host"
                );
                return Ok(());
            }
            if let Ok(owner_id) = resolve_or_fetch_remote_account(state, actor_uri).await {
                crate::federation::featured_collections::process_featured_collection(
                    state, owner_id, actor_uri, object,
                )
                .await
                .map_err(crate::error::AppError::Internal)?;
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
            // `update_poll!` saving a `Question` with no option raises
            // `RecordInvalid` inside the edit's transaction, and none of the
            // edit is kept.
            if super::poll_parser::PollParser::parse(object)
                .is_some_and(|poll| poll.options.is_empty())
            {
                tracing::debug!(note_uri, "refused an edit whose poll has no options");
                return Ok(());
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

            // `update_media_attachments!`: each attachment the object lists,
            // the one the status already had at its URL updated in place, a
            // new one created; at most `MEDIA_ATTACHMENTS_LIMIT`. Those no
            // longer listed stay attached, for the history, and the order is
            // recorded.
            let attachments = super::attachment::attachments_of(object);
            let previous_media = sqlx::query!(
                "SELECT id, remote_url FROM media_attachments WHERE status_id = $1",
                row.id
            )
            .fetch_all(&state.db)
            .await?;
            let skip_download =
                crate::federation::moderation::account_media_rejected(state, row.account_id).await;
            let mut media_ids: Vec<i64> = Vec::new();
            for att in &attachments {
                if media_ids.len() >= 4 {
                    break;
                }
                let Some(media) = super::attachment::remote_media(att) else {
                    continue;
                };
                // `skip_download`: a new attachment from a domain blocked
                // with `reject_media` is recorded without its file.
                let media = if skip_download {
                    media.not_downloaded()
                } else {
                    media
                };
                let focus = media
                    .file_meta
                    .as_ref()
                    .and_then(|meta| meta.get("focus"))
                    .cloned();
                if let Some(previous) = previous_media
                    .iter()
                    .find(|m| m.remote_url == media.remote_url && !media_ids.contains(&m.id))
                {
                    sqlx::query!(
                        r#"UPDATE media_attachments
                           SET description = $2, thumbnail_remote_url = $3, blurhash = $4,
                               file_meta = CASE WHEN $5::jsonb IS NULL THEN file_meta
                                   ELSE jsonb_set(COALESCE(file_meta::jsonb, '{}'::jsonb), '{focus}', $5::jsonb)::json END,
                               status_id = $6, updated_at = now()
                           WHERE id = $1"#,
                        previous.id,
                        media.description,
                        media.thumbnail_remote_url,
                        media.blurhash,
                        focus,
                        row.id,
                    )
                    .execute(&state.db)
                    .await?;
                    media_ids.push(previous.id);
                    continue;
                }
                let media_id = crate::snowflake::next_id();
                if let Ok(id) = sqlx::query_scalar!(
                    r#"INSERT INTO media_attachments (id, account_id, status_id, remote_url, description, blurhash, type, thumbnail_remote_url, file_content_type, file_meta, processing, created_at, updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10, 2, now(), now()) RETURNING id"#,
                    media_id, row.account_id, row.id, media.remote_url, media.description, media.blurhash, media.kind, media.thumbnail_remote_url, media.file_content_type, media.file_meta,
                ).fetch_one(&state.db).await { media_ids.push(id); }
            }
            sqlx::query!(
                "UPDATE statuses SET ordered_media_attachment_ids = $1 WHERE id = $2",
                &media_ids,
                row.id
            )
            .execute(&state.db)
            .await?;

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
            let previous_tags: Vec<i64> = sqlx::query_scalar!(
                "DELETE FROM statuses_tags WHERE status_id = $1 RETURNING tag_id",
                row.id
            )
            .fetch_all(&state.db)
            .await?;
            let mut current_tags: Vec<i64> = vec![];
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
                    if !current_tags.contains(&tid) {
                        current_tags.push(tid);
                    }
                }
            }
            // `update_mentions!`: whoever it now tags is mentioned, and not
            // silently; whoever it no longer tags stays mentioned, silently,
            // as taking back access from someone already told would confuse
            // more than it helps. An account that cannot be fetched for now
            // is tried again later.
            let mut mentioned: Vec<i64> = Vec::new();
            let mut unresolved: Vec<String> = Vec::new();
            for href in tags_arr
                .iter()
                .filter(|t| type_is(t, "Mention"))
                .filter_map(|t| t.get("href").and_then(Value::as_str))
                .filter(|href| !href.trim().is_empty())
            {
                match resolve_or_fetch_remote_account(state, href).await {
                    Ok(id) => {
                        if !mentioned.contains(&id) {
                            mentioned.push(id);
                        }
                    }
                    Err(error) if super::create::fetch_failed_for_now(&error) => {
                        if !unresolved.iter().any(|u| u == href) {
                            unresolved.push(href.to_owned());
                        }
                    }
                    Err(_) => {}
                }
            }
            for &id in &mentioned {
                sqlx::query!(
                    r#"INSERT INTO mentions (status_id, account_id, silent, created_at, updated_at)
                       VALUES ($1, $2, false, now(), now())
                       ON CONFLICT (account_id, status_id) DO UPDATE SET silent = false, updated_at = now()"#,
                    row.id,
                    id,
                )
                .execute(&state.db)
                .await?;
            }
            sqlx::query!(
                "UPDATE mentions SET silent = true WHERE status_id = $1 AND NOT (account_id = ANY($2))",
                row.id,
                &mentioned,
            )
            .execute(&state.db)
            .await?;
            for uri in unresolved {
                super::create::resolve_mention_later(state, row.id, uri, None).await;
            }

            // `update_tags!`: the featured tags the edit added or took away.
            if let Some(counted) = sqlx::query!(
                "SELECT visibility, created_at FROM statuses WHERE id = $1",
                row.id
            )
            .fetch_optional(&state.db)
            .await?
            {
                if let Err(error) = crate::featured_tags::update_for_status(
                    &state.db,
                    row.account_id,
                    row.id,
                    counted.visibility,
                    counted.created_at,
                    &previous_tags,
                    &current_tags,
                )
                .await
                {
                    tracing::warn!(%error, "could not move an edited status's featured tags");
                }
            }
            // `update_tagged_objects!`.
            crate::federation::tagged_collections::update(state, row.id, object).await;
            // An edit: `update_index('statuses', :proper)`.
            crate::search::elasticsearch::indexing::status(state, row.id).await;

            sync_remote_poll(state, row.id, row.account_id, object, true).await?;
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
    // `update_poll!(allow_significant_changes: false)`; a poll that is not
    // valid raises, and nothing after it runs.
    if !sync_remote_poll(state, row.id, row.account_id, object, false).await? {
        return Ok(());
    }
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
