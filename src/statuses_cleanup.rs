//! Automated post deletion: Mastodon's `AccountStatusesCleanupPolicy`,
//! `AccountStatusesCleanupService` and `Scheduler::AccountsStatusesCleanupScheduler`.
//!
//! A local account with an enabled row in `account_statuses_cleanup_policies`
//! has its posts deleted once they are older than the policy's
//! `min_status_age`, except those the policy keeps: direct posts, pinned posts,
//! polls, posts with media, posts the author favourited or bookmarked, and
//! posts with at least `min_favs` favourites or `min_reblogs` boosts. Mastodon
//! edits the policy in its web settings (`/statuses_cleanup`), not through its
//! REST API, and eunha has no page for it yet; a policy saved by Mastodon
//! against the same database is carried out all the same.
//!
//! Every minute the scheduler deletes up to a budget of posts across the
//! accounts, a few per account at a time, starting after the policy it stopped
//! at last time, and skips its turn while the job queues are behind.

use std::time::Duration;

use redis::AsyncCommands;

use crate::db::models::Status;
use crate::state::AppState;

/// The scheduler runs every minute.
pub const EVERY: Duration = Duration::from_secs(60);

/// `MAX_BUDGET`: at most this many posts deleted per run.
const MAX_BUDGET: i64 = 300;
/// `PER_ACCOUNT_BUDGET`: at most this many of one account's at a time.
const PER_ACCOUNT_BUDGET: i64 = 5;
/// `PER_THREAD_BUDGET`: this many per thread that runs jobs.
const PER_THREAD_BUDGET: i64 = 5;
/// `LOAD_LATENCY_THRESHOLDS`, in seconds: a queue whose oldest due job has
/// waited longer than this means the instance is under load, and the run is
/// skipped.
const LOAD_LATENCY_THRESHOLDS: [(&str, f64); 3] =
    [("default", 5.0), ("push", 10.0), ("pull", 5.0 * 60.0)];

/// `AccountStatusesCleanupPolicy::EARLY_SEARCH_CUTOFF`.
const EARLY_SEARCH_CUTOFF: i64 = 5_000;
/// How long `record_last_inspected` keeps its mark: two weeks.
const LAST_INSPECTED_TTL_SECS: u64 = 14 * 24 * 60 * 60;
/// How long the scheduler remembers where it stopped: an hour.
const LAST_POLICY_TTL_SECS: u64 = 60 * 60;
/// sidekiq-unique-jobs' `lock_ttl: 1.day`, for the run's lock.
const LOCK_TTL_MS: usize = 24 * 60 * 60 * 1000;

/// `Status` visibility `direct`.
const DIRECT: i32 = 3;

/// A row of `account_statuses_cleanup_policies`.
#[derive(Debug, Clone)]
pub struct Policy {
    pub id: i64,
    pub account_id: i64,
    pub enabled: bool,
    pub min_status_age: i32,
    pub keep_direct: bool,
    pub keep_pinned: bool,
    pub keep_polls: bool,
    pub keep_media: bool,
    pub keep_self_fav: bool,
    pub keep_self_bookmark: bool,
    pub min_favs: Option<i32>,
    pub min_reblogs: Option<i32>,
}

/// `Mastodon::Snowflake.id_at(time, with_random: false)`.
fn id_at(time: chrono::DateTime<chrono::Utc>) -> i64 {
    time.timestamp_millis() << 16
}

fn last_inspected_key(state: &AppState, account_id: i64) -> String {
    state
        .redis_keys
        .key(format!("account_cleanup:{account_id}"))
}

fn last_policy_key(state: &AppState) -> String {
    state
        .redis_keys
        .key("account_statuses_cleanup_scheduler:last_policy_id")
}

impl Policy {
    /// `old_enough_scope`'s snowflake: the newest id a post old enough to
    /// go can have.
    fn old_enough_id(&self) -> i64 {
        id_at(chrono::Utc::now() - chrono::Duration::seconds(i64::from(self.min_status_age)))
    }

    /// `last_inspected`: every post of the account older than this is one
    /// the policy has already decided to keep.
    pub async fn last_inspected(&self, state: &AppState) -> Option<i64> {
        let mut redis = state.redis.clone();
        redis
            .get::<_, Option<String>>(last_inspected_key(state, self.account_id))
            .await
            .ok()
            .flatten()
            .and_then(|value| value.parse().ok())
    }

    /// `record_last_inspected`.
    pub async fn record_last_inspected(&self, state: &AppState, last_id: i64) {
        let mut redis = state.redis.clone();
        let _: redis::RedisResult<()> = redis
            .set_ex(
                last_inspected_key(state, self.account_id),
                last_id,
                LAST_INSPECTED_TTL_SECS,
            )
            .await;
    }

    /// `compute_cutoff_id`: the newest post old enough to go among the
    /// `EARLY_SEARCH_CUTOFF` after the last one inspected, so that an
    /// account with many posts the policy keeps is searched a slice at a
    /// time. `None` when there are none.
    pub async fn compute_cutoff_id(&self, state: &AppState) -> anyhow::Result<Option<i64>> {
        let min_id = self.last_inspected(state).await.unwrap_or(0);
        let max_id = self.old_enough_id();
        Ok(sqlx::query_scalar!(
            r#"SELECT max(id) FROM (
                 SELECT id FROM statuses
                 WHERE deleted_at IS NULL AND account_id = $1 AND id BETWEEN $2 AND $3
                 ORDER BY id ASC LIMIT $4
               ) t"#,
            self.account_id,
            min_id,
            max_id,
            EARLY_SEARCH_CUTOFF,
        )
        .fetch_one(&state.db)
        .await?)
    }

    /// `statuses_to_delete(limit, max_id, min_id)`: the oldest `limit` of
    /// the account's posts the policy does not keep, up to `max_id` and from
    /// `min_id`.
    pub async fn statuses_to_delete(
        &self,
        state: &AppState,
        limit: i64,
        max_id: Option<i64>,
        min_id: Option<i64>,
    ) -> anyhow::Result<Vec<i64>> {
        let snowflake = self.old_enough_id();
        let max_id = max_id.map_or(snowflake, |max| max.min(snowflake));
        Ok(sqlx::query_scalar!(
            r#"SELECT s.id FROM statuses s
               LEFT JOIN status_stats ss ON ss.status_id = s.id
               WHERE s.deleted_at IS NULL AND s.account_id = $1
                 AND s.id <= $2
                 AND ($3::bigint IS NULL OR s.id >= $3)
                 AND ($4::int IS NULL OR COALESCE(ss.reblogs_count, 0) < $4)
                 AND ($5::int IS NULL OR COALESCE(ss.favourites_count, 0) < $5)
                 AND (NOT $6 OR s.visibility <> $13)
                 AND (NOT $7 OR NOT EXISTS (
                   SELECT 1 FROM status_pins p
                   WHERE p.account_id = s.account_id AND p.status_id = s.id))
                 AND (NOT $8 OR s.poll_id IS NULL)
                 AND (NOT $9 OR NOT EXISTS (
                   SELECT 1 FROM media_attachments m WHERE m.status_id = s.id))
                 AND (NOT $10 OR NOT EXISTS (
                   SELECT 1 FROM favourites f
                   WHERE f.account_id = s.account_id AND f.status_id = s.id))
                 AND (NOT $11 OR NOT EXISTS (
                   SELECT 1 FROM bookmarks b
                   WHERE b.account_id = s.account_id AND b.status_id = s.id))
               ORDER BY s.id ASC LIMIT $12"#,
            self.account_id,
            max_id,
            min_id,
            self.min_reblogs,
            self.min_favs,
            self.keep_direct,
            self.keep_pinned,
            self.keep_polls,
            self.keep_media,
            self.keep_self_fav,
            self.keep_self_bookmark,
            limit,
            DIRECT,
        )
        .fetch_all(&state.db)
        .await?)
    }
}

/// The policy of `account_id`, if it has one.
pub async fn find_policy(state: &AppState, account_id: i64) -> anyhow::Result<Option<Policy>> {
    Ok(sqlx::query_as!(
        Policy,
        r#"SELECT id, account_id, enabled, min_status_age, keep_direct, keep_pinned,
                  keep_polls, keep_media, keep_self_fav, keep_self_bookmark, min_favs,
                  min_reblogs
           FROM account_statuses_cleanup_policies WHERE account_id = $1"#,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?)
}

/// Why a post's mark is moved back: the author took back the favourite,
/// bookmark or pin that may have kept it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Undone {
    Unfav,
    Unbookmark,
    Unpin,
}

/// `Favourite`, `Bookmark` and `StatusPin`'s `invalidate_cleanup_info`, run
/// once one of them is destroyed: when a local account takes back its own
/// favourite, bookmark or pin of its own post, and its policy kept posts for
/// that, the post is no longer known to be kept, and the policy's mark moves
/// back to it (`invalidate_last_inspected`).
pub async fn invalidate_cleanup_info(
    state: &AppState,
    account_id: i64,
    status_id: i64,
    undone: Undone,
) {
    let owner = sqlx::query_scalar!(
        r#"SELECT s.account_id FROM statuses s JOIN accounts a ON a.id = $2
           WHERE s.id = $1 AND a.domain IS NULL"#,
        status_id,
        account_id,
    )
    .fetch_optional(&state.db)
    .await;
    if !matches!(owner, Ok(Some(owner)) if owner == account_id) {
        return;
    }
    let Ok(Some(policy)) = find_policy(state, account_id).await else {
        return;
    };
    let Some(last) = policy.last_inspected(state).await else {
        return;
    };
    if status_id > last {
        return;
    }
    let kept_for_it = match undone {
        Undone::Unbookmark => policy.keep_self_bookmark,
        Undone::Unfav => policy.keep_self_fav,
        Undone::Unpin => policy.keep_pinned,
    };
    if kept_for_it {
        policy.record_last_inspected(state, status_id).await;
    }
}

/// `AccountStatusesCleanupService#call`: delete up to `budget` of the posts
/// `policy` does not keep, oldest first, and move its mark past them.
/// Returns how many were deleted.
pub async fn clean(state: &AppState, policy: &Policy, budget: i64) -> anyhow::Result<i64> {
    if !policy.enabled {
        return Ok(0);
    }
    let Some(cutoff_id) = policy.compute_cutoff_id(state).await? else {
        return Ok(0);
    };
    let last_inspected = policy.last_inspected(state).await;
    let ids = policy
        .statuses_to_delete(state, budget, Some(cutoff_id), last_inspected)
        .await?;

    let mut deleted = 0;
    let mut last_deleted = None;
    if !ids.is_empty() {
        for id in ids {
            let Some(status) = sqlx::query_as!(
                Status,
                "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
                id
            )
            .fetch_optional(&state.db)
            .await?
            else {
                continue;
            };
            // `status.discard_with_reblogs`, then
            // `RemovalWorker.perform_async(status.id, { 'redraft' => false })`.
            crate::remove_status::discard_with_reblogs(state, &status).await?;
            crate::jobs::push(
                state,
                crate::remove_status::RemovalWorker {
                    status_id: status.id,
                    options: crate::remove_status::Options::default(),
                },
            )
            .await;
            deleted += 1;
            last_deleted = Some(id);
        }
    }
    policy
        .record_last_inspected(state, last_deleted.unwrap_or(cutoff_id))
        .await;
    Ok(deleted)
}

/// Run the scheduler every minute for as long as the instance runs, the
/// first pass a minute in.
pub async fn run(state: AppState) {
    loop {
        crate::background::rest(&state.stop, EVERY).await;
        if state.stop.is_cancelled() {
            break;
        }
        if let Err(error) = perform(&state).await {
            tracing::error!(%error, "account statuses cleanup failed");
        }
    }
}

/// `compute_budget`: five posts for each thread that runs jobs, Sidekiq's
/// `concurrency`, here `[workers] job_workers × job_concurrency`, but no
/// more than `MAX_BUDGET`.
pub fn compute_budget(state: &AppState) -> i64 {
    let workers = state.config.workers.sanitized();
    let threads = i64::try_from(workers.job_workers * workers.job_concurrency).unwrap_or(i64::MAX);
    PER_THREAD_BUDGET.saturating_mul(threads).min(MAX_BUDGET)
}

/// How long the oldest job due in `queue` has waited, in seconds, as
/// `Sidekiq::Queue#latency`. `push` is the delivery queue.
async fn latency(state: &AppState, queue: &str) -> anyhow::Result<f64> {
    let latency = if queue == "push" {
        sqlx::query_scalar!(
            r#"SELECT extract(epoch FROM now() - min(run_at))::float8 FROM eunha.ojak_queue
               WHERE queue = 'delivery' AND failed_at IS NULL AND run_at <= now()"#
        )
        .fetch_one(&state.db)
        .await?
    } else {
        sqlx::query_scalar!(
            r#"SELECT extract(epoch FROM now() - min(run_at))::float8 FROM eunha.jobs
               WHERE queue = $1 AND dead_at IS NULL AND locked_at IS NULL AND run_at <= now()"#,
            queue,
        )
        .fetch_one(&state.db)
        .await?
    };
    Ok(latency.unwrap_or(0.0))
}

/// `under_load?`.
pub async fn under_load(state: &AppState) -> anyhow::Result<bool> {
    for (queue, max_latency) in LOAD_LATENCY_THRESHOLDS {
        if latency(state, queue).await? > max_latency {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn last_processed_id(state: &AppState) -> Option<i64> {
    let mut redis = state.redis.clone();
    redis
        .get::<_, Option<String>>(last_policy_key(state))
        .await
        .ok()
        .flatten()
        .and_then(|value| value.parse().ok())
}

async fn save_last_processed_id(state: &AppState, id: i64) {
    let mut redis = state.redis.clone();
    let _: redis::RedisResult<()> = redis
        .set_ex(last_policy_key(state), id, LAST_POLICY_TTL_SECS)
        .await;
}

/// `cleanup_policies`: which enabled policies a pass looks at.
#[derive(Debug, Clone, PartialEq)]
enum Scope {
    /// The first pass: those from where the last run stopped.
    From(i64),
    /// The second: those up to it, and those that have yielded posts.
    UpToOr(i64, Vec<i64>),
    /// Every pass after: only those that have yielded posts.
    Only(Vec<i64>),
}

impl Scope {
    fn new(first_policy_id: i64, affected: &[i64], first: bool, full: bool) -> Self {
        match (full, first) {
            (true, true) => Self::From(first_policy_id),
            (true, false) => Self::UpToOr(first_policy_id, affected.to_vec()),
            (false, _) => Self::Only(affected.to_vec()),
        }
    }
}

/// One batch of `scope`'s policies after `after`, by id, as `find_each`
/// reads them.
async fn policies(
    state: &AppState,
    scope: &Scope,
    after: i64,
    batch: i64,
) -> anyhow::Result<Vec<Policy>> {
    let (from, up_to, ids): (Option<i64>, Option<i64>, Option<&[i64]>) = match scope {
        Scope::From(id) => (Some(*id), None, None),
        Scope::UpToOr(id, ids) => (None, Some(*id), Some(ids)),
        Scope::Only(ids) => (None, None, Some(ids)),
    };
    let ids = ids.unwrap_or(&[]);
    Ok(sqlx::query_as!(
        Policy,
        r#"SELECT id, account_id, enabled, min_status_age, keep_direct, keep_pinned,
                  keep_polls, keep_media, keep_self_fav, keep_self_bookmark, min_favs,
                  min_reblogs
           FROM account_statuses_cleanup_policies
           WHERE enabled AND id > $1
             AND CASE
                   WHEN $2::bigint IS NOT NULL THEN id >= $2
                   WHEN $3::bigint IS NOT NULL THEN id <= $3 OR id = ANY($4)
                   ELSE id = ANY($4)
                 END
           ORDER BY id ASC LIMIT $5"#,
        after,
        from,
        up_to,
        ids,
        batch,
    )
    .fetch_all(&state.db)
    .await?)
}

/// `AccountsStatusesCleanupScheduler#perform`. It runs once at a time across
/// the processes serving the instance, as its `until_executed` lock has it.
pub async fn perform(state: &AppState) -> anyhow::Result<()> {
    let Some(_lock) = crate::redis_lock::try_acquire(
        state,
        "account_statuses_cleanup_scheduler:lock",
        LOCK_TTL_MS,
    )
    .await
    else {
        return Ok(());
    };
    if under_load(state).await? {
        return Ok(());
    }
    let mut budget = compute_budget(state);

    // If the budget allows it, every enabled policy is looked at once, from
    // where the last run stopped round to it again, noting the ones that
    // deleted something; then only those are, until the budget is spent or
    // none of them deletes anything more.
    let first_policy_id = last_processed_id(state).await.unwrap_or(0);
    let mut first_iteration = true;
    let mut full_iteration = true;
    let mut affected: Vec<i64> = Vec::new();

    loop {
        let mut processed_accounts = 0;
        let scope = Scope::new(first_policy_id, &affected, first_iteration, full_iteration);
        let mut after = i64::MIN;
        'pass: loop {
            let batch = policies(state, &scope, after, 1000).await?;
            let Some(last) = batch.last() else { break };
            after = last.id;
            for policy in batch {
                let deleted = clean(state, &policy, budget.min(PER_ACCOUNT_BUDGET)).await?;
                budget -= deleted;
                if deleted != 0 {
                    processed_accounts += 1;
                    if full_iteration {
                        affected.push(policy.id);
                    }
                }
                if !first_iteration && policy.id >= first_policy_id {
                    full_iteration = false;
                }
                if budget == 0 {
                    save_last_processed_id(state, policy.id).await;
                    break 'pass;
                }
            }
        }

        if budget == 0 || (processed_accounts == 0 && !full_iteration) {
            break;
        }
        if !first_iteration {
            full_iteration = false;
        }
        first_iteration = false;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Scope;

    #[test]
    fn passes_widen_then_narrow_to_the_policies_that_yielded() {
        assert_eq!(Scope::new(7, &[], true, true), Scope::From(7));
        assert_eq!(Scope::new(7, &[9], false, true), Scope::UpToOr(7, vec![9]));
        assert_eq!(Scope::new(7, &[9], false, false), Scope::Only(vec![9]));
    }
}
