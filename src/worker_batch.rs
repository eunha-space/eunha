//! Mastodon's `WorkerBatch`: a set of queued jobs that together do one piece
//! of work, connected to the async refresh that tells a client the work is
//! still running. The refresh is finished when the batch's last job leaves it.
//!
//! Sidekiq's client middleware adds a job to the batch that is current when
//! the job is pushed, and Sidekiq hands a job its `jid`. Eunha's jobs have
//! neither, so a job that belongs to a batch carries the batch's id and a
//! `jid` of its own in its arguments, and is added to the batch before it is
//! queued — a job run at once could otherwise leave the batch before it
//! joined it.
//!
//! The batch is two keys in the coordination Redis, as the refresh it
//! finishes is: `worker_batch:{id}`, a hash of the pending and processed
//! counts and the refresh's key, and `worker_batch:{id}:jobs`, the set of
//! jobs still in it. Both live an hour.

use crate::state::AppState;

/// `WorkerBatch::TTL`.
pub const TTL_SECONDS: i64 = 3600;

/// A batch, by its id.
#[derive(Debug, Clone)]
pub struct WorkerBatch {
    pub id: String,
}

/// `SecureRandom.hex(12)`, which is what Mastodon names batches and Sidekiq
/// names jobs with.
pub fn random_id() -> String {
    hex::encode(rand::random::<[u8; 12]>())
}

impl WorkerBatch {
    /// `WorkerBatch.new(id)`: the batch `id`, or a new one.
    pub fn new(id: Option<String>) -> Self {
        Self {
            id: id.unwrap_or_else(random_id),
        }
    }

    fn key(&self, state: &AppState, suffix: Option<&str>) -> String {
        state.redis_keys.key(match suffix {
            Some(suffix) => format!("worker_batch:{}:{suffix}", self.id),
            None => format!("worker_batch:{}", self.id),
        })
    }

    /// `WorkerBatch#connect`: finish the refresh `async_refresh_key` once the
    /// share `threshold` of the batch's jobs has been processed.
    pub async fn connect(&self, state: &AppState, async_refresh_key: &str, threshold: f64) {
        let mut redis = state.redis_coordination.clone();
        let result: redis::RedisResult<()> = redis::pipe()
            .hset(
                self.key(state, None),
                "async_refresh_key",
                async_refresh_key,
            )
            .ignore()
            .hset(self.key(state, None), "threshold", threshold)
            .ignore()
            .query_async(&mut redis)
            .await;
        if let Err(error) = result {
            tracing::warn!(%error, batch = self.id, "could not connect a worker batch");
        }
    }

    /// `WorkerBatch#add_jobs`.
    pub async fn add_jobs(&self, state: &AppState, jids: &[String]) {
        if jids.is_empty() {
            return;
        }
        let mut redis = state.redis_coordination.clone();
        let jobs = self.key(state, Some("jobs"));
        let batch = self.key(state, None);
        let result: redis::RedisResult<()> = redis::pipe()
            .sadd(&jobs, jids)
            .ignore()
            .expire(&jobs, TTL_SECONDS)
            .ignore()
            .hincr(&batch, "pending", jids.len())
            .ignore()
            .expire(&batch, TTL_SECONDS)
            .ignore()
            .query_async(&mut redis)
            .await;
        if let Err(error) = result {
            tracing::warn!(%error, batch = self.id, "could not add jobs to a worker batch");
        }
    }

    /// `WorkerBatch#remove_job`: the job `jid` is done, one way or the other,
    /// and counts in the refresh's results when `increment`. The refresh is
    /// finished, and the batch removed, once no job is pending or the
    /// threshold share has been processed. As Mastodon's, a job that leaves
    /// twice — once for each attempt — counts twice.
    pub async fn remove_job(&self, state: &AppState, jid: &str, increment: bool) {
        let mut redis = state.redis_coordination.clone();
        let batch = self.key(state, None);
        let fields: redis::RedisResult<(i64, i64, Option<String>, Option<String>)> = redis::pipe()
            .srem(self.key(state, Some("jobs")), jid)
            .ignore()
            .hincr(&batch, "pending", -1)
            .hincr(&batch, "processed", 1)
            .hget(&batch, "async_refresh_key")
            .hget(&batch, "threshold")
            .query_async(&mut redis)
            .await;
        let (pending, processed, async_refresh_key, threshold) = match fields {
            Ok(fields) => fields,
            Err(error) => {
                tracing::warn!(%error, batch = self.id, "could not remove a job from a worker batch");
                return;
            }
        };
        let async_refresh_key = async_refresh_key.filter(|key| !key.is_empty());
        if increment {
            if let Some(key) = &async_refresh_key {
                crate::async_refresh::increment_result_count(state, key, 1).await;
            }
        }
        let threshold: f64 = threshold.and_then(|t| t.parse().ok()).unwrap_or(1.0);
        if pending == 0 || processed as f64 >= threshold * (processed + pending) as f64 {
            if let Some(key) = &async_refresh_key {
                crate::async_refresh::finish(state, key).await;
            }
            self.cleanup(state).await;
        }
    }

    /// `WorkerBatch#finish!`.
    pub async fn finish(&self, state: &AppState) {
        let mut redis = state.redis_coordination.clone();
        let key: Option<String> = redis::cmd("HGET")
            .arg(self.key(state, None))
            .arg("async_refresh_key")
            .query_async(&mut redis)
            .await
            .unwrap_or(None);
        if let Some(key) = key.filter(|key| !key.is_empty()) {
            crate::async_refresh::finish(state, &key).await;
        }
        self.cleanup(state).await;
    }

    async fn cleanup(&self, state: &AppState) {
        let mut redis = state.redis_coordination.clone();
        let result: redis::RedisResult<()> = redis::cmd("DEL")
            .arg(self.key(state, None))
            .arg(self.key(state, Some("jobs")))
            .query_async(&mut redis)
            .await;
        if let Err(error) = result {
            tracing::warn!(%error, batch = self.id, "could not remove a worker batch");
        }
    }
}

/// A job's place in its batch, which it leaves however its attempt ends —
/// Mastodon's `ensure batch.remove_job(jid)`. Dropped without
/// [`Membership::leave`], when the attempt fails or the instance stops, it
/// leaves without counting.
pub struct Membership {
    state: Option<AppState>,
    batch: WorkerBatch,
    jid: String,
}

impl Membership {
    /// The job `jid`'s place in the batch `batch_id`, if it has one.
    pub fn new(state: &AppState, batch_id: Option<&str>, jid: Option<&str>) -> Option<Self> {
        Some(Self {
            state: Some(state.clone()),
            batch: WorkerBatch::new(Some(batch_id?.to_owned())),
            jid: jid?.to_owned(),
        })
    }

    pub async fn leave(mut self, increment: bool) {
        if let Some(state) = self.state.take() {
            self.batch.remove_job(&state, &self.jid, increment).await;
        }
    }
}

impl Drop for Membership {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let batch = self.batch.clone();
        let jid = std::mem::take(&mut self.jid);
        crate::tenants::spawn(async move { batch.remove_job(&state, &jid, false).await });
    }
}
