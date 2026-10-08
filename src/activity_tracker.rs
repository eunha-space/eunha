//! Mastodon's `ActivityTracker`: daily counters in Redis, under the same keys
//! Mastodon keeps them, so that a Mastodon process sharing the Redis and the
//! key prefix reads and adds to the same counts.
//!
//! A counter is `<prefix>:<midnight UTC as a Unix timestamp>`, which an
//! increment adds one to (`INCRBY`) and a record adds a value to as a
//! HyperLogLog (`PFADD`), each kept six months after it last changed. What
//! eunha counts, as Mastodon does: `activity:logins` (the users who signed
//! in), `activity:accounts:local` (sign-ups), `activity:statuses:local` (local
//! public and unlisted posts) and `activity:interactions` (favourites, boosts,
//! follows, poll votes, and replies to someone else).

use chrono::{Datelike, NaiveDate};

use crate::state::AppState;

/// `ActivityTracker::EXPIRE_AFTER`: six months, as ActiveSupport counts them.
const EXPIRE_AFTER: i64 = 15_778_476;

pub const LOGINS: &str = "activity:logins";
pub const ACCOUNTS_LOCAL: &str = "activity:accounts:local";
pub const STATUSES_LOCAL: &str = "activity:statuses:local";
pub const INTERACTIONS: &str = "activity:interactions";

/// `ActivityTracker.new(prefix, type)`'s type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `:basic`, a counter.
    Basic,
    /// `:unique`, a HyperLogLog of distinct values.
    Unique,
}

/// `key_at(at_time)`: the day's counter.
fn key_at(state: &AppState, prefix: &str, date: NaiveDate) -> String {
    let midnight = date.and_hms_opt(0, 0, 0).expect("midnight").and_utc();
    state
        .redis_keys
        .key(format!("{prefix}:{}", midnight.timestamp()))
}

/// `legacy_key_at(at_time)`: the ISO week number the counters were once kept
/// under (`Date#cweek`), still read when summing.
fn legacy_key_at(state: &AppState, prefix: &str, date: NaiveDate) -> String {
    state
        .redis_keys
        .key(format!("{prefix}:{}", date.iso_week().week()))
}

fn days(start: NaiveDate, end: NaiveDate) -> impl Iterator<Item = NaiveDate> {
    start.iter_days().take_while(move |date| *date <= end)
}

/// `ActivityTracker#add`, today: one more for a basic counter, `value` into a
/// unique one. Best-effort, as nothing Mastodon does waits on it either.
async fn add(state: &AppState, prefix: &str, value: Option<i64>) {
    let key = key_at(state, prefix, chrono::Utc::now().date_naive());
    let mut redis = state.redis.clone();
    let mut pipe = redis::pipe();
    match value {
        Some(value) => pipe.cmd("PFADD").arg(&key).arg(value).ignore(),
        None => pipe.cmd("INCRBY").arg(&key).arg(1).ignore(),
    };
    let result: redis::RedisResult<()> = pipe
        .cmd("EXPIRE")
        .arg(&key)
        .arg(EXPIRE_AFTER)
        .ignore()
        .query_async(&mut redis)
        .await;
    if let Err(error) = result {
        tracing::warn!(%error, prefix, "could not record activity");
    }
}

/// `ActivityTracker.increment(prefix)`.
pub async fn increment(state: &AppState, prefix: &str) {
    add(state, prefix, None).await;
}

/// `ActivityTracker.record(prefix, value)`.
pub async fn record(state: &AppState, prefix: &str, value: i64) {
    add(state, prefix, Some(value)).await;
}

/// `Status#update_statistics`, after a local status is created: a public or
/// unlisted one (`distributable?`) counts towards the day's local posts.
pub async fn local_status_created(state: &AppState, visibility: i32) {
    use crate::db::models::vis;
    if matches!(visibility, vis::PUBLIC | vis::UNLISTED) {
        increment(state, STATUSES_LOCAL).await;
    }
}

/// `ActivityTracker#get(start_at, end_at)`: each day from `start` to `end`
/// with its count.
pub async fn get(
    state: &AppState,
    prefix: &str,
    kind: Kind,
    start: NaiveDate,
    end: NaiveDate,
) -> redis::RedisResult<Vec<(NaiveDate, i64)>> {
    let dates: Vec<NaiveDate> = days(start, end).collect();
    if dates.is_empty() {
        return Ok(Vec::new());
    }
    let mut redis = state.redis.clone();
    let mut pipe = redis::pipe();
    for date in &dates {
        let key = key_at(state, prefix, *date);
        match kind {
            Kind::Basic => pipe.cmd("GET").arg(key),
            Kind::Unique => pipe.cmd("PFCOUNT").arg(key),
        };
    }
    let values: Vec<i64> = match kind {
        // `redis.get(key).to_i`.
        Kind::Basic => {
            let raw: Vec<Option<String>> = pipe.query_async(&mut redis).await?;
            raw.into_iter().map(|v| to_i(v.as_deref())).collect()
        }
        Kind::Unique => pipe.query_async(&mut redis).await?,
    };
    Ok(dates.into_iter().zip(values).collect())
}

/// `ActivityTracker#sum(start_at, end_at)`: the days from `start` to `end`,
/// each under its own key and the week it was once kept under.
pub async fn sum(
    state: &AppState,
    prefix: &str,
    kind: Kind,
    start: NaiveDate,
    end: NaiveDate,
) -> redis::RedisResult<i64> {
    let mut keys: Vec<String> = Vec::new();
    for date in days(start, end) {
        for key in [
            key_at(state, prefix, date),
            legacy_key_at(state, prefix, date),
        ] {
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
    }
    if keys.is_empty() {
        return Ok(0);
    }
    let mut redis = state.redis.clone();
    match kind {
        Kind::Basic => {
            let raw: Vec<Option<String>> = redis::cmd("MGET")
                .arg(&keys)
                .query_async(&mut redis)
                .await?;
            Ok(raw.into_iter().map(|v| to_i(v.as_deref())).sum())
        }
        Kind::Unique => {
            redis::cmd("PFCOUNT")
                .arg(&keys)
                .query_async(&mut redis)
                .await
        }
    }
}

/// Ruby's `String#to_i` (and `nil.to_i`): the leading integer, else zero.
fn to_i(value: Option<&str>) -> i64 {
    let Some(value) = value else { return 0 };
    let value = value.trim_start();
    let (sign, digits) = match value.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, value.strip_prefix('+').unwrap_or(value)),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    digits[..end].parse::<i64>().map_or(0, |n| sign * n)
}

/// `InstancePresenter#active_user_count(num_weeks)`: the users who signed in
/// since `num_weeks` weeks ago, cached for ten minutes as `Rails.cache` keeps
/// it by default.
pub async fn active_user_count(state: &AppState, num_weeks: i64) -> i64 {
    let cache_key = state
        .redis_keys
        .key(format!("active_user_count/{num_weeks}"));
    let mut redis = state.redis.clone();
    let cached: redis::RedisResult<Option<i64>> = redis::cmd("GET")
        .arg(&cache_key)
        .query_async(&mut redis)
        .await;
    if let Ok(Some(count)) = cached {
        return count;
    }
    let today = chrono::Utc::now().date_naive();
    let start = (chrono::Utc::now() - chrono::Duration::weeks(num_weeks)).date_naive();
    let count = match sum(state, LOGINS, Kind::Unique, start, today).await {
        Ok(count) => count,
        Err(error) => {
            tracing::warn!(%error, "could not count active users");
            return 0;
        }
    };
    let _: redis::RedisResult<()> = redis::cmd("SET")
        .arg(&cache_key)
        .arg(count)
        .arg("EX")
        .arg(600)
        .query_async(&mut redis)
        .await;
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_counters_as_ruby_does() {
        assert_eq!(to_i(None), 0);
        assert_eq!(to_i(Some("12")), 12);
        assert_eq!(to_i(Some("12abc")), 12);
        assert_eq!(to_i(Some("abc")), 0);
        assert_eq!(to_i(Some("-3")), -3);
    }

    #[test]
    fn days_include_both_ends() {
        let start = NaiveDate::from_ymd_opt(2026, 9, 28).unwrap();
        let end = NaiveDate::from_ymd_opt(2026, 10, 4).unwrap();
        assert_eq!(days(start, end).count(), 7);
        assert_eq!(days(end, start).count(), 0);
    }
}
