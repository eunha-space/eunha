//! The job queue: what Mastodon hands to Sidekiq, kept in `eunha.jobs` so
//! that a restart loses none of it (docs/operating/jobs.md).
//!
//! A [`Job`] is a Mastodon worker class: its name ([`Job::KIND`]), its
//! arguments (the job's own fields, serialized as JSON), and its
//! `sidekiq_options` ([`Job::OPTIONS`]) — the queue it waits in, how many
//! times it is retried and whether it is kept in the dead set after the
//! last, and the sidekiq-unique-jobs lock it holds. [`perform_async`],
//! [`perform_in`] and [`perform_at`] queue one, as their Sidekiq namesakes
//! do; the instance's job loops ([`run`]) claim due jobs with `FOR UPDATE
//! SKIP LOCKED`, in Sidekiq's weighted queue order, and run them.
//!
//! A job that fails is retried `(count**4) + 15` seconds later, plus
//! Sidekiq's jitter of `rand(10) * (count + 1)`, unless its worker has a
//! `sidekiq_retry_in` of its own ([`Job::retry_in`]). A claimed job is held
//! by a lease its loop renews while it runs, so one whose process died is
//! taken up again by another once the lease lapses.
//!
//! Tests run jobs as soon as they are queued, in a task of their own
//! ([`Mode::Immediate`]), or leave them queued and [`drain`] them.

mod registry;

use std::future::Future;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use futures::StreamExt as _;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::state::AppState;

// ── Options ────────────────────────────────────────────────────────────────

/// The queues of Mastodon's `config/sidekiq.yml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Queue {
    Default,
    Push,
    Ingress,
    Mailers,
    Pull,
    Scheduler,
    Fasp,
}

impl Queue {
    pub const ALL: [Self; 7] = [
        Self::Default,
        Self::Push,
        Self::Ingress,
        Self::Mailers,
        Self::Pull,
        Self::Scheduler,
        Self::Fasp,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Push => "push",
            Self::Ingress => "ingress",
            Self::Mailers => "mailers",
            Self::Pull => "pull",
            Self::Scheduler => "scheduler",
            Self::Fasp => "fasp",
        }
    }

    /// Its weight in `config/sidekiq.yml`: `[default, 8]`, `[push, 6]`,
    /// `[ingress, 4]`, `[mailers, 2]`, and one for the rest.
    pub const fn weight(self) -> usize {
        match self {
            Self::Default => 8,
            Self::Push => 6,
            Self::Ingress => 4,
            Self::Mailers => 2,
            Self::Pull | Self::Scheduler | Self::Fasp => 1,
        }
    }
}

/// `sidekiq_options retry:`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retry {
    /// Retried this many times after the first failure; `retry: 0` runs it
    /// once, and still counts its failure as exhausting its retries.
    Count(u32),
    /// `retry: false`: run once, and forgotten if it fails.
    Never,
}

/// sidekiq-unique-jobs' `lock:`, with its `lock_ttl`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lock {
    None,
    /// No other job with the same worker and arguments is queued until this
    /// one has run successfully, or failed for good.
    UntilExecuted(Duration),
    /// No other is queued until this one starts.
    UntilExecuting(Duration),
}

/// Mastodon's `SidekiqUniqueJobs.config.lock_ttl`, for a lock that names
/// none of its own.
pub const DEFAULT_LOCK_TTL: Duration = Duration::from_secs(50 * 24 * 3600);

/// A worker's `sidekiq_options`.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub queue: Queue,
    pub retry: Retry,
    /// `dead:`: whether a job that has run out of retries is kept in the dead
    /// set.
    pub dead: bool,
    pub lock: Lock,
}

impl Options {
    /// Sidekiq's defaults: the `default` queue, 25 retries, the dead set.
    pub const DEFAULT: Self = Self {
        queue: Queue::Default,
        retry: Retry::Count(25),
        dead: true,
        lock: Lock::None,
    };

    pub const fn queue(self, queue: Queue) -> Self {
        Self { queue, ..self }
    }

    pub const fn retry(self, retries: u32) -> Self {
        Self {
            retry: Retry::Count(retries),
            ..self
        }
    }

    pub const fn no_retry(self) -> Self {
        Self {
            retry: Retry::Never,
            ..self
        }
    }

    pub const fn dead(self, dead: bool) -> Self {
        Self { dead, ..self }
    }

    pub const fn lock(self, lock: Lock) -> Self {
        Self { lock, ..self }
    }
}

/// A Mastodon worker.
pub trait Job: Serialize + DeserializeOwned + Send + Sync + 'static {
    /// The worker's class name, which is what a queued job is stored under.
    const KIND: &'static str;
    const OPTIONS: Options;

    /// `#perform`. An error retries the job, unless it is a [`Discard`].
    fn perform(self, state: &AppState) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// `sidekiq_retry_in`: how long to wait after the `count`th retry (from
    /// nought) fails, before Sidekiq's jitter. `None` is Sidekiq's own
    /// `(count**4) + 15`.
    fn retry_in(_count: u32) -> Option<Duration> {
        None
    }

    /// `sidekiq_retries_exhausted`.
    fn retries_exhausted(self, _state: &AppState, _error: &str) -> impl Future<Output = ()> + Send {
        async {}
    }
}

/// An error that fails a job without retrying it, as Mastodon's
/// `SidekiqMiddleware` swallows a `HostValidationError`.
#[derive(Debug)]
pub struct Discard(pub String);

impl std::fmt::Display for Discard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Discard {}

/// `ExponentialBackoff`'s `sidekiq_retry_in`: `15 + (10 * (count**4)) +
/// rand(10 * (count**4))`.
pub fn exponential_backoff(count: u32) -> Option<Duration> {
    let base = 10 * u64::from(count).saturating_pow(4);
    let jitter = if base == 0 {
        0
    } else {
        rand::random_range(0..base)
    };
    Some(Duration::from_secs(15 + base + jitter))
}

/// How long Sidekiq waits after the `count`th retry fails (`delay_for`):
/// `custom`, or `(count**4) + 15`, plus `rand(10) * (count + 1)`.
fn retry_delay(count: u32, custom: Option<Duration>) -> Duration {
    let count = u64::from(count);
    let base = custom.unwrap_or_else(|| Duration::from_secs(count.saturating_pow(4) + 15));
    let jitter = rand::random_range(0..10u64) * (count + 1);
    Duration::from_secs(base.as_secs() + jitter)
}

// ── The runtime ────────────────────────────────────────────────────────────

/// How queued jobs are run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    /// By the instance's job loops, when they come due.
    Durable = 0,
    /// At once, each in a task of its own, whatever its delay: what the tests
    /// use, as Mastodon's use `Sidekiq::Testing.inline!`. A job that fails
    /// is still queued for its retry, which [`drain`] runs.
    Immediate = 1,
}

/// An instance's job runtime: its mode, and the jobs running in tasks of
/// their own under [`Mode::Immediate`].
#[derive(Default)]
pub struct Runtime {
    mode: AtomicU8,
    running: AtomicUsize,
    settled: tokio::sync::Notify,
}

impl Runtime {
    pub fn mode(&self) -> Mode {
        match self.mode.load(Ordering::Relaxed) {
            1 => Mode::Immediate,
            _ => Mode::Durable,
        }
    }

    pub fn set_mode(&self, mode: Mode) {
        self.mode.store(mode as u8, Ordering::Relaxed);
    }

    /// Wait until no job started under [`Mode::Immediate`] is still running.
    pub async fn settle(&self) {
        loop {
            let settled = self.settled.notified();
            if self.running.load(Ordering::Acquire) == 0 {
                return;
            }
            settled.await;
        }
    }
}

struct Running(std::sync::Arc<Runtime>);

impl Running {
    fn start(runtime: &std::sync::Arc<Runtime>) -> Self {
        runtime.running.fetch_add(1, Ordering::AcqRel);
        Self(runtime.clone())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.0.running.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.settled.notify_waiters();
        }
    }
}

// ── Queueing ───────────────────────────────────────────────────────────────

/// `Worker.perform_async(*args)`. Returns the job's id, or `None` when a job
/// holding its unique lock is already queued.
pub async fn perform_async<J: Job>(state: &AppState, job: J) -> anyhow::Result<Option<i64>> {
    enqueue(state, Duration::ZERO, job).await
}

/// `Worker.perform_in(delay, *args)`.
pub async fn perform_in<J: Job>(
    state: &AppState,
    delay: Duration,
    job: J,
) -> anyhow::Result<Option<i64>> {
    enqueue(state, delay, job).await
}

/// `Worker.perform_at(at, *args)`.
pub async fn perform_at<J: Job>(
    state: &AppState,
    at: chrono::DateTime<chrono::Utc>,
    job: J,
) -> anyhow::Result<Option<i64>> {
    let delay = (at - chrono::Utc::now()).to_std().unwrap_or_default();
    enqueue(state, delay, job).await
}

/// [`perform_async`], logging a job that could not be queued rather than
/// returning the error: for the places where Mastodon queues a job and
/// carries on whatever came of it.
pub async fn push<J: Job>(state: &AppState, job: J) {
    if let Err(error) = perform_async(state, job).await {
        tracing::error!(kind = J::KIND, %error, "could not queue a job");
    }
}

/// [`perform_in`], logging a job that could not be queued.
pub async fn push_in<J: Job>(state: &AppState, delay: Duration, job: J) {
    if let Err(error) = perform_in(state, delay, job).await {
        tracing::error!(kind = J::KIND, %error, "could not queue a job");
    }
}

async fn enqueue<J: Job>(state: &AppState, delay: Duration, job: J) -> anyhow::Result<Option<i64>> {
    let args = serde_json::to_value(&job)?;
    let id = insert(&state.db, J::KIND, &J::OPTIONS, &args, delay).await?;
    if let Some(id) = id {
        state.queues.jobs.notify_one();
        if state.jobs.mode() == Mode::Immediate {
            let state = state.clone();
            let running = Running::start(&state.jobs);
            crate::tenants::spawn(async move {
                let _running = running;
                if let Err(error) = run_one(&state, id).await {
                    tracing::warn!(id, %error, "could not run a job");
                }
            });
        }
    }
    Ok(id)
}

/// The unique lock of a job of `kind` with `args`: sidekiq-unique-jobs'
/// digest is of the worker, its queue and its arguments.
fn unique_key(kind: &str, queue: Queue, args: &Value) -> String {
    format!("{}:{kind}:{args}", queue.name())
}

async fn insert(
    db: &sqlx::PgPool,
    kind: &str,
    options: &Options,
    args: &Value,
    delay: Duration,
) -> anyhow::Result<Option<i64>> {
    let (unique_key, ttl, unlock_on_claim) = match options.lock {
        Lock::None => (None, Duration::ZERO, false),
        Lock::UntilExecuted(ttl) => (Some(unique_key(kind, options.queue, args)), ttl, false),
        Lock::UntilExecuting(ttl) => (Some(unique_key(kind, options.queue, args)), ttl, true),
    };
    if let Some(key) = &unique_key {
        // A lock past its TTL is no longer held, whether or not its job has
        // run.
        sqlx::query!(
            "UPDATE eunha.jobs SET unique_key = NULL
             WHERE unique_key = $1 AND unique_until <= now()",
            key,
        )
        .execute(db)
        .await?;
    }
    let (max_retries, keep_dead) = match options.retry {
        Retry::Count(n) => (i32::try_from(n).unwrap_or(i32::MAX), options.dead),
        Retry::Never => (-1, false),
    };
    let delay = delay.as_secs_f64();
    let id = sqlx::query_scalar!(
        r#"INSERT INTO eunha.jobs
             (queue, kind, args, run_at, max_retries, keep_dead,
              unique_key, unique_until, unlock_on_claim)
           VALUES ($1, $2, $3, now() + make_interval(secs => $4), $5, $6,
                   $7, now() + make_interval(secs => $4 + $8), $9)
           ON CONFLICT (unique_key) WHERE unique_key IS NOT NULL DO NOTHING
           RETURNING id"#,
        options.queue.name(),
        kind,
        args,
        delay,
        max_retries,
        keep_dead,
        unique_key,
        ttl.as_secs_f64(),
        unlock_on_claim,
    )
    .fetch_optional(db)
    .await?;
    Ok(id)
}

// ── Running ────────────────────────────────────────────────────────────────

/// How long a claimed job stays claimed without its lease being renewed.
const LEASE: Duration = Duration::from_secs(300);

/// How often a running job's lease is renewed.
const HEARTBEAT: Duration = Duration::from_secs(60);

/// How long a stopping loop waits for its jobs to finish before handing them
/// back to the queue, as Sidekiq's shutdown `timeout` does.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

/// A claimed job.
struct Claimed {
    id: i64,
    kind: String,
    args: Value,
    attempts: i32,
    max_retries: i32,
    keep_dead: bool,
}

/// The queues in the order one fetch looks at them: Sidekiq's weighted fetch,
/// which lists each queue as many times as its weight, shuffles the list,
/// and keeps each queue's first place.
fn queue_order() -> Vec<String> {
    use rand::seq::SliceRandom as _;
    let mut weighted: Vec<Queue> = Queue::ALL
        .iter()
        .flat_map(|q| std::iter::repeat_n(*q, q.weight()))
        .collect();
    weighted.shuffle(&mut rand::rng());
    let mut order = Vec::with_capacity(Queue::ALL.len());
    for queue in weighted {
        let name = queue.name().to_owned();
        if !order.contains(&name) {
            order.push(name);
        }
    }
    order
}

async fn claim(
    state: &AppState,
    limit: i64,
    worker: &str,
    only: Option<i64>,
) -> anyhow::Result<Vec<Claimed>> {
    let order = queue_order();
    let rows = sqlx::query_as!(
        Claimed,
        r#"WITH picked AS (
             SELECT id FROM eunha.jobs
             WHERE dead_at IS NULL
               AND ($5::bigint IS NULL OR id = $5)
               AND ($5::bigint IS NOT NULL OR run_at <= now())
               AND (locked_at IS NULL OR locked_at < now() - make_interval(secs => $3))
             ORDER BY array_position($1::text[], queue), run_at, id
             LIMIT $2
             FOR UPDATE SKIP LOCKED
           )
           UPDATE eunha.jobs j
           SET locked_at = now(), locked_by = $4,
               unique_key = CASE WHEN j.unlock_on_claim THEN NULL ELSE j.unique_key END
           FROM picked WHERE j.id = picked.id
           RETURNING j.id, j.kind, j.args, j.attempts, j.max_retries, j.keep_dead"#,
        &order,
        limit,
        LEASE.as_secs_f64(),
        worker,
        only,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows)
}

/// Run job `id` now, whenever it is due, unless another worker holds it.
async fn run_one(state: &AppState, id: i64) -> anyhow::Result<()> {
    let worker = format!("immediate-{}", std::process::id());
    for job in claim(state, 1, &worker, Some(id)).await? {
        execute(state, job, &worker).await?;
    }
    Ok(())
}

/// Run every job that is due, one at a time, until none is: what the tests
/// use to run what they queued, and to run a retry once they have made it
/// due. Returns how many ran.
pub async fn drain(state: &AppState) -> anyhow::Result<usize> {
    let worker = format!("drain-{}", std::process::id());
    let mut ran = 0;
    loop {
        let jobs = claim(state, 1, &worker, None).await?;
        if jobs.is_empty() {
            return Ok(ran);
        }
        for job in jobs {
            execute(state, job, &worker).await?;
            ran += 1;
        }
    }
}

/// Make every queued job due now, for a test to run its retry.
pub async fn make_due(state: &AppState) -> anyhow::Result<()> {
    sqlx::query!("UPDATE eunha.jobs SET run_at = now() WHERE dead_at IS NULL AND run_at > now()")
        .execute(&state.db)
        .await?;
    Ok(())
}

/// Run a claimed job, renewing its lease while it runs, and record how it
/// came out.
async fn execute(state: &AppState, job: Claimed, worker: &str) -> anyhow::Result<()> {
    let Some(entry) = registry::get(&job.kind) else {
        tracing::error!(kind = job.kind, id = job.id, "no worker for a queued job");
        return bury(state, &job, "no such worker").await;
    };
    let perform = (entry.perform)(state.clone(), job.args.clone());
    tokio::pin!(perform);
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.tick().await;
    let result = loop {
        tokio::select! {
            result = &mut perform => break result,
            _ = heartbeat.tick() => {
                let _ = sqlx::query!(
                    "UPDATE eunha.jobs SET locked_at = now() WHERE id = $1 AND locked_by = $2",
                    job.id,
                    worker,
                )
                .execute(&state.db)
                .await;
            }
        }
    };
    match result {
        Ok(()) => {
            sqlx::query!("DELETE FROM eunha.jobs WHERE id = $1", job.id)
                .execute(&state.db)
                .await?;
        }
        Err(error) if error.downcast_ref::<Discard>().is_some() => {
            tracing::warn!(kind = job.kind, id = job.id, %error, "job discarded");
            sqlx::query!("DELETE FROM eunha.jobs WHERE id = $1", job.id)
                .execute(&state.db)
                .await?;
        }
        Err(error) => fail(state, entry, job, &error).await?,
    }
    Ok(())
}

/// Sidekiq's retry handling for a job that raised.
async fn fail(
    state: &AppState,
    entry: &registry::Entry,
    job: Claimed,
    error: &anyhow::Error,
) -> anyhow::Result<()> {
    let message = crate::error::sanitize_error_text(&format!("{error:#}"));
    // `retry_count`, which is nought at the first failure.
    let count = u32::try_from(job.attempts).unwrap_or(0);
    if job.max_retries < 0 {
        tracing::warn!(kind = job.kind, id = job.id, error = message, "job failed");
        sqlx::query!("DELETE FROM eunha.jobs WHERE id = $1", job.id)
            .execute(&state.db)
            .await?;
        return Ok(());
    }
    if count >= u32::try_from(job.max_retries).unwrap_or(0) {
        tracing::warn!(
            kind = job.kind,
            id = job.id,
            error = message,
            "job failed for the last time"
        );
        (entry.exhausted)(state.clone(), job.args.clone(), message.clone()).await;
        if job.keep_dead {
            return bury(state, &job, &message).await;
        }
        sqlx::query!("DELETE FROM eunha.jobs WHERE id = $1", job.id)
            .execute(&state.db)
            .await?;
        return Ok(());
    }
    let delay = retry_delay(count, (entry.retry_in)(count));
    tracing::info!(
        kind = job.kind,
        id = job.id,
        retry = count + 1,
        in_seconds = delay.as_secs(),
        error = message,
        "job failed; retrying"
    );
    sqlx::query!(
        "UPDATE eunha.jobs
         SET attempts = attempts + 1, run_at = now() + make_interval(secs => $2),
             locked_at = NULL, locked_by = NULL, last_error = $3, failed_at = now()
         WHERE id = $1",
        job.id,
        delay.as_secs_f64(),
        message,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Move a job to the dead set, giving up its unique lock.
async fn bury(state: &AppState, job: &Claimed, message: &str) -> anyhow::Result<()> {
    sqlx::query!(
        "UPDATE eunha.jobs
         SET attempts = attempts + 1, dead_at = now(), failed_at = now(), last_error = $2,
             unique_key = NULL, locked_at = NULL, locked_by = NULL
         WHERE id = $1",
        job.id,
        message,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Sidekiq's `dead_max_jobs`.
const DEAD_MAX_JOBS: i64 = 10_000;

/// Sidekiq's `dead_timeout_in_seconds`, six months.
const DEAD_TIMEOUT: Duration = Duration::from_secs(180 * 24 * 3600);

/// Trim the dead set as Sidekiq does: nothing older than six months, and no
/// more than the newest ten thousand.
pub async fn prune_dead(state: &AppState) -> anyhow::Result<u64> {
    let old = sqlx::query!(
        "DELETE FROM eunha.jobs
         WHERE dead_at IS NOT NULL AND dead_at < now() - make_interval(secs => $1)",
        DEAD_TIMEOUT.as_secs_f64(),
    )
    .execute(&state.db)
    .await?
    .rows_affected();
    let excess = sqlx::query!(
        "DELETE FROM eunha.jobs WHERE id IN (
           SELECT id FROM eunha.jobs WHERE dead_at IS NOT NULL
           ORDER BY dead_at DESC, id DESC OFFSET $1)",
        DEAD_MAX_JOBS,
    )
    .execute(&state.db)
    .await?
    .rows_affected();
    Ok(old + excess)
}

/// One of the instance's job loops: keep up to `[workers] job_concurrency`
/// jobs running, claiming more as each finishes, until the instance stops.
/// Once it has, jobs still running after [`SHUTDOWN_TIMEOUT`] are handed back
/// to the queue for another process to run.
pub async fn run(state: AppState, index: usize) {
    let workers = state.config.workers.sanitized();
    let concurrency = workers.job_concurrency;
    let worker = format!(
        "jobs-{}-{index}-{:08x}",
        std::process::id(),
        rand::random::<u32>()
    );
    let mut idle =
        crate::background::IdleBackoff::new(Duration::from_secs(1), workers.queue_idle_poll());
    let mut in_flight = futures::stream::FuturesUnordered::new();
    let mut last_prune = None::<std::time::Instant>;
    while !state.stop.is_cancelled() {
        if index == 0
            && last_prune
                .is_none_or(|at: std::time::Instant| at.elapsed() >= Duration::from_secs(3600))
        {
            last_prune = Some(std::time::Instant::now());
            if let Err(error) = prune_dead(&state).await {
                tracing::warn!(%error, "could not prune the dead jobs");
            }
        }
        let free = concurrency.saturating_sub(in_flight.len());
        let mut claimed = 0;
        if free > 0 {
            match claim(&state, free as i64, &worker, None).await {
                Ok(jobs) => {
                    claimed = jobs.len();
                    for job in jobs {
                        // Each in a task of its own, so that a job waiting on
                        // a database connection is not starved by the claim
                        // that is waiting on another.
                        let (state, worker) = (state.clone(), worker.clone());
                        in_flight.push(crate::tenants::spawn(async move {
                            let (kind, id) = (job.kind.clone(), job.id);
                            if let Err(error) = execute(&state, job, &worker).await {
                                tracing::error!(kind, id, %error, "could not record how a job came out");
                            }
                        }));
                    }
                }
                Err(error) => {
                    tracing::error!(%error, "could not claim jobs");
                    crate::background::rest(&state.stop, Duration::from_secs(5)).await;
                    continue;
                }
            }
        }
        if claimed > 0 {
            idle.reset();
        }
        if in_flight.is_empty() {
            idle.idle(&state.queues.jobs, &state.stop).await;
            continue;
        }
        if claimed > 0 && in_flight.len() < concurrency {
            // There may be more due.
            continue;
        }
        // Nothing more is due, or there is no room: wait for a job to finish,
        // for one to be queued, or for the poll that finds retries come due.
        let room = in_flight.len() < concurrency;
        tokio::select! {
            _ = in_flight.next() => {}
            _ = state.queues.jobs.notified(), if room => {}
            () = tokio::time::sleep(workers.queue_idle_poll()), if room => {}
            () = state.stop.cancelled() => {}
        }
    }
    let finished = tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
        while in_flight.next().await.is_some() {}
    })
    .await;
    if finished.is_err() {
        for task in in_flight.iter() {
            task.abort();
        }
        drop(in_flight);
        let handed_back = sqlx::query!(
            "UPDATE eunha.jobs SET locked_at = NULL, locked_by = NULL WHERE locked_by = $1",
            worker,
        )
        .execute(&state.db)
        .await;
        match handed_back {
            Ok(done) => tracing::warn!(
                jobs = done.rows_affected(),
                "jobs still running at shutdown were handed back to the queue"
            ),
            Err(error) => tracing::error!(%error, "could not hand running jobs back to the queue"),
        }
    }
}

/// A job as the tests and the admin see it.
#[derive(Debug, Clone)]
pub struct Queued {
    pub id: i64,
    pub queue: String,
    pub kind: String,
    pub args: Value,
    pub attempts: i32,
    pub seconds_until_due: f64,
    pub dead: bool,
    pub unique_key: Option<String>,
    pub last_error: Option<String>,
}

/// The jobs of `kind` in the queue and the dead set, oldest first.
pub async fn queued(state: &AppState, kind: &str) -> anyhow::Result<Vec<Queued>> {
    let rows = sqlx::query!(
        r#"SELECT id, queue, kind, args, attempts,
                  EXTRACT(EPOCH FROM run_at - now())::float8 AS "due!",
                  dead_at IS NOT NULL AS "dead!", unique_key, last_error
           FROM eunha.jobs WHERE kind = $1 ORDER BY id"#,
        kind,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Queued {
            id: r.id,
            queue: r.queue,
            kind: r.kind,
            args: r.args,
            attempts: r.attempts,
            seconds_until_due: r.due,
            dead: r.dead,
            unique_key: r.unique_key,
            last_error: r.last_error,
        })
        .collect())
}

// ── Probes ─────────────────────────────────────────────────────────────────

/// A worker of eunha's own that does nothing but count its runs, failing the
/// first `fail_times` of them: how the tests, and an operator wondering
/// whether the queue is moving, see the queue's retries for themselves. Its
/// runs are counted in Redis, under `jobs:probe:<name>`.
#[derive(Serialize, serde::Deserialize, Clone, Debug)]
pub struct Probe {
    pub name: String,
    #[serde(default)]
    pub fail_times: i64,
}

impl Job for Probe {
    const KIND: &'static str = "Eunha::ProbeWorker";
    const OPTIONS: Options = Options::DEFAULT.retry(2);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        probe_run(state, &self.name, self.fail_times).await
    }

    async fn retries_exhausted(self, state: &AppState, _error: &str) {
        let mut redis = state.redis_coordination.clone();
        let _: redis::RedisResult<i64> = redis::cmd("INCRBY")
            .arg(
                state
                    .redis_keys
                    .key(format!("jobs:probe:{}:exhausted", self.name)),
            )
            .arg(1)
            .query_async(&mut redis)
            .await;
    }
}

/// [`Probe`], held unique until it has run (`lock: :until_executed`) and not
/// kept when it fails (`retry: 0, dead: false`).
#[derive(Serialize, serde::Deserialize, Clone, Debug)]
pub struct UniqueProbe {
    pub name: String,
    #[serde(default)]
    pub fail_times: i64,
}

impl Job for UniqueProbe {
    const KIND: &'static str = "Eunha::UniqueProbeWorker";
    const OPTIONS: Options = Options::DEFAULT
        .retry(0)
        .dead(false)
        .lock(Lock::UntilExecuted(Duration::from_secs(3600)));

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        probe_run(state, &self.name, self.fail_times).await
    }
}

async fn probe_run(state: &AppState, name: &str, fail_times: i64) -> anyhow::Result<()> {
    let mut redis = state.redis_coordination.clone();
    let runs: i64 = redis::cmd("INCRBY")
        .arg(state.redis_keys.key(format!("jobs:probe:{name}")))
        .arg(1)
        .query_async(&mut redis)
        .await?;
    anyhow::ensure!(
        runs > fail_times,
        "probe {name} failing as asked, run {runs}"
    );
    Ok(())
}

/// How many times the probe `name` has run, and how many of those exhausted
/// its retries.
pub async fn probe_runs(state: &AppState, name: &str) -> (i64, i64) {
    let mut redis = state.redis_coordination.clone();
    let runs: Option<i64> = redis::cmd("GET")
        .arg(state.redis_keys.key(format!("jobs:probe:{name}")))
        .query_async(&mut redis)
        .await
        .unwrap_or(None);
    let exhausted: Option<i64> = redis::cmd("GET")
        .arg(state.redis_keys.key(format!("jobs:probe:{name}:exhausted")))
        .query_async(&mut redis)
        .await
        .unwrap_or(None);
    (runs.unwrap_or(0), exhausted.unwrap_or(0))
}

pub(crate) type Perform = fn(AppState, Value) -> BoxFuture<'static, anyhow::Result<()>>;
pub(crate) type Exhausted = fn(AppState, Value, String) -> BoxFuture<'static, ()>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidekiq_default_backoff() {
        for count in 0..10u32 {
            let delay = retry_delay(count, None).as_secs();
            let base = u64::from(count).pow(4) + 15;
            assert!(delay >= base && delay <= base + 9 * (u64::from(count) + 1));
        }
    }

    #[test]
    fn custom_backoff_keeps_sidekiq_jitter() {
        let delay = retry_delay(2, Some(Duration::from_secs(100))).as_secs();
        assert!((100..=127).contains(&delay));
    }

    #[test]
    fn exponential_backoff_matches_mastodon() {
        assert_eq!(exponential_backoff(0), Some(Duration::from_secs(15)));
        for _ in 0..50 {
            let d = exponential_backoff(2).unwrap().as_secs();
            assert!((175..335).contains(&d), "{d}");
        }
    }

    #[test]
    fn queue_order_lists_every_queue_once() {
        for _ in 0..20 {
            let order = queue_order();
            assert_eq!(order.len(), Queue::ALL.len());
            for q in Queue::ALL {
                assert!(order.iter().any(|n| n == q.name()));
            }
        }
    }

    #[test]
    fn worker_names_are_unique() {
        registry::check_unique();
    }
}
