//! Removing a status: Mastodon's `RemoveStatusService`, the `RemovalWorker`
//! that runs it from the job queue, and the daily pass of
//! `Scheduler::UserCleanupScheduler` that purges what it kept
//! (`clean_discarded_statuses!`).
//!
//! A removal first discards the status and the boosts of it
//! (`Status#discard_with_reblogs`), takes it off every feed and stream, tells
//! other servers when the author is local, removes its boosts, its featured
//! tags' counts and its media, and revokes the quote it made of a local post.
//! Then, unless it is to be kept, the row is destroyed (`@status.destroy!`)
//! with what Rails destroys along with it, and only then is it uncounted.
//!
//! It is kept, discarded, when a moderator may still need it: when it is
//! cited by an unresolved report or by a strike (`Status#reported?`), or when
//! the removal asks to `preserve` it, as a moderator's deletion of a local
//! post does. `immediate` destroys it all the same. What is kept is purged
//! [`DISCARDED_STATUSES_MAX_AGE_DAYS`] after it was discarded.
//!
//! Its media goes with a destroyed status, unless the removal is a `redraft`,
//! which leaves the attachments to be attached to the post that replaces it.
//! The media of a status that is kept would be made private
//! (`UpdateMediaAttachmentsPermissionsService`), which eunha's storage, with no
//! per-object permissions, has nothing to do for.

use anyhow::{Context, Result};
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

use crate::db::models::{vis, Account, Status};
use crate::state::AppState;

/// `UserCleanupScheduler::DISCARDED_STATUSES_MAX_AGE_DAYS`.
pub const DISCARDED_STATUSES_MAX_AGE_DAYS: i32 = 30;

/// How many statuses one batch of the cleanup queues, as `find_in_batches`.
const BATCH: i64 = 1000;

/// `RemoveStatusService#call`'s options.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    /// Leave the media to be attached again, for delete-and-redraft.
    pub redraft: bool,
    /// Destroy the status even if a moderator might need it.
    pub immediate: bool,
    /// Keep the status, discarded, for moderators.
    pub preserve: bool,
    /// The status is a boost removed because what it boosted was: no `Undo`
    /// is sent, the original's `Delete` covering it.
    pub original_removed: bool,
    /// Stream no `delete` to the mentioned accounts, the hashtags and the
    /// public timelines.
    pub skip_streaming: bool,
}

/// `RemovalWorker`: [`call`] for a status, discarded or not, from the job
/// queue. A status that is gone already is nothing to do.
#[derive(Serialize, Deserialize)]
pub struct RemovalWorker {
    pub status_id: i64,
    #[serde(default)]
    pub options: Options,
}

impl crate::jobs::Job for RemovalWorker {
    const KIND: &'static str = "RemovalWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT;

    async fn perform(self, state: &AppState) -> Result<()> {
        call(state, self.status_id, self.options).await
    }
}

/// `clean_discarded_statuses!`: a [`RemovalWorker`] with `immediate` and
/// `skip_streaming` for every status discarded more than
/// [`DISCARDED_STATUSES_MAX_AGE_DAYS`] ago. Returns how many were queued.
pub async fn clean_discarded_statuses(state: &AppState) -> Result<usize> {
    let mut queued = 0;
    let mut after = 0_i64;
    loop {
        let ids: Vec<i64> = sqlx::query_scalar!(
            r#"SELECT id FROM statuses
               WHERE deleted_at IS NOT NULL
                 AND deleted_at <= now() - make_interval(days => $1)
                 AND id > $2
               ORDER BY id LIMIT $3"#,
            DISCARDED_STATUSES_MAX_AGE_DAYS,
            after,
            BATCH,
        )
        .fetch_all(&state.db)
        .await?;
        let Some(&last) = ids.last() else {
            break;
        };
        after = last;
        queued += crate::jobs::perform_bulk(
            state,
            ids.into_iter().map(|status_id| RemovalWorker {
                status_id,
                options: Options {
                    immediate: true,
                    skip_streaming: true,
                    ..Options::default()
                },
            }),
        )
        .await?;
    }
    Ok(queued)
}

async fn load_status(state: &AppState, id: i64) -> Result<Option<Status>> {
    Ok(
        sqlx::query_as!(Status, "SELECT * FROM statuses WHERE id = $1", id)
            .fetch_optional(&state.db)
            .await?,
    )
}

async fn load_account(state: &AppState, id: i64) -> Result<Option<Account>> {
    Ok(
        sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", id)
            .fetch_optional(&state.db)
            .await?,
    )
}

/// `with_redis_lock("distribute:#{@status.id}")`, which raises when another
/// holds it, so that a queued removal is retried. It waits a moment first,
/// as a removal from a request or an inbox should not fail on a brief
/// overlap.
async fn lock(state: &AppState, status_id: i64) -> Result<crate::redis_lock::RedisLock> {
    let name = format!("distribute:{status_id}");
    for attempt in 0..40 {
        if let Some(lock) =
            crate::redis_lock::try_acquire(state, &name, crate::redis_lock::DEFAULT_TTL_MS).await
        {
            return Ok(lock);
        }
        if attempt < 39 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    anyhow::bail!("could not take the lock {name}")
}

/// `Status#discard_with_reblogs`: the status discarded now, and, unless it is
/// a boost, the boosts of it that were not discarded or were discarded with
/// it, at the same moment. Returns that moment. The status is saved through
/// its model ([`after_discard`]); its boosts are not (`update_all`).
pub async fn discard_with_reblogs_in(
    conn: &mut PgConnection,
    status: &Status,
) -> Result<NaiveDateTime> {
    let discarded_at = chrono::Utc::now().naive_utc();
    if status.reblog_of_id.is_none() {
        sqlx::query!(
            r#"UPDATE statuses SET deleted_at = $2
               WHERE reblog_of_id = $1
                 AND (deleted_at IS NULL OR deleted_at = $3)"#,
            status.id,
            discarded_at,
            status.deleted_at,
        )
        .execute(&mut *conn)
        .await?;
    }
    discard_in(conn, status.id, discarded_at).await?;
    Ok(discarded_at)
}

/// `Status#discard`: the status alone, through its model.
async fn discard_in(conn: &mut PgConnection, status_id: i64, at: NaiveDateTime) -> Result<()> {
    sqlx::query!(
        "UPDATE statuses SET deleted_at = $2, updated_at = now() WHERE id = $1",
        status_id,
        at,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The `after_update_commit` callbacks of a status saved through its model:
/// the `status.updated` webhook of a local one, and what providers are told.
/// Call it once the discard has committed.
pub async fn after_discard(state: &AppState, status_id: i64) {
    crate::moderation::webhooks::status_updated(state, status_id).await;
    crate::fasp::events::status_updated(state, status_id).await;
}

/// [`discard_with_reblogs_in`] and its callbacks.
pub async fn discard_with_reblogs(state: &AppState, status: &Status) -> Result<NaiveDateTime> {
    let mut conn = state.db.acquire().await?;
    let at = discard_with_reblogs_in(&mut conn, status).await?;
    drop(conn);
    after_discard(state, status.id).await;
    Ok(at)
}

/// [`discard_in`] and its callbacks, for a boost undone from the API
/// (`@status.discard`).
pub async fn discard(state: &AppState, status_id: i64) -> Result<()> {
    let mut conn = state.db.acquire().await?;
    discard_in(&mut conn, status_id, chrono::Utc::now().naive_utc()).await?;
    drop(conn);
    after_discard(state, status_id).await;
    Ok(())
}

/// `RemoveStatusService#call` for the status `status_id`, discarded or not.
/// A status that is gone already is nothing to do.
pub async fn call(state: &AppState, status_id: i64, options: Options) -> Result<()> {
    let Some(status) = load_status(state, status_id).await? else {
        return Ok(());
    };
    let Some(account) = load_account(state, status.account_id).await? else {
        return Ok(());
    };
    let _lock = lock(state, status.id).await?;
    let id = status.id;

    let discarded_at = discard_with_reblogs(state, &status).await?;

    // `StatusPin.find_by(status: @status)&.destroy`.
    sqlx::query!("DELETE FROM status_pins WHERE status_id = $1", id)
        .execute(&state.db)
        .await?;

    // `remove_from_self`, `remove_from_followers` and `remove_from_lists`, and
    // the streaming messages of the rest, while the feeds still hold it.
    crate::streaming::fan_out::remove(state, id, options.skip_streaming).await;
    remove_from_feeds(state, account.id, id, status.reblog_of_id).await;

    if account.is_local() && !options.original_removed {
        remove_from_remote_reach(state, &status, &account, discarded_at).await;
    }

    let reported = reported(state, &status).await?;
    let permanently = options.immediate || !(options.preserve || reported);

    // A boost mentions nobody, is not boosted, and carries no media or
    // hashtags of its own.
    if status.reblog_of_id.is_none() {
        remove_reblogs(state, id, discarded_at, options.skip_streaming).await?;
        if let Err(error) =
            crate::featured_tags::status_removed(&state.db, account.id, id, status.created_at).await
        {
            tracing::error!(status_id = id, %error, "could not count a removed status out of its featured tags");
        }
        if !options.redraft {
            remove_media(state, id, permanently).await?;
        }
    }

    // `RevokeQuoteService`, while there is a chance.
    crate::quotes::status_removed(state, id).await;

    // `update_index('statuses', :proper)` and the author's, for the discard.
    crate::search::elasticsearch::indexing::status(state, status.reblog_of_id.unwrap_or(id)).await;
    crate::search::elasticsearch::indexing::account(state, account.id).await;

    if permanently {
        destroy(state, &status, &account).await?;
    }
    Ok(())
}

/// `FeedManager#unpush_from_home` for the author when local and each
/// follower, and `#unpush_from_list` for each list, which run inline under
/// [`crate::feed::sync_fanout`] and otherwise in a task.
async fn remove_from_feeds(
    state: &AppState,
    author_id: i64,
    status_id: i64,
    reblog_of_id: Option<i64>,
) {
    let mut redis = state.redis.clone();
    let keys = state.redis_keys.clone();
    let db = state.db.clone();
    let work = async move {
        crate::feed::fanout_remove_boost(
            &mut redis,
            &keys,
            &db,
            author_id,
            status_id,
            reblog_of_id,
        )
        .await;
        crate::feed::fanout_remove_boost_from_lists(
            &mut redis,
            &keys,
            &db,
            author_id,
            status_id,
            reblog_of_id,
        )
        .await;
    };
    if crate::feed::sync_fanout() {
        work.await;
    } else {
        crate::tenants::spawn(work);
    }
}

/// `Status#reported?`: cited by an unresolved report about its author, or by
/// one of its author's strikes.
async fn reported(state: &AppState, status: &Status) -> Result<bool> {
    Ok(sqlx::query_scalar!(
        r#"SELECT (EXISTS (SELECT 1 FROM reports
                           WHERE target_account_id = $1 AND action_taken_at IS NULL
                             AND $2 = ANY(status_ids))
                   OR EXISTS (SELECT 1 FROM account_warnings
                              WHERE target_account_id = $1 AND $2::text = ANY(status_ids)))
           AS "reported!""#,
        status.account_id,
        status.id,
    )
    .fetch_one(&state.db)
    .await?)
}

/// `remove_reblogs`: a removal of each boost discarded with the status, or
/// not yet discarded, with `original_removed`.
async fn remove_reblogs(
    state: &AppState,
    status_id: i64,
    discarded_at: NaiveDateTime,
    skip_streaming: bool,
) -> Result<()> {
    let reblogs: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT id FROM statuses
           WHERE reblog_of_id = $1 AND (deleted_at IS NULL OR deleted_at = $2)
           ORDER BY id"#,
        status_id,
        discarded_at,
    )
    .fetch_all(&state.db)
    .await?;
    for reblog in reblogs {
        Box::pin(call(
            state,
            reblog,
            Options {
                original_removed: true,
                skip_streaming,
                ..Options::default()
            },
        ))
        .await?;
    }
    Ok(())
}

/// `remove_media`: a destroyed status's attachments are destroyed, their
/// files with them; a kept one's would be made private, which is nothing to
/// do here.
async fn remove_media(state: &AppState, status_id: i64, permanently: bool) -> Result<()> {
    if !permanently {
        return Ok(());
    }
    let rows = sqlx::query!(
        "SELECT id, file_file_name, thumbnail_file_name FROM media_attachments WHERE status_id = $1",
        status_id
    )
    .fetch_all(&state.db)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    sqlx::query!("DELETE FROM media_attachments WHERE id = ANY($1)", &ids)
        .execute(&state.db)
        .await?;
    for row in rows {
        for key in crate::media::attachment_keys(
            row.id,
            row.file_file_name.as_deref(),
            row.thumbnail_file_name.as_deref(),
        ) {
            if let Err(error) = state.storage.delete(&key).await {
                tracing::debug!(%error, key, "could not remove a destroyed attachment's file");
            }
        }
    }
    Ok(())
}

/// `remove_from_remote_reach`: a local status's `Delete`, or a local boost's
/// `Undo` of its `Announce`, to everyone `StatusReachFinder` finds with
/// `unsafe: true`.
async fn remove_from_remote_reach(
    state: &AppState,
    status: &Status,
    account: &Account,
    discarded_at: NaiveDateTime,
) {
    if let Err(error) = try_remove_from_remote_reach(state, status, account, discarded_at).await {
        tracing::warn!(status_id = status.id, %error, "could not federate a status's removal");
    }
}

async fn try_remove_from_remote_reach(
    state: &AppState,
    status: &Status,
    account: &Account,
    discarded_at: NaiveDateTime,
) -> Result<()> {
    if !crate::federation::keypair::has_signing_key(state, account.id)
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }
    let id = status.id;
    let domain = &state.instance.domain;
    let actor_url = crate::federation::tag::account_uri_of(domain, account);
    let key_id = format!("{actor_url}#main-key");
    let distributable = vis::distributable(status.visibility);
    let is_public = status.visibility == vis::PUBLIC;
    let followers_allowed = matches!(
        status.visibility,
        vis::PUBLIC | vis::UNLISTED | vis::PRIVATE
    );

    let (activity, reblog_of_account_id) = if let Some(original_id) = status.reblog_of_id {
        let original = sqlx::query!(
            "SELECT account_id, uri FROM statuses WHERE id = $1",
            original_id,
        )
        .fetch_optional(&state.db)
        .await?;
        let original_uri = original
            .as_ref()
            .and_then(|r| r.uri.clone())
            .unwrap_or_default();
        let announce_id = format!("{actor_url}/statuses/{id}/activity");
        let undo_id = format!("{announce_id}#undo");
        let undo = crate::federation::activity::undo_announce(
            &undo_id,
            &actor_url,
            &announce_id,
            &original_uri,
        )?;
        (undo, original.map(|r| r.account_id))
    } else if let Some(ref status_uri) = status.uri {
        let mut activity = crate::federation::activity::delete(
            &format!("{status_uri}#delete"),
            &actor_url,
            status_uri,
        )?;
        activity["to"] = serde_json::json!([crate::federation::activity::AS_PUBLIC]);
        if let Some(obj) = activity.get_mut("object").and_then(|o| o.as_object_mut()) {
            obj.insert("atomUri".to_string(), serde_json::json!(status_uri));
        }
        (activity, None)
    } else {
        return Ok(());
    };

    // `reblogs_account_ids` with `unsafe`: the boosts discarded with it count.
    let reblogger_ids: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT account_id FROM statuses
           WHERE reblog_of_id = $1 AND (deleted_at IS NULL OR deleted_at = $2)"#,
        id,
        discarded_at,
    )
    .fetch_all(&state.db)
    .await?;
    let inboxes = crate::federation::delivery::status_reach_inboxes(
        state,
        id,
        account.id,
        status.in_reply_to_account_id,
        distributable,
        true,
        is_public,
        followers_allowed,
        reblog_of_account_id,
        &reblogger_ids,
    )
    .await
    .unwrap_or_default();
    if inboxes.is_empty() {
        return Ok(());
    }
    // `always_sign`, for a status that `sign?`s.
    let signed = crate::federation::delivery::LinkedData::for_status(
        distributable,
        crate::federation::delivery::LinkedData::Always,
    );
    crate::federation::delivery::deliver_to_inboxes_signed(
        state, activity, inboxes, key_id, signed,
    )
    .await
    .context("could not queue a status's removal")?;
    Ok(())
}

// ── destroy! ──────────────────────────────────────────────────────────────

/// `@status.destroy!`: the row, and what Rails destroys with it.
///
///  -  `before_destroy :unlink_from_conversations!`, a direct status taken
///     out of its local participants' conversations;
///  -  `dependent: :destroy` on its favourites, its kept boosts, its
///     mentions, its notification, its poll and its quote, each with the
///     notifications they have (`has_one :notification` for all but the
///     poll's `has_many`), and the notification requests those leave behind
///     reconsidered (`Notification#remove_from_notification_request`);
///  -  `dependent: :destroy` on its bookmarks, edits and tagged objects,
///     `:delete` on its preview card link, and `:nullify` on its media and the
///     quotes of it, the rest of its rows going by their foreign keys;
///  -  and once committed, `decrement_counter_caches`, the quote's own
///     `decrement_counter_caches!`, what providers are told, and the search
///     indexes.
///
/// A boost discarded with the status is not among its `reblogs`, which are
/// under the default scope: that row goes with the status, by its foreign
/// key, once its own removal has destroyed it or kept it.
async fn destroy(state: &AppState, status: &Status, account: &Account) -> Result<()> {
    let id = status.id;
    if status.visibility == vis::DIRECT {
        unlink_from_conversations(state, status, account).await?;
    }

    // `after_commit :announce_deleted_content_to_subscribed_fasp, on:
    // :destroy`, which needs the row to describe.
    crate::fasp::events::status_deleted(state, id).await;

    // `has_many :reblogs, dependent: :destroy`: the kept ones.
    let kept_reblogs: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE reblog_of_id = $1 AND deleted_at IS NULL ORDER BY id",
        id
    )
    .fetch_all(&state.db)
    .await?;
    for reblog_id in kept_reblogs {
        let Some(reblog) = load_status(state, reblog_id).await? else {
            continue;
        };
        let Some(booster) = load_account(state, reblog.account_id).await? else {
            continue;
        };
        Box::pin(destroy(state, &reblog, &booster)).await?;
    }

    let mut tx = state.db.begin().await?;
    let mut touched: Vec<(i64, i64)> = vec![];

    // `has_many :favourites, dependent: :destroy`.
    let favourites: Vec<i64> =
        sqlx::query_scalar!("SELECT id FROM favourites WHERE status_id = $1", id)
            .fetch_all(&mut *tx)
            .await?;
    touched.extend(destroy_notifications(&mut tx, "Favourite", &favourites, true).await?);
    sqlx::query!("DELETE FROM favourites WHERE status_id = $1", id)
        .execute(&mut *tx)
        .await?;

    // `has_many :bookmarks, dependent: :destroy`.
    sqlx::query!("DELETE FROM bookmarks WHERE status_id = $1", id)
        .execute(&mut *tx)
        .await?;

    // `has_many :mentions, dependent: :destroy`.
    let mentions: Vec<i64> =
        sqlx::query_scalar!("SELECT id FROM mentions WHERE status_id = $1", id)
            .fetch_all(&mut *tx)
            .await?;
    touched.extend(destroy_notifications(&mut tx, "Mention", &mentions, true).await?);
    sqlx::query!("DELETE FROM mentions WHERE status_id = $1", id)
        .execute(&mut *tx)
        .await?;

    // `has_many :media_attachments, dependent: :nullify`: what a redraft
    // leaves, to be attached again.
    sqlx::query!(
        "UPDATE media_attachments SET status_id = NULL WHERE status_id = $1",
        id
    )
    .execute(&mut *tx)
    .await?;

    // `has_many :tagged_objects, dependent: :destroy`.
    sqlx::query!("DELETE FROM tagged_objects WHERE status_id = $1", id)
        .execute(&mut *tx)
        .await?;

    // `has_many :quotes, dependent: :nullify`.
    sqlx::query!(
        "UPDATE quotes SET quoted_status_id = NULL WHERE quoted_status_id = $1",
        id
    )
    .execute(&mut *tx)
    .await?;

    // `has_one :preview_cards_status, dependent: :delete`, which has no
    // foreign key to do it.
    sqlx::query!(
        "DELETE FROM preview_cards_statuses WHERE status_id = $1",
        id
    )
    .execute(&mut *tx)
    .await?;

    // `has_one :notification, as: :activity, dependent: :destroy`.
    touched.extend(destroy_notifications(&mut tx, "Status", &[id], true).await?);

    // `has_one :poll, dependent: :destroy`: its votes by `delete_all`, its
    // notifications by `has_many ... dependent: :destroy`.
    let polls: Vec<i64> = sqlx::query_scalar!("SELECT id FROM polls WHERE status_id = $1", id)
        .fetch_all(&mut *tx)
        .await?;
    touched.extend(destroy_notifications(&mut tx, "Poll", &polls, false).await?);
    sqlx::query!("DELETE FROM poll_votes WHERE poll_id = ANY($1)", &polls)
        .execute(&mut *tx)
        .await?;
    sqlx::query!("DELETE FROM polls WHERE status_id = $1", id)
        .execute(&mut *tx)
        .await?;

    // `has_one :quote, dependent: :destroy`.
    let quote = sqlx::query!(
        "SELECT id, state, quoted_status_id FROM quotes WHERE status_id = $1",
        id
    )
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(quote) = &quote {
        touched.extend(destroy_notifications(&mut tx, "Quote", &[quote.id], true).await?);
        sqlx::query!("DELETE FROM quotes WHERE id = $1", quote.id)
            .execute(&mut *tx)
            .await?;
    }

    // `has_many :edits, dependent: :destroy`.
    sqlx::query!("DELETE FROM status_edits WHERE status_id = $1", id)
        .execute(&mut *tx)
        .await?;

    let deleted = sqlx::query!("DELETE FROM statuses WHERE id = $1", id)
        .execute(&mut *tx)
        .await?
        .rows_affected();

    touched.sort_unstable();
    touched.dedup();
    for (account_id, from_account_id) in touched {
        reconsider_notification_request(&mut tx, account_id, from_account_id).await?;
    }
    tx.commit().await?;
    if deleted == 0 {
        return Ok(());
    }

    // `after_destroy_commit :decrement_counter_caches`.
    if let Err(error) = crate::counters::on_status_destroyed(
        &state.db,
        status.account_id,
        status.visibility,
        status.reblog_of_id,
        status.in_reply_to_id,
    )
    .await
    {
        tracing::error!(status_id = id, %error, "could not uncount a destroyed status");
    }
    // The quote's `after_destroy_commit :decrement_counter_caches!`, on a
    // quoted status still there (`belongs_to` under the default scope).
    if let Some(quote) = quote.filter(|q| q.state == crate::db::models::quote_state::ACCEPTED) {
        if let Some(quoted_id) = quote.quoted_status_id {
            sqlx::query!(
                r#"UPDATE status_stats SET quotes_count = GREATEST(quotes_count - 1, 0), updated_at = now()
                   WHERE status_id = $1
                     AND EXISTS (SELECT 1 FROM statuses WHERE id = $1 AND deleted_at IS NULL)"#,
                quoted_id,
            )
            .execute(&state.db)
            .await?;
        }
    }

    // `update_index`, which deletes what is gone.
    crate::search::elasticsearch::indexing::status(state, id).await;
    if let Some(original_id) = status.reblog_of_id {
        crate::search::elasticsearch::indexing::status(state, original_id).await;
    }
    crate::search::elasticsearch::indexing::account(state, status.account_id).await;
    Ok(())
}

/// `has_one :notification, as: :activity, dependent: :destroy` (`one`) and
/// `has_many :notifications, ...` for the given activities. `has_one` loads
/// one notification per activity and destroys that one. Returns the
/// recipient and sender of each destroyed notification.
async fn destroy_notifications(
    conn: &mut PgConnection,
    activity_type: &str,
    activity_ids: &[i64],
    one: bool,
) -> Result<Vec<(i64, i64)>> {
    if activity_ids.is_empty() {
        return Ok(vec![]);
    }
    let rows = if one {
        sqlx::query!(
            r#"DELETE FROM notifications WHERE id IN (
                 SELECT DISTINCT ON (activity_id) id FROM notifications
                 WHERE activity_type = $1 AND activity_id = ANY($2)
                 ORDER BY activity_id, id)
               RETURNING account_id, from_account_id"#,
            activity_type,
            activity_ids,
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|r| (r.account_id, r.from_account_id))
        .collect()
    } else {
        sqlx::query!(
            r#"DELETE FROM notifications WHERE activity_type = $1 AND activity_id = ANY($2)
               RETURNING account_id, from_account_id"#,
            activity_type,
            activity_ids,
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|r| (r.account_id, r.from_account_id))
        .collect()
    };
    Ok(rows)
}

/// `NotificationRequest#reconsider_existence!`: the request from
/// `from_account_id` to `account_id`, if there is one, recounted
/// (`prepare_notifications_count`) and kept while something is left in it.
async fn reconsider_notification_request(
    conn: &mut PgConnection,
    account_id: i64,
    from_account_id: i64,
) -> Result<()> {
    let count: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM (
             SELECT 1 FROM notifications
             WHERE account_id = $1 AND from_account_id = $2 AND filtered
               AND "type" IN ('mention', 'quote')
             LIMIT 100) n"#,
        account_id,
        from_account_id,
    )
    .fetch_one(&mut *conn)
    .await?;
    if count > 0 {
        sqlx::query!(
            r#"UPDATE notification_requests SET notifications_count = $3, updated_at = now()
               WHERE account_id = $1 AND from_account_id = $2 AND notifications_count <> $3"#,
            account_id,
            from_account_id,
            count,
        )
        .execute(&mut *conn)
        .await?;
    } else {
        sqlx::query!(
            "DELETE FROM notification_requests WHERE account_id = $1 AND from_account_id = $2",
            account_id,
            from_account_id,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// `Status#unlink_from_conversations!`: `AccountConversation.remove_status`
/// for each local account the direct status mentions and its author when
/// local. Each finds its conversation by the status's participants as that
/// account sees them, takes the status out, and goes when it is left empty
/// or is saved, and streamed, otherwise.
async fn unlink_from_conversations(
    state: &AppState,
    status: &Status,
    account: &Account,
) -> Result<()> {
    let mut owners: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT m.account_id FROM mentions m JOIN accounts a ON a.id = m.account_id
           WHERE m.status_id = $1 AND a.domain IS NULL ORDER BY m.id"#,
        status.id
    )
    .fetch_all(&state.db)
    .await?;
    if account.is_local() {
        owners.push(account.id);
    }
    // `participants_from_status`: the active mentions and the author, less
    // the owner.
    let mut participants: Vec<i64> = sqlx::query_scalar!(
        "SELECT account_id FROM mentions WHERE status_id = $1 AND NOT silent",
        status.id
    )
    .fetch_all(&state.db)
    .await?;
    participants.push(status.account_id);
    participants.sort_unstable();
    participants.dedup();

    for owner in owners {
        let others: Vec<i64> = participants
            .iter()
            .copied()
            .filter(|id| *id != owner)
            .collect();
        let Some(conversation) = sqlx::query!(
            r#"SELECT id, status_ids FROM account_conversations
               WHERE account_id = $1 AND conversation_id IS NOT DISTINCT FROM $2
                 AND participant_account_ids = $3
               LIMIT 1"#,
            owner,
            status.conversation_id,
            &others,
        )
        .fetch_optional(&state.db)
        .await?
        else {
            continue;
        };
        let mut ids: Vec<i64> = conversation
            .status_ids
            .into_iter()
            .filter(|id| *id != status.id)
            .collect();
        if ids.is_empty() {
            sqlx::query!(
                "DELETE FROM account_conversations WHERE id = $1",
                conversation.id
            )
            .execute(&state.db)
            .await?;
        } else {
            // `set_last_status`.
            ids.sort_unstable();
            sqlx::query!(
                r#"UPDATE account_conversations
                   SET status_ids = $2, last_status_id = $3, lock_version = lock_version + 1
                   WHERE id = $1"#,
                conversation.id,
                &ids,
                ids.last().copied(),
            )
            .execute(&state.db)
            .await?;
            crate::api::mastodon::conversations::push_to_streaming(state, conversation.id).await;
        }
    }
    Ok(())
}
