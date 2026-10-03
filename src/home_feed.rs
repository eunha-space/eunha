//! Mastodon's `HomeFeed` regeneration, and the sign-in tracking that decides
//! when a feed is regenerated.
//!
//! A home feed is kept in Redis only for users who signed in recently
//! (`User::ACTIVE_DURATION`, seven days): the fan-out skips everyone else, and
//! the daily vacuum (`Vacuum::FeedsVacuum`) removes their feeds. A user who
//! comes back is noticed by `User#update_sign_in!`
//! ([`crate::middleware::update_sign_in`]), which `UserTrackingConcern` runs
//! at most once a day per user; when the sign-in before was longer ago than
//! the active duration, `User#regenerate_feed!` marks the feed regenerating
//! and queues `RegenerationWorker` to rebuild it and the user's lists.
//!
//! While the feed is regenerating — a hash `account:<id>:regeneration` in
//! the shape of an [`AsyncRefresh`] — `GET /api/v1/timelines/home` answers
//! `206 Partial Content` with what the feed already holds and a
//! `Mastodon-Async-Refresh` header with `retry=5`. `FollowService` marks it so
//! too when an account that follows no one makes its first follow, and the
//! merge of the followed account's posts (`MergeWorker`) finishes it.
//!
//! Nothing else regenerates a feed: reading one that is empty, or that Redis
//! lost, answers what it holds, as Mastodon's does. The merges and
//! unmerges a follow, an unfollow, an unmute or a list change queue are
//! [`MergeWorker`] and [`UnmergeWorker`].

use crate::async_refresh::{self, AsyncRefresh};
use crate::state::AppState;

/// `User::ACTIVE_DURATION` (`USER_ACTIVE_DAYS`, seven days by default).
pub const ACTIVE_DAYS: i32 = 7;

/// The `retry_seconds` the home timeline's async refresh header carries.
pub const RETRY_SECONDS: u32 = 5;

/// `HomeFeed#redis_regeneration_key`, before the instance's prefix.
pub fn regeneration_key(account_id: i64) -> String {
    format!("account:{account_id}:regeneration")
}

/// `HomeFeed#upgrade_redis_key!`'s test: whether `key` held something other
/// than a hash — a string, as Mastodon kept it before regenerations became
/// async refreshes — and so was deleted. Mastodon asks `TYPE` after a command
/// failed; reading `HGET`'s `WRONGTYPE` keeps to the commands tenants are
/// granted.
async fn delete_if_not_hash(state: &AppState, key: &str) -> bool {
    let mut redis = state.redis_coordination.clone();
    let redis_key = state.redis_keys.key(key);
    let probe: redis::RedisResult<Option<String>> = redis::cmd("HGET")
        .arg(&redis_key)
        .arg("status")
        .query_async(&mut redis)
        .await;
    if matches!(&probe, Err(error) if error.code() == Some("WRONGTYPE")) {
        let _: redis::RedisResult<()> = redis::cmd("DEL")
            .arg(&redis_key)
            .query_async(&mut redis)
            .await;
        return true;
    }
    false
}

/// `HomeFeed#async_refresh`. A key left as a string is replaced with a
/// running refresh, as `HomeFeed#upgrade_redis_key!` does.
pub async fn async_refresh(state: &AppState, account_id: i64) -> AsyncRefresh {
    let key = regeneration_key(account_id);
    if delete_if_not_hash(state, &key).await {
        return AsyncRefresh::create(state, &key, false).await;
    }
    AsyncRefresh::new(state, &key).await
}

/// `HomeFeed#regenerating?`.
pub async fn regenerating(state: &AppState, account_id: i64) -> bool {
    async_refresh(state, account_id).await.is_running()
}

/// `HomeFeed#regeneration_in_progress!`.
pub async fn regeneration_in_progress(state: &AppState, account_id: i64) {
    let key = regeneration_key(account_id);
    delete_if_not_hash(state, &key).await;
    AsyncRefresh::create(state, &key, false).await;
}

/// `HomeFeed#regeneration_finished!`. Like `AsyncRefresh#finish!`, it leaves
/// a finished refresh for an hour even where none was running.
pub async fn regeneration_finished(state: &AppState, account_id: i64) {
    let key = regeneration_key(account_id);
    delete_if_not_hash(state, &key).await;
    async_refresh::finish(state, &key).await;
}

/// `User#regenerate_feed!`: unless the feed is already regenerating, mark it
/// so and rebuild it in the background.
pub async fn regenerate_feed(state: &AppState, account_id: i64) {
    if regenerating(state, account_id).await {
        return;
    }
    regeneration_in_progress(state, account_id).await;
    enqueue_regeneration(state, account_id).await;
}

/// `RegenerationWorker.perform_async(account_id)`, or the worker at once when
/// the tests ask for background work inline ([`crate::feed::sync_fanout`]).
pub async fn enqueue_regeneration(state: &AppState, account_id: i64) {
    if crate::feed::sync_fanout() {
        regenerate(state, account_id).await;
    } else {
        crate::jobs::push(state, RegenerationWorker { account_id }).await;
    }
}

/// `RegenerationWorker`, with its `lock: :until_executed`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegenerationWorker {
    pub account_id: i64,
}

impl crate::jobs::Job for RegenerationWorker {
    const KIND: &'static str = "RegenerationWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT.lock(
        crate::jobs::Lock::UntilExecuted(crate::jobs::DEFAULT_LOCK_TTL),
    );

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        regenerate(state, self.account_id).await;
        Ok(())
    }
}

/// Whether the account is there: `Account.find`, whose `RecordNotFound` the
/// feed workers swallow.
async fn account_exists(state: &AppState, account_id: i64) -> bool {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM accounts WHERE id = $1) AS "e!""#,
        account_id
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(false)
}

/// `RegenerationWorker#perform`, which is `PrecomputeFeedService#call`: fill
/// the home feed and each of the account's lists, then finish the
/// regeneration however that went.
pub async fn regenerate(state: &AppState, account_id: i64) {
    if !account_exists(state, account_id).await {
        return;
    }
    let finish = async_refresh::FinishOnDrop::new(state, &regeneration_key(account_id));
    let mut redis = state.redis.clone();
    crate::feed::populate_home(&mut redis, &state.redis_keys, &state.db, account_id).await;
    let lists: Vec<i64> =
        sqlx::query_scalar!("SELECT id FROM lists WHERE account_id = $1", account_id)
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();
    for list_id in lists {
        crate::feed::populate_list(&mut redis, &state.redis_keys, &state.db, list_id).await;
    }
    finish.finish().await;
}

/// Which feed a [`MergeWorker`] or [`UnmergeWorker`] works on: `'home'`, an
/// account's, or `'list'`, a list's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FeedType {
    Home,
    List,
}

/// `MergeWorker`: merge `from_account_id`'s posts into the home feed of the
/// account `into_id`, or into the list `into_id`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MergeWorker {
    pub from_account_id: i64,
    pub into_id: i64,
    #[serde(rename = "type")]
    pub feed: FeedType,
}

impl crate::jobs::Job for MergeWorker {
    const KIND: &'static str = "MergeWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT;

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        merge(state, self.from_account_id, self.into_id, self.feed).await;
        Ok(())
    }
}

/// `MergeWorker#perform`. A merge into a home feed finishes any
/// regeneration a first follow started, however it went (its `ensure`).
pub async fn merge(state: &AppState, from_account_id: i64, into_id: i64, feed: FeedType) {
    if !account_exists(state, from_account_id).await {
        return;
    }
    let mut redis = state.redis.clone();
    match feed {
        FeedType::Home => {
            if !account_exists(state, into_id).await {
                return;
            }
            crate::feed::merge_into_home(
                &mut redis,
                &state.redis_keys,
                &state.db,
                from_account_id,
                into_id,
            )
            .await;
            regeneration_finished(state, into_id).await;
        }
        FeedType::List => {
            crate::feed::merge_into_list(
                &mut redis,
                &state.redis_keys,
                &state.db,
                from_account_id,
                into_id,
            )
            .await;
        }
    }
}

/// `UnmergeWorker`, in the `pull` queue: take `from_account_id`'s posts out
/// of the home feed of the account `into_id`, or out of the list `into_id`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UnmergeWorker {
    pub from_account_id: i64,
    pub into_id: i64,
    #[serde(rename = "type")]
    pub feed: FeedType,
}

impl crate::jobs::Job for UnmergeWorker {
    const KIND: &'static str = "UnmergeWorker";
    const OPTIONS: crate::jobs::Options =
        crate::jobs::Options::DEFAULT.queue(crate::jobs::Queue::Pull);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        unmerge(state, self.from_account_id, self.into_id, self.feed).await;
        Ok(())
    }
}

/// `UnmergeWorker#perform`.
pub async fn unmerge(state: &AppState, from_account_id: i64, into_id: i64, feed: FeedType) {
    if !account_exists(state, from_account_id).await {
        return;
    }
    let mut redis = state.redis.clone();
    match feed {
        FeedType::Home => {
            crate::feed::unmerge_from_home(
                &mut redis,
                &state.redis_keys,
                &state.db,
                from_account_id,
                into_id,
            )
            .await;
        }
        FeedType::List => {
            crate::feed::unmerge_from_list(
                &mut redis,
                &state.redis_keys,
                &state.db,
                from_account_id,
                into_id,
            )
            .await;
        }
    }
}

/// `MergeWorker.perform_async(from, into, type)`, or the worker at once when
/// the tests ask for background work inline.
pub async fn enqueue_merge(state: &AppState, from_account_id: i64, into_id: i64, feed: FeedType) {
    if crate::feed::sync_fanout() {
        merge(state, from_account_id, into_id, feed).await;
    } else {
        crate::jobs::push(
            state,
            MergeWorker {
                from_account_id,
                into_id,
                feed,
            },
        )
        .await;
    }
}

/// `UnmergeWorker.perform_async(from, into, type)`, or the worker at once.
pub async fn enqueue_unmerge(state: &AppState, from_account_id: i64, into_id: i64, feed: FeedType) {
    if crate::feed::sync_fanout() {
        unmerge(state, from_account_id, into_id, feed).await;
    } else {
        crate::jobs::push(
            state,
            UnmergeWorker {
                from_account_id,
                into_id,
                feed,
            },
        )
        .await;
    }
}

/// `account.owned_lists.with_list_account(target_account).pluck(:id)`.
pub async fn lists_with_account(state: &AppState, owner_id: i64, account_id: i64) -> Vec<i64> {
    sqlx::query_scalar!(
        "SELECT l.id FROM lists l JOIN list_accounts la ON la.list_id = l.id
         WHERE l.account_id = $1 AND la.account_id = $2",
        owner_id,
        account_id,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
}

/// What `FollowService#direct_follow!`, `FollowRequest#authorize!` and
/// `UnmuteService` queue: a [`MergeWorker`] of `target_id`'s posts into
/// `account_id`'s home feed, and one into each of its lists that hold
/// `target_id`.
pub async fn merge_into_home_and_lists(state: &AppState, target_id: i64, account_id: i64) {
    let lists = lists_with_account(state, account_id, target_id).await;
    enqueue_merge(state, target_id, account_id, FeedType::Home).await;
    for list_id in lists {
        enqueue_merge(state, target_id, list_id, FeedType::List).await;
    }
}

/// What `UnfollowService` queues: an [`UnmergeWorker`] of `target_id`'s
/// posts out of `account_id`'s home feed, and out of each of `list_ids`, the
/// lists that held `target_id` before the follow went (and with it their
/// memberships).
pub async fn unmerge_from_home_and_lists(
    state: &AppState,
    target_id: i64,
    account_id: i64,
    list_ids: Vec<i64>,
) {
    enqueue_unmerge(state, target_id, account_id, FeedType::Home).await;
    for list_id in list_ids {
        enqueue_unmerge(state, target_id, list_id, FeedType::List).await;
    }
}
