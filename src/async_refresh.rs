//! Mastodon's `AsyncRefresh`: a Redis hash recording that work a request
//! started in the background is still running, which the client is told about
//! in a `Mastodon-Async-Refresh` header and can poll at
//! `GET /api/v1_alpha/async_refreshes/:id` until it finishes.
//!
//! The hash holds `status` (`running` or `finished`) and, for work that counts
//! what it found, `result_count`. A new refresh lives for a day; a finished one
//! for an hour, long enough for the client's next poll to see it finish.
//!
//! Mastodon's id is the Redis key signed with `Rails.application.
//! message_verifier('async_refreshes')`, which derives its key from
//! `SECRET_KEY_BASE` — a secret eunha does not have. The id here has the same
//! shape (the key in base64, `--`, an HMAC-SHA256 of it) and is keyed from the
//! instance's VAPID private key, the one secret every instance is configured
//! with. Ids are opaque to clients and live at most a day, and the hashes they
//! name sit under eunha's key prefix where a Mastodon would not look, so
//! nothing depends on the two signers agreeing.
//!
//! The hashes live in the coordination Redis, which is not evicted: a refresh
//! that vanished while its work ran would let the next request start the same
//! work again.

use redis::AsyncCommands;
use serde_json::{json, Value};

use crate::state::AppState;

/// `AsyncRefresh::NEW_REFRESH_EXPIRATION`.
pub const NEW_REFRESH_EXPIRATION_SECONDS: i64 = 24 * 60 * 60;
/// `AsyncRefresh::FINISHED_REFRESH_EXPIRATION`.
pub const FINISHED_REFRESH_EXPIRATION_SECONDS: i64 = 60 * 60;

/// The header Mastodon's `AsyncRefreshesConcern` sets.
pub const HEADER: &str = "Mastodon-Async-Refresh";

/// A refresh as Redis last described it.
#[derive(Debug, Clone)]
pub struct AsyncRefresh {
    /// The key without the instance's prefix, which is what the id signs.
    key: String,
    pub status: Option<String>,
    pub result_count: Option<i64>,
}

impl AsyncRefresh {
    /// `AsyncRefresh.new(key)`: whatever Redis holds under `key`, which may be
    /// nothing (neither running nor finished).
    pub async fn new(state: &AppState, key: &str) -> Self {
        let mut refresh = Self {
            key: key.to_owned(),
            status: None,
            result_count: None,
        };
        refresh.reload(state).await;
        refresh
    }

    /// `AsyncRefresh.create`: mark `key` running, with a `result_count` of 0
    /// when the work counts its results.
    pub async fn create(state: &AppState, key: &str, count_results: bool) -> Self {
        let mut redis = state.redis_coordination.clone();
        let redis_key = state.redis_keys.key(key);
        let mut pipe = redis::pipe();
        pipe.hset(&redis_key, "status", "running").ignore();
        if count_results {
            pipe.hset(&redis_key, "result_count", 0).ignore();
        }
        pipe.expire(&redis_key, NEW_REFRESH_EXPIRATION_SECONDS)
            .ignore();
        if let Err(error) = pipe.query_async::<()>(&mut redis).await {
            tracing::warn!(%error, key, "could not record an async refresh");
        }
        Self {
            key: key.to_owned(),
            status: Some("running".into()),
            result_count: count_results.then_some(0),
        }
    }

    /// `AsyncRefresh.find`: the refresh an id names, if the id is genuine and
    /// the refresh has not expired.
    pub async fn find(state: &AppState, id: &str) -> Option<Self> {
        let key = verify(state, id)?;
        let mut redis = state.redis_coordination.clone();
        let exists: bool = redis
            .exists(state.redis_keys.key(&key))
            .await
            .unwrap_or(false);
        if !exists {
            return None;
        }
        Some(Self::new(state, &key).await)
    }

    /// The signed id clients are handed.
    pub fn id(&self, state: &AppState) -> String {
        sign(state, &self.key)
    }

    pub fn is_running(&self) -> bool {
        self.status.as_deref() == Some("running")
    }

    pub fn is_finished(&self) -> bool {
        self.status.as_deref() == Some("finished")
    }

    pub async fn reload(&mut self, state: &AppState) {
        let mut redis = state.redis_coordination.clone();
        let redis_key = state.redis_keys.key(&self.key);
        let fields: redis::RedisResult<(Option<String>, Option<String>)> = redis::pipe()
            .hget(&redis_key, "status")
            .hget(&redis_key, "result_count")
            .query_async(&mut redis)
            .await;
        let (status, result_count) = fields.unwrap_or_default();
        self.status = status;
        self.result_count = result_count
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().unwrap_or(0));
    }

    /// `AsyncRefreshesConcern#add_async_refresh_header`'s value, or `None`
    /// when the refresh is not running and so no header is sent.
    pub fn header_value(&self, state: &AppState, retry_seconds: u32) -> Option<String> {
        if !self.is_running() {
            return None;
        }
        let mut value = format!("id=\"{}\", retry={retry_seconds}", self.id(state));
        if let Some(count) = self.result_count {
            value.push_str(&format!(", result_count={count}"));
        }
        Some(value)
    }

    /// `AsyncRefresh#to_json`.
    pub fn to_json(&self, state: &AppState) -> Value {
        json!({
            "async_refresh": {
                "id": self.id(state),
                "status": self.status,
                "result_count": self.result_count,
            }
        })
    }
}

/// `AsyncRefresh#finish!`.
pub async fn finish(state: &AppState, key: &str) {
    let mut redis = state.redis_coordination.clone();
    let redis_key = state.redis_keys.key(key);
    let result: redis::RedisResult<()> = redis::pipe()
        .hset(&redis_key, "status", "finished")
        .ignore()
        .expire(&redis_key, FINISHED_REFRESH_EXPIRATION_SECONDS)
        .ignore()
        .query_async(&mut redis)
        .await;
    if let Err(error) = result {
        tracing::warn!(%error, key, "could not finish an async refresh");
    }
}

/// `AsyncRefresh#increment_result_count`. Mastodon's `HINCRBY` alone would
/// recreate an expired hash with no expiry at all, so the expiry is set again
/// with it; the work is still running, and a running refresh lives a day.
pub async fn increment_result_count(state: &AppState, key: &str, by: i64) {
    let mut redis = state.redis_coordination.clone();
    let redis_key = state.redis_keys.key(key);
    let result: redis::RedisResult<()> = redis::pipe()
        .hincr(&redis_key, "result_count", by)
        .ignore()
        .expire(&redis_key, NEW_REFRESH_EXPIRATION_SECONDS)
        .ignore()
        .query_async(&mut redis)
        .await;
    if let Err(error) = result {
        tracing::warn!(%error, key, "could not count an async refresh's results");
    }
}

/// Finishes a refresh when the task doing its work ends, however it ends —
/// returning early, failing, or being dropped when the instance stops — so
/// that a refresh is never left `running` for the day a new one lives,
/// holding off the next attempt at the same work.
pub struct FinishOnDrop {
    state: Option<AppState>,
    key: String,
}

impl FinishOnDrop {
    pub fn new(state: &AppState, key: &str) -> Self {
        Self {
            state: Some(state.clone()),
            key: key.to_owned(),
        }
    }

    pub async fn finish(mut self) {
        if let Some(state) = self.state.take() {
            finish(&state, &self.key).await;
        }
    }
}

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let key = std::mem::take(&mut self.key);
        crate::tenants::spawn(async move { finish(&state, &key).await });
    }
}

/// `MessageVerifier#generate`, in URL-safe base64 so the id can sit in a path.
fn sign(state: &AppState, key: &str) -> String {
    crate::crypto::sign_message(&state.instance.vapid_private_key, b"async_refreshes", key)
}

/// `MessageVerifier#verify`: the key an id signs, or `None` for an id that was
/// not signed here.
fn verify(state: &AppState, id: &str) -> Option<String> {
    crate::crypto::verify_message(&state.instance.vapid_private_key, b"async_refreshes", id)
}
