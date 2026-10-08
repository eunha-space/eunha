//! Denormalized account counter maintenance (`account_stats`).
//!
//! Mirrors Mastodon's `Account::Counters`: the follower/following totals are
//! kept as running counts that are bumped on follow create/remove, guarded so
//! decrements never fall below zero, and reconcilable from the source-of-truth
//! `follows` table when they drift (Mastodon's `Account#refresh_counts`, run by
//! `tootctl accounts refresh`).
//!
//! `follower` is the account doing the following; `target` is the account being
//! followed. A follow bumps `target.followers_count` and `follower.following_count`.
//!
//! Status counters live here for a reason. Mastodon keeps them in
//! `Status#increment_counter_caches`, a model callback that runs however the
//! status came to exist — posted through the API, published from a schedule, or
//! arrived in an inbox. eunha has no such callback, so every path had to
//! remember, and the ones that arrived over ActivityPub did not: replies from
//! other instances went uncounted, so did their authors' posts, their boosts,
//! and their follows. Each was fixed where it was found, which is a poor way to
//! be sure there are no others.
//!
//! So the rules are written once, here, and the call sites say only *what
//! happened*. A new path that creates a status calls [`on_status_created`] and
//! gets the conditions for free; one that does not call it is visibly missing a
//! line rather than silently missing a rule.

use sqlx::PgPool;

use crate::db::models::vis;

/// Record a new follow edge: increment the target's `followers_count` and the
/// follower's `following_count`, creating the `account_stats` row if absent.
/// Call after the `follows` row is inserted.
pub async fn on_follow_created(db: &PgPool, follower: i64, target: i64) -> sqlx::Result<()> {
    sqlx::query!(
        "INSERT INTO account_stats (account_id, followers_count, created_at, updated_at)
         VALUES ($1, 1, now(), now())
         ON CONFLICT (account_id) DO UPDATE
           SET followers_count = account_stats.followers_count + 1, updated_at = now()",
        target,
    )
    .execute(db)
    .await?;
    sqlx::query!(
        "INSERT INTO account_stats (account_id, following_count, created_at, updated_at)
         VALUES ($1, 1, now(), now())
         ON CONFLICT (account_id) DO UPDATE
           SET following_count = account_stats.following_count + 1, updated_at = now()",
        follower,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `Follow`'s `after_destroy` callbacks, which every path that deletes a
/// `follows` row runs: `remove_endorsements`, the follower's endorsement of
/// the target deleted, then `decrement_cache_counters`, the target's
/// `followers_count` and the follower's `following_count` decremented,
/// floored at 0. Call only when a `follows` row was actually deleted, so
/// idempotent unfollows don't over-decrement.
pub async fn on_follow_removed(db: &PgPool, follower: i64, target: i64) -> sqlx::Result<()> {
    sqlx::query!(
        "DELETE FROM account_pins WHERE account_id = $1 AND target_account_id = $2",
        follower,
        target,
    )
    .execute(db)
    .await?;
    sqlx::query!(
        "UPDATE account_stats SET followers_count = GREATEST(followers_count - 1, 0), updated_at = now()
         WHERE account_id = $1",
        target,
    )
    .execute(db)
    .await?;
    sqlx::query!(
        "UPDATE account_stats SET following_count = GREATEST(following_count - 1, 0), updated_at = now()
         WHERE account_id = $1",
        follower,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Recompute a **local** account's follow counters from the `follows` table,
/// matching Mastodon's `Account#refresh_counts`. Reconciles rows that have
/// drifted from the source of truth (including any left negative by legacy code
/// paths).
///
/// Local-only by design: for remote actors the `follows` table holds only the
/// edges involving local accounts, while `account_stats` mirrors the remote
/// instance's true totals (seeded at import). Recounting a remote account from
/// our partial view would clobber those totals, so this no-ops for them.
pub async fn recount_follows(db: &PgPool, account_id: i64) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE account_stats s
         SET followers_count = (SELECT COUNT(*) FROM follows WHERE target_account_id = s.account_id),
             following_count = (SELECT COUNT(*) FROM follows WHERE account_id = s.account_id),
             updated_at = now()
         FROM accounts a
         WHERE a.id = s.account_id AND a.domain IS NULL AND s.account_id = $1",
        account_id,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// A status came into existence, however it got here.
///
/// Mastodon's `Status#increment_counter_caches`, which is one method with two
/// conditions worth stating plainly:
///
/// * `return if direct_visibility?` — a direct message moves no counter at all.
///   The post count is on a profile anyone can read, and one that climbed
///   whenever an account sent a private message would report that it had.
/// * the parent's reply count moves only `if in_reply_to_id.present? &&
///   distributable?` — a followers-only or direct reply leaves it alone, or the
///   number visible to everyone would announce that someone replied privately.
///
/// `created_at` advances `last_status_at`, which only ever moves forward.
///
/// Call once, after the status row is committed, and only when the row was
/// actually new — federation redelivers, and a second call counts twice.
pub async fn on_status_created(
    db: &PgPool,
    account_id: i64,
    visibility: i32,
    in_reply_to_id: Option<i64>,
    created_at: chrono::NaiveDateTime,
) -> sqlx::Result<()> {
    if !vis::counted(visibility) {
        return Ok(());
    }

    sqlx::query!(
        "INSERT INTO account_stats (account_id, statuses_count, last_status_at, created_at, updated_at)
         VALUES ($1, 1, $2, now(), now())
         ON CONFLICT (account_id) DO UPDATE
           SET statuses_count = account_stats.statuses_count + 1,
               last_status_at = GREATEST(account_stats.last_status_at, $2),
               updated_at = now()",
        account_id,
        created_at,
    )
    .execute(db)
    .await?;

    if let Some(parent_id) = in_reply_to_id.filter(|_| vis::distributable(visibility)) {
        sqlx::query!(
            "INSERT INTO status_stats (status_id, replies_count, created_at, updated_at)
             VALUES ($1, 1, now(), now())
             ON CONFLICT (status_id) DO UPDATE
               SET replies_count = status_stats.replies_count + 1, updated_at = now()",
            parent_id,
        )
        .execute(db)
        .await?;
    }
    Ok(())
}

/// A status was destroyed: `Status#decrement_counter_caches`, its
/// `after_destroy_commit`.
///
/// The mirror of [`on_status_created`] under the same conditions, so that what
/// was never counted is never subtracted — a direct message that lowered the
/// post count on deletion would walk it down one DM at a time. A discarded
/// status is still counted: Mastodon uncounts a status only when its row goes,
/// which for one kept for moderators is when the daily cleanup purges it
/// ([`crate::remove_status`]).
///
/// The boosted and replied-to statuses are `belongs_to` associations under
/// `Status`'s default scope, so one already discarded is not found, and its
/// count is left alone. A boost of a remote status takes one off the count
/// its server reported too (`untrusted_reblogs_count`), when there is one.
///
/// Call only when a row was actually removed.
pub async fn on_status_destroyed(
    db: &PgPool,
    account_id: i64,
    visibility: i32,
    reblog_of_id: Option<i64>,
    in_reply_to_id: Option<i64>,
) -> sqlx::Result<()> {
    if !vis::counted(visibility) {
        return Ok(());
    }

    sqlx::query!(
        "UPDATE account_stats
         SET statuses_count = GREATEST(statuses_count - 1, 0), updated_at = now()
         WHERE account_id = $1",
        account_id,
    )
    .execute(db)
    .await?;

    if let Some(original_id) = reblog_of_id {
        sqlx::query!(
            r#"UPDATE status_stats ss
               SET reblogs_count = GREATEST(ss.reblogs_count - 1, 0),
                   untrusted_reblogs_count = CASE
                     WHEN ss.untrusted_reblogs_count IS NULL OR s.local OR s.uri IS NULL
                       THEN ss.untrusted_reblogs_count
                     ELSE GREATEST(ss.untrusted_reblogs_count - 1, 0) END,
                   updated_at = now()
               FROM statuses s
               WHERE ss.status_id = $1 AND s.id = ss.status_id AND s.deleted_at IS NULL"#,
            original_id,
        )
        .execute(db)
        .await?;
    }

    if let Some(parent_id) = in_reply_to_id.filter(|_| vis::distributable(visibility)) {
        sqlx::query!(
            "UPDATE status_stats
             SET replies_count = GREATEST(replies_count - 1, 0), updated_at = now()
             WHERE status_id = $1
               AND EXISTS (SELECT 1 FROM statuses WHERE id = $1 AND deleted_at IS NULL)",
            parent_id,
        )
        .execute(db)
        .await?;
    }
    Ok(())
}
