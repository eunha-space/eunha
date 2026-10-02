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
//! A feed Redis does not hold at all, which happens when Redis lost it, is
//! regenerated the same way the first time it is read.

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

/// `RegenerationWorker.perform_async(account_id)`. Run in the request under
/// [`crate::feed::sync_fanout`], as the tests ask.
pub async fn enqueue_regeneration(state: &AppState, account_id: i64) {
    if crate::feed::sync_fanout() {
        regenerate(state, account_id).await;
    } else {
        let state = state.clone();
        crate::tenants::spawn(async move { regenerate(&state, account_id).await });
    }
}

/// `RegenerationWorker#perform`, which is `PrecomputeFeedService#call`: fill
/// the home feed and each of the account's lists, then finish the
/// regeneration however that went.
pub async fn regenerate(state: &AppState, account_id: i64) {
    let finish = async_refresh::FinishOnDrop::new(state, &regeneration_key(account_id));
    let mut redis = state.redis.clone();
    crate::feed::feed_populate(&mut redis, &state.redis_keys, account_id, &state.db).await;
    let lists = sqlx::query!(
        r#"SELECT id,
                  CASE replies_policy WHEN 0 THEN 'list' WHEN 1 THEN 'followed'
                                      WHEN 2 THEN 'none' ELSE 'list' END AS "replies_policy!"
           FROM lists WHERE account_id = $1"#,
        account_id,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    for list in lists {
        crate::feed::list_feed_populate(
            &mut redis,
            &state.redis_keys,
            list.id,
            account_id,
            &list.replies_policy,
            &state.db,
        )
        .await;
    }
    finish.finish().await;
}

/// `MergeWorker#merge_into_home!`: merge `from`'s posts into `into`'s home
/// feed, then finish any regeneration the follow started (its `ensure`).
pub async fn merge_into_home(state: &AppState, from_account_id: i64, into_account_id: i64) {
    let mut redis = state.redis.clone();
    crate::feed::backfill_follow(
        &mut redis,
        &state.redis_keys,
        &state.db,
        into_account_id,
        from_account_id,
    )
    .await;
    regeneration_finished(state, into_account_id).await;
}

/// `MergeWorker.perform_async(from, into, 'home')`: [`merge_into_home`] in
/// the background, or in the request under [`crate::feed::sync_fanout`].
pub async fn enqueue_merge_into_home(state: &AppState, from_account_id: i64, into_account_id: i64) {
    if crate::feed::sync_fanout() {
        merge_into_home(state, from_account_id, into_account_id).await;
    } else {
        let state = state.clone();
        crate::tenants::spawn(async move {
            merge_into_home(&state, from_account_id, into_account_id).await;
        });
    }
}
