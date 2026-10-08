//! `Lockable#with_redis_lock`: a best-effort Redis lock, held for as long as
//! its guard lives.

use crate::state::AppState;

/// Mastodon's default `with_redis_lock` autorelease, fifteen minutes.
pub const DEFAULT_TTL_MS: usize = 15 * 60 * 1000;

/// A held lock: released on drop, but only while this guard still owns it,
/// so a lock that autoreleased and was taken by someone else stays theirs.
pub struct RedisLock {
    redis: redis::aio::ConnectionManager,
    key: String,
    token: String,
    use_pooled_function: bool,
}

impl RedisLock {
    /// Release the lock now, as `with_redis_lock` does when its block ends,
    /// so that whatever runs next can take it. Dropping the guard releases it
    /// too, but in the background, a moment later.
    pub async fn release(mut self) {
        let key = std::mem::take(&mut self.key);
        let token = std::mem::take(&mut self.token);
        release(self.redis.clone(), key, token, self.use_pooled_function).await;
    }
}

impl Drop for RedisLock {
    fn drop(&mut self) {
        if self.key.is_empty() {
            return;
        }
        let redis = self.redis.clone();
        let key = std::mem::take(&mut self.key);
        let token = std::mem::take(&mut self.token);
        let use_pooled_function = self.use_pooled_function;
        crate::tenants::spawn(release(redis, key, token, use_pooled_function));
    }
}

/// Delete `key` if it still holds `token`.
async fn release(
    mut redis: redis::aio::ConnectionManager,
    key: String,
    token: String,
    use_pooled_function: bool,
) {
    // A pooled Redis installs this named function so tenant users need no
    // permission to submit arbitrary Lua. Dedicated deployments retain the
    // EVAL fallback and require no provisioning change.
    if use_pooled_function {
        let released: redis::RedisResult<i64> = redis::cmd("FCALL")
            .arg("eunha_compare_delete")
            .arg(1)
            .arg(&key)
            .arg(&token)
            .query_async(&mut redis)
            .await;
        if released.is_ok() {
            return;
        }
    }
    let _: redis::RedisResult<i64> = redis::cmd("EVAL")
        .arg("if redis.call('get', KEYS[1]) == ARGV[1] then return redis.call('del', KEYS[1]) else return 0 end")
        .arg(1)
        .arg(&key)
        .arg(&token)
        .query_async(&mut redis)
        .await;
}

/// Try once to take the lock named `name`, which goes into Redis under the
/// instance's key prefix exactly as given. `None` when someone else holds it
/// or Redis cannot be reached.
pub async fn try_acquire(state: &AppState, name: &str, ttl_ms: usize) -> Option<RedisLock> {
    let redis = &state.redis_coordination;
    let keys = &state.redis_keys;
    let key = keys.key(name);
    let token = crate::snowflake::next_id().to_string();
    let mut connection = redis.clone();
    let acquired: redis::RedisResult<Option<String>> = redis::cmd("SET")
        .arg(&key)
        .arg(&token)
        .arg("NX")
        .arg("PX")
        .arg(ttl_ms)
        .query_async(&mut connection)
        .await;
    matches!(acquired, Ok(Some(_))).then(|| RedisLock {
        redis: redis.clone(),
        key,
        token,
        use_pooled_function: keys.is_shared(),
    })
}
