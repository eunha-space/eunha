//! `Trends::History`: seven days of uses and distinct users of something,
//! counted in Redis under `activity:{prefix}:{id}:{day}`. Email domain blocks
//! keep one of how often each refused a sign-up.
//!
//! Writes are best-effort: a Redis user without the commands loses the count,
//! not the request.

use serde_json::{json, Value};

use crate::state::AppState;

/// `Trends::History::Day::EXPIRE_AFTER`.
const EXPIRE_AFTER: i64 = 14 * 24 * 60 * 60;

fn day_start(days_ago: i64) -> i64 {
    let now = chrono::Utc::now().timestamp();
    now - now.rem_euclid(86_400) - days_ago * 86_400
}

fn key(state: &AppState, prefix: &str, id: i64, day: i64) -> String {
    state
        .redis_keys
        .key(format!("activity:{prefix}:{id}:{day}"))
}

/// `Trends::History#add(value)`, today.
pub async fn add(state: &AppState, prefix: &str, id: i64, value: &str) {
    let uses = key(state, prefix, id, day_start(0));
    let accounts = format!("{uses}:accounts");
    let mut redis = state.redis.clone();
    let result: redis::RedisResult<()> = redis::pipe()
        .cmd("INCRBY")
        .arg(&uses)
        .arg(1)
        .ignore()
        .cmd("PFADD")
        .arg(&accounts)
        .arg(value)
        .ignore()
        .cmd("EXPIRE")
        .arg(&uses)
        .arg(EXPIRE_AFTER)
        .ignore()
        .cmd("EXPIRE")
        .arg(&accounts)
        .arg(EXPIRE_AFTER)
        .ignore()
        .query_async(&mut redis)
        .await;
    if let Err(error) = result {
        tracing::debug!(%error, prefix, id, "could not count a history entry");
    }
}

/// `Trends::History#as_json`: today first, `day`, `accounts` and `uses` as
/// strings.
pub async fn as_json(state: &AppState, prefix: &str, id: i64) -> Value {
    let mut redis = state.redis.clone();
    let mut days = vec![];
    for days_ago in 0..7 {
        let day = day_start(days_ago);
        let uses_key = key(state, prefix, id, day);
        let uses: i64 = redis::cmd("GET")
            .arg(&uses_key)
            .query_async::<Option<i64>>(&mut redis)
            .await
            .ok()
            .flatten()
            .unwrap_or(0);
        let accounts: i64 = redis::cmd("PFCOUNT")
            .arg(format!("{uses_key}:accounts"))
            .query_async(&mut redis)
            .await
            .unwrap_or(0);
        days.push(json!({
            "day": day.to_string(),
            "accounts": accounts.to_string(),
            "uses": uses.to_string(),
        }));
    }
    Value::Array(days)
}
