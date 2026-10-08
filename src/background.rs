use std::future::Future;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::state::AppState;

/// How long a stopped instance's background task may take to finish the pass it
/// is in before it is dropped where it stands. Every loop notices a stop between
/// passes and while it sleeps, so this only bounds a pass that is slow — a large
/// delivery batch, an account deletion. Dropping one loses no work: a claimed
/// job's lock goes stale, and a worker takes it up again.
pub const STOP_GRACE: Duration = Duration::from_secs(20);

/// Spawns all of an instance's background tasks, each in the tenant's span so
/// that what they log names the instance, and returns them so that stopping the
/// instance can wait for them.
pub fn spawn(state: AppState) -> Vec<JoinHandle<()>> {
    let _tenant = crate::tenants::span(&state.instance.domain).entered();
    // The schedules. A self-destructing instance runs its own alone, as
    // Mastodon replaces `Sidekiq.schedule` with `SelfDestructScheduler`.
    let mut tasks = if crate::self_destruct::enabled(&state.instance) {
        tracing::warn!("self-destruct mode: running no schedule but the self-destruct");
        vec![until_stopped(
            &state,
            "self-destruct",
            crate::self_destruct::run(state.clone()),
        )]
    } else {
        schedules(&state)
    };
    tasks.push(until_stopped(
        &state,
        "media queue",
        crate::api::mastodon::media::run_media_queue(state.clone()),
    ));
    tasks.push(until_stopped(
        &state,
        "archive queue",
        crate::portability::backup::run_queue(state.clone()),
    ));

    // Queue loops are sized from `[workers]` in config. Each loop claims work
    // with `FOR UPDATE SKIP LOCKED`, so adding loops within this process scales
    // the same way adding processes would.
    let workers = state.config.workers.sanitized();
    for _ in 0..workers.delivery_workers {
        let deliverer = state.deliverer.clone();
        let stop = state.stop.clone();
        tasks.push(until_stopped(&state, "delivery queue", async move {
            deliverer.run_until(stop.cancelled_owned()).await;
        }));
    }
    for index in 0..workers.job_workers {
        tasks.push(until_stopped(
            &state,
            "job queue",
            crate::jobs::run(state.clone(), index),
        ));
    }
    tracing::info!(
        job_workers = workers.job_workers,
        job_concurrency = workers.job_concurrency,
        delivery_workers = workers.delivery_workers,
        delivery_concurrency = workers.delivery_concurrency,
        "background queues started"
    );
    tasks
}

/// The timed tasks: what Mastodon's *config/sidekiq.yml* schedules.
fn schedules(state: &AppState) -> Vec<JoinHandle<()>> {
    let state = state.clone();
    vec![
        until_stopped(
            &state,
            "scheduled statuses",
            run_scheduled_statuses(state.clone()),
        ),
        until_stopped(
            &state,
            "suspended account cleanup",
            run_suspended_account_cleanup(state.clone()),
        ),
        until_stopped(&state, "trends refresh", run_trends_refresh(state.clone())),
        until_stopped(
            &state,
            "instances refresh",
            run_instances_refresh(state.clone()),
        ),
        until_stopped(&state, "trends review", run_trends_review(state.clone())),
        until_stopped(
            &state,
            "user cleanup",
            crate::email_subscriptions::run_cleanup(state.clone()),
        ),
        until_stopped(
            &state,
            "delivery cleanup",
            crate::federation::delivery::run_delivery_cleanup(state.clone()),
        ),
        until_stopped(
            &state,
            "import and archive vacuum",
            crate::portability::run_vacuum(state.clone()),
        ),
        until_stopped(&state, "vacuum", crate::vacuum::run(state.clone())),
        until_stopped(&state, "IP cleanup", crate::ip_cleanup::run(state.clone())),
        until_stopped(
            &state,
            "account statuses cleanup",
            crate::statuses_cleanup::run(state.clone()),
        ),
        until_stopped(
            &state,
            "collection item cleanup",
            crate::collection_item_cleanup::run(state.clone()),
        ),
        until_stopped(
            &state,
            "remote collection repair",
            crate::federation::featured_collections::run_repair(state.clone()),
        ),
        until_stopped(
            &state,
            "announcement schedule",
            crate::announcements::run_schedule(state.clone()),
        ),
        until_stopped(
            &state,
            "follow recommendations",
            crate::suggestions::run(state.clone()),
        ),
        until_stopped(
            &state,
            "auto-close registrations",
            crate::auto_close_registrations::run(state.clone()),
        ),
        until_stopped(
            &state,
            "FASP follow recommendation cleanup",
            crate::fasp::workers::run_follow_recommendation_cleanup(state.clone()),
        ),
        until_stopped(
            &state,
            "search indexing",
            crate::search::elasticsearch::indexing::run(state.clone()),
        ),
    ]
}

/// Spawn `work`, one of the loops above, which returns by itself once the
/// instance is stopped and it has finished the pass it was in — or is dropped,
/// if that takes longer than [`STOP_GRACE`].
fn until_stopped(
    state: &AppState,
    task: &'static str,
    work: impl Future<Output = ()> + Send + 'static,
) -> JoinHandle<()> {
    let stop = state.stop.clone();
    crate::tenants::spawn(async move {
        tokio::select! {
            () = work => {}
            () = async {
                stop.cancelled().await;
                tokio::time::sleep(STOP_GRACE).await;
            } => {
                tracing::warn!(task, "background task still busy after the grace period; dropped");
            }
        }
    })
}

/// Sleep for `nap`, or until the instance is stopped.
pub async fn rest(stop: &CancellationToken, nap: Duration) {
    tokio::select! {
        () = stop.cancelled() => {}
        () = tokio::time::sleep(nap) => {}
    }
}

// ── Queue wake-ups ────────────────────────────────────────────────────────

/// One wake-up per durable queue, raised by whoever enqueues a job.
///
/// Jobs are enqueued by requests this process serves, so the loop draining a
/// queue can be told about them rather than finding them by polling. Polling
/// every half-second cost an idle instance three transactions a second and kept
/// a database connection open for good, which on a host of mostly idle tenants
/// is most of what those tenants cost. The loops still poll, backing off towards
/// `[workers] queue_idle_poll_seconds`, for what no wake-up announces: a retry
/// whose `run_at` has come due, and a job enqueued by another process sharing
/// the database.
///
/// The timed tasks have wake-ups too, for when the next thing they are waiting
/// for moves earlier than the time they went to sleep until.
#[derive(Default)]
pub struct QueueWakes {
    pub delivery: tokio::sync::Notify,
    pub media: tokio::sync::Notify,
    /// A job was queued (crate::jobs).
    pub jobs: tokio::sync::Notify,
    /// An archive takeout was requested.
    pub backups: tokio::sync::Notify,
    /// A scheduled status was created or moved.
    pub scheduled_statuses: tokio::sync::Notify,
}

/// How long a queue loop sleeps after finding nothing: `floor` at first,
/// doubling with every empty pass up to `ceiling`, and `floor` again once work
/// turns up.
pub struct IdleBackoff {
    floor: Duration,
    ceiling: Duration,
    current: Duration,
}

impl IdleBackoff {
    pub fn new(floor: Duration, ceiling: Duration) -> Self {
        Self {
            floor,
            ceiling: ceiling.max(floor),
            current: floor,
        }
    }

    pub fn reset(&mut self) {
        self.current = self.floor;
    }

    /// Sleep until `wake` is raised, the current interval passes, or the
    /// instance is stopped.
    ///
    /// `Notify` keeps a permit raised while nobody was waiting, so a job
    /// enqueued between an empty claim and this call ends the sleep at once
    /// instead of waiting out the interval.
    pub async fn idle(&mut self, wake: &tokio::sync::Notify, stop: &CancellationToken) {
        tokio::select! {
            () = stop.cancelled() => {}
            _ = wake.notified() => self.reset(),
            _ = tokio::time::sleep(jittered(self.current, rand::random())) => {
                self.current = (self.current * 2).min(self.ceiling);
            }
        }
    }
}

/// The most an idle sleep is shortened by, at random.
const IDLE_JITTER: f64 = 0.25;

/// `nap`, shortened by up to `IDLE_JITTER` of itself; `unit`, in `[0, 1)`, says
/// how far.
///
/// Tenants started together — every tenant on a host that has just restarted —
/// would otherwise wake together on every idle poll and open their whole pools
/// at once: a hundred started as one reached 200 connections. Jittering each
/// sleep independently lets them drift apart within a few rounds. It only ever
/// shortens a sleep, so a configured ceiling is still the longest anything
/// waits.
fn jittered(nap: Duration, unit: f64) -> Duration {
    nap.mul_f64(1.0 - unit.clamp(0.0, 1.0) * IDLE_JITTER)
}

// ── Timed tasks ───────────────────────────────────────────────────────────

/// How long a timed task sleeps before its next pass: until its next item is
/// due, but no less than `floor` and no more than `ceiling`.
///
/// Scheduled statuses and suspended account cleanup used to run
/// every minute or two whether or not anything was due, and every pass opened a
/// database connection, so an idle tenant was never without one for long. Most
/// tenants have nothing scheduled, and for them this is the
/// ceiling. The floor keeps an item that stays due — one whose work keeps
/// failing — from turning the loop into a busy one.
///
/// Only an idle nap, with nothing due before the ceiling, is `jittered` by
/// `jitter`; an item that falls due is woken for on time.
fn timed_task_nap(
    seconds_until_due: Option<f64>,
    floor: Duration,
    ceiling: Duration,
    jitter: f64,
) -> Duration {
    let ceiling = ceiling.max(floor);
    match seconds_until_due {
        Some(s) if s.is_finite() && s < ceiling.as_secs_f64() => {
            Duration::from_secs_f64(s.max(0.0)).max(floor)
        }
        _ => jittered(ceiling, jitter).max(floor),
    }
}

/// Sleep for `nap`, until `wake` says the next item may now be due sooner, or
/// until the instance is stopped.
async fn sleep_or_wake(wake: &tokio::sync::Notify, stop: &CancellationToken, nap: Duration) {
    tokio::select! {
        () = stop.cancelled() => {}
        _ = wake.notified() => {}
        _ = tokio::time::sleep(nap) => {}
    }
}

/// Shortest pause between passes when an item is due. Scheduled statuses and
/// polls fall due at a known instant, so a second is precise enough.
const TIMED_TASK_FLOOR: Duration = Duration::from_secs(1);

/// Shortest pause after a pass fails, which is the minute these tasks ran on:
/// a database that is down should not be asked again every second.
const TIMED_TASK_FAILURE_FLOOR: Duration = Duration::from_secs(60);

// ── Scheduled status publisher ────────────────────────────────────────────

async fn run_scheduled_statuses(state: AppState) {
    let ceiling = state.config.workers.sanitized().timed_task_idle_poll();
    while !state.stop.is_cancelled() {
        let floor = match publish_due_statuses(&state).await {
            Ok(()) => TIMED_TASK_FLOOR,
            Err(e) => {
                tracing::error!(error = %e, "scheduled status publish failed");
                TIMED_TASK_FAILURE_FLOOR
            }
        };
        let due = next_scheduled_status_due(&state).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "could not find when the next scheduled status is due");
            None
        });
        sleep_or_wake(
            &state.queues.scheduled_statuses,
            &state.stop,
            timed_task_nap(due, floor, ceiling, rand::random()),
        )
        .await;
    }
}

/// Seconds until the next schedule falls due — at its time, or at its retry
/// time after a failed attempt — or `None` when nothing is waiting. Measured
/// against the database's clock, which is the one `publish_due_statuses` uses.
pub async fn next_scheduled_status_due(state: &AppState) -> anyhow::Result<Option<f64>> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXTRACT(EPOCH FROM
                    min(GREATEST(s.scheduled_at::timestamptz, a.run_at)) - now())::float8
           FROM scheduled_statuses s
           LEFT JOIN eunha.scheduled_status_attempts a
             ON a.scheduled_status_id = s.id
           WHERE a.failed_at IS NULL"#,
    )
    .fetch_one(&state.db)
    .await?)
}

/// How many times a scheduled status that wrote nothing is retried before it is
/// parked. Combined with the backoff below this spans a couple of hours, so a
/// database blip or a restart doesn't cost anyone a post.
const SCHEDULED_STATUS_MAX_ATTEMPTS: i32 = 8;

/// Why a scheduled status could not be published, which decides whether it is
/// worth trying again.
enum PublishError {
    /// These params can never produce a status (the account is gone, the row
    /// carries no params). Retrying would fail identically every minute, so the
    /// schedule is dropped.
    Permanent(anyhow::Error),
    /// Nothing was written and the cause may not recur — a database error, a
    /// lock, a restart mid-publish. Safe to run again.
    Transient(anyhow::Error),
}

impl PublishError {
    fn error(&self) -> &anyhow::Error {
        match self {
            Self::Permanent(e) | Self::Transient(e) => e,
        }
    }
}

/// A failed `fetch_one` means the account no longer exists; anything else is
/// the database being unhappy, which may well pass.
fn classify_db(e: sqlx::Error, context: &str) -> PublishError {
    let msg = format!("{context}: {e}");
    match e {
        sqlx::Error::RowNotFound => PublishError::Permanent(anyhow::anyhow!(msg)),
        _ => PublishError::Transient(anyhow::anyhow!(msg)),
    }
}

pub async fn publish_due_statuses(state: &AppState) -> anyhow::Result<()> {
    // Skip schedules that are backing off from an earlier failure, and those
    // that have exhausted their attempts (kept, but no longer retried).
    let rows = sqlx::query!(
        r#"SELECT s.id, s.account_id, s.params
           FROM scheduled_statuses s
           LEFT JOIN eunha.scheduled_status_attempts a
             ON a.scheduled_status_id = s.id
           WHERE s.scheduled_at <= now()
             AND a.failed_at IS NULL
             AND (a.run_at IS NULL OR a.run_at <= now())
           ORDER BY s.scheduled_at ASC
           LIMIT 50"#,
    )
    .fetch_all(&state.db)
    .await?;

    for row in rows {
        match publish_one(state, row.id, row.account_id, &row.params).await {
            // The status exists now, so the schedule has been consumed even if
            // some follow-up step (fan-out, notifications) logged a failure.
            Ok(()) => forget_schedule(state, row.id).await?,
            Err(PublishError::Permanent(e)) => {
                tracing::warn!(id = row.id, error = %e, "scheduled status cannot be published; dropping");
                forget_schedule(state, row.id).await?;
            }
            Err(e @ PublishError::Transient(_)) => {
                record_publish_failure(state, row.id, e.error()).await?;
            }
        }
    }
    Ok(())
}

/// Drop a schedule and its retry bookkeeping.
async fn forget_schedule(state: &AppState, scheduled_id: i64) -> anyhow::Result<()> {
    sqlx::query!(
        "DELETE FROM eunha.scheduled_status_attempts WHERE scheduled_status_id = $1",
        scheduled_id,
    )
    .execute(&state.db)
    .await?;
    sqlx::query!("DELETE FROM scheduled_statuses WHERE id = $1", scheduled_id)
        .execute(&state.db)
        .await?;
    Ok(())
}

/// Count an attempt that wrote nothing and schedule the next one. Once the
/// attempts run out the schedule is parked rather than deleted, so the author
/// still sees the post they scheduled.
async fn record_publish_failure(
    state: &AppState,
    scheduled_id: i64,
    error: &anyhow::Error,
) -> anyhow::Result<()> {
    let err = crate::error::sanitize_error_text(&error.to_string());
    // Exponential backoff: 1m, 2m, 4m, … capped at an hour.
    let attempts = sqlx::query_scalar!(
        r#"INSERT INTO eunha.scheduled_status_attempts
             (scheduled_status_id, attempts, run_at, last_error, created_at, updated_at)
           VALUES ($1, 1, now() + interval '1 minute', $2, now(), now())
           ON CONFLICT (scheduled_status_id) DO UPDATE
             SET attempts = eunha.scheduled_status_attempts.attempts + 1,
                 run_at = now() + LEAST(
                     interval '1 hour',
                     interval '1 minute' * pow(2, eunha.scheduled_status_attempts.attempts)
                 ),
                 last_error = $2,
                 updated_at = now()
           RETURNING attempts"#,
        scheduled_id,
        err,
    )
    .fetch_one(&state.db)
    .await?;

    if attempts >= SCHEDULED_STATUS_MAX_ATTEMPTS {
        sqlx::query!(
            r#"UPDATE eunha.scheduled_status_attempts
               SET failed_at = now(), updated_at = now()
               WHERE scheduled_status_id = $1"#,
            scheduled_id,
        )
        .execute(&state.db)
        .await?;
        tracing::error!(
            id = scheduled_id,
            attempts,
            error = %err,
            "scheduled status still unpublished after every attempt; parked (schedule kept)"
        );
    } else {
        tracing::warn!(
            id = scheduled_id,
            attempts,
            error = %err,
            "scheduled status publish failed; will retry"
        );
    }
    Ok(())
}

/// Publish one scheduled status: `PublishScheduledStatusWorker`, which hands
/// the params to `PostStatusService` as the API does
/// ([`process_status`](crate::api::mastodon::statuses::process_status)), so a
/// scheduled post is written, distributed and federated as any other.
///
/// Retrying is safe because the status's id is chosen here: a failure before
/// the status was written leaves nothing behind and may be tried again, and
/// one after it was written still consumes the schedule, since running it
/// again would post a second time.
async fn publish_one(
    state: &AppState,
    scheduled_id: i64,
    account_id: i64,
    params: &Option<serde_json::Value>,
) -> Result<(), PublishError> {
    use crate::api::mastodon::statuses::{PollForm, PostStatusForm};
    let params = params
        .as_ref()
        .ok_or_else(|| PublishError::Permanent(anyhow::anyhow!("no params")))?;

    let account = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        account_id,
    )
    .fetch_one(&state.db)
    .await
    .map_err(|e| classify_db(e, "load scheduled status author"))?;
    // `return true if scheduled_status.account.user_disabled?`.
    let disabled = sqlx::query_scalar!(
        "SELECT disabled FROM users WHERE account_id = $1",
        account_id,
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|e| classify_db(e, "load scheduled status author's user"))?
    .unwrap_or(false);
    if disabled {
        return Err(PublishError::Permanent(anyhow::anyhow!(
            "the account's user is disabled"
        )));
    }

    // The params as stored, by Mastodon (ids as numbers) or by eunha before
    // it stored them so (ids as strings).
    let string = |key: &str| params[key].as_str().map(str::to_owned);
    let id = |key: &str| match &params[key] {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    };
    let ids = |key: &str| {
        params[key].as_array().map(|ids| {
            ids.iter()
                .map(|id| match id {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
        })
    };
    let poll = params["poll"].as_object().map(|poll| PollForm {
        options: poll
            .get("options")
            .and_then(|o| o.as_array())
            .map(|o| {
                o.iter()
                    .filter_map(|o| o.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        expires_in: poll.get("expires_in").and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        }),
        multiple: poll.get("multiple").and_then(|v| v.as_bool()),
        hide_totals: poll.get("hide_totals").and_then(|v| v.as_bool()),
    });
    let form = PostStatusForm {
        status: string("text"),
        in_reply_to_id: id("in_reply_to_id").map(|id| id.to_string()),
        quoted_status_id: id("quoted_status_id").map(|id| id.to_string()),
        quote_approval_policy: None,
        spoiler_text: string("spoiler_text"),
        sensitive: params["sensitive"].as_bool(),
        language: string("language"),
        visibility: string("visibility"),
        media_ids: ids("media_ids"),
        poll,
        scheduled_at: None,
        allowed_mentions: ids("allowed_mentions"),
    };

    // `PostStatusService#validate_media!` over the scheduled status's own
    // uploads, which `scheduled_status.destroy!` has handed back (`dependent:
    // :nullify`) before it runs. A refusal is raised past the worker's rescue
    // with the schedule already gone, so the post is never made.
    let media_ids = match crate::api::mastodon::statuses::validate_media(
        state,
        account.id,
        form.media_ids.as_deref(),
        None,
    )
    .await
    {
        Ok(ids) => ids,
        Err(e) => return Err(classify_app(e, "validate scheduled status media")),
    };
    // `options_hash[:quoted_status] = Status.find(quoted_status_id)`: a
    // quoted post that is gone is `RecordNotFound`, which the worker rescues
    // with the schedule already destroyed.
    let quoted = match form
        .quoted_status_id
        .as_deref()
        .and_then(|id| id.parse::<i64>().ok())
    {
        Some(quoted_id) => Some(
            sqlx::query_as!(
                crate::db::models::Status,
                "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
                quoted_id,
            )
            .fetch_optional(&state.db)
            .await
            .map_err(|e| classify_db(e, "load the quoted status"))?
            .ok_or_else(|| PublishError::Permanent(anyhow::anyhow!("the quoted status is gone")))?,
        ),
        None => None,
    };
    // `options[:quote_approval_policy]`, the bits the controller stored; none
    // in params written before it was kept, which the column's default is.
    let quote_policy = params["quote_approval_policy"]
        .as_i64()
        .and_then(|bits| i32::try_from(bits).ok())
        .unwrap_or(0);
    let application_id = id("application_id");

    let status_id = crate::snowflake::next_id();
    let posted = crate::api::mastodon::statuses::process_status(
        state,
        crate::api::mastodon::statuses::Posting {
            account: &account,
            application_id,
            form: &form,
            media_ids,
            quoted,
            quote_policy,
            status_id,
        },
    )
    .await;
    let error = match posted {
        Ok(_) => return Ok(()),
        Err(crate::api::mastodon::statuses::PostError::App(e)) => {
            classify_app(e, "publish scheduled status")
        }
        Err(crate::api::mastodon::statuses::PostError::UnexpectedMentions(_)) => {
            PublishError::Permanent(anyhow::anyhow!(
                "the post would mention accounts it was not allowed to"
            ))
        }
    };
    // Whatever failed after the status was written, it has been posted.
    let written = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM statuses WHERE id = $1) AS "e!""#,
        status_id,
    )
    .fetch_one(&state.db)
    .await
    .map_err(|e| classify_db(e, "look for the published status"))?;
    if written {
        tracing::error!(scheduled_id, status_id, error = %error.error(), "scheduled status published, but not all of what follows posting ran");
        return Ok(());
    }
    Err(error)
}

/// A database failure may pass; anything else the API would have refused
/// (`RecordInvalid`, `RecordNotFound`, `Mastodon::ValidationError`) will be
/// refused again.
fn classify_app(e: crate::error::AppError, context: &str) -> PublishError {
    match e {
        crate::error::AppError::Database(e) => classify_db(e, context),
        crate::error::AppError::Internal(e) => {
            PublishError::Transient(anyhow::anyhow!("{context}: {e}"))
        }
        e => PublishError::Permanent(anyhow::anyhow!("{context}: {e}")),
    }
}

/// `Scheduler::InstanceRefreshScheduler`: `Instance.refresh` every hour,
/// which refreshes the `instances` materialized view concurrently. A view
/// never populated (the schema creates it `WITH NO DATA`) is filled plainly
/// first, since PostgreSQL refuses `CONCURRENTLY` on one.
async fn run_instances_refresh(state: AppState) {
    while !state.stop.is_cancelled() {
        if let Err(e) = refresh_instances(&state).await {
            tracing::error!(error = %e, "instances refresh failed");
        }
        rest(&state.stop, Duration::from_secs(60 * 60)).await;
    }
}

/// `Instance.refresh`.
pub async fn refresh_instances(state: &AppState) -> anyhow::Result<()> {
    let populated: bool = sqlx::query_scalar(
        "SELECT ispopulated FROM pg_matviews WHERE schemaname = 'public' AND matviewname = 'instances'",
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(true);
    let sql = if populated {
        "REFRESH MATERIALIZED VIEW CONCURRENTLY public.instances"
    } else {
        "REFRESH MATERIALIZED VIEW public.instances"
    };
    sqlx::query(sql).execute(&state.db).await?;
    Ok(())
}

/// `Scheduler::Trends::RefreshScheduler`: `every: ['5m', first_in: '4m']`.
async fn run_trends_refresh(state: AppState) {
    rest(&state.stop, crate::trends::REFRESH_FIRST_IN).await;
    while !state.stop.is_cancelled() {
        if let Err(e) = crate::trends::refresh(&state).await {
            tracing::error!(error = %e, "trends refresh failed");
        }
        rest(&state.stop, crate::trends::REFRESH_EVERY).await;
    }
}

/// `Scheduler::Trends::ReviewNotificationsScheduler`: ask staff every six
/// hours about trends waiting on a review. The first pass waits its six hours,
/// as a newly started Sidekiq scheduler does.
async fn run_trends_review(state: AppState) {
    loop {
        rest(&state.stop, crate::trends::REVIEW_EVERY).await;
        if state.stop.is_cancelled() {
            break;
        }
        if let Err(e) = crate::trends::request_review(&state).await {
            tracing::error!(error = %e, "trends review request failed");
        }
    }
}

// ── Suspended account cleanup ─────────────────────────────────────────────

/// Mastodon's `Scheduler::SuspendedUserCleanupScheduler`: once a suspension has
/// stood for `DELAY_TO_DELETION`, the account's data is purged for good. Since
/// account deletion is expensive, only a few are processed per pass.
///
/// Between passes it sleeps until the oldest request comes due. It needs no
/// wake-up: a new request falls due `DELAY_TO_DELETION` after it is made, later
/// than anything already waiting and far later than the ceiling.
async fn run_suspended_account_cleanup(state: AppState) {
    let ceiling = state.config.workers.sanitized().timed_task_idle_poll();
    while !state.stop.is_cancelled() {
        if let Err(e) = process_deletion_requests(&state).await {
            tracing::error!(error = %e, "suspended account cleanup failed");
        }
        let due = next_deletion_request_due(&state).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "could not find when the next deletion request is due");
            None
        });
        rest(
            &state.stop,
            timed_task_nap(due, DELETION_PASS_FLOOR, ceiling, rand::random()),
        )
        .await;
    }
}

/// `Scheduler::SuspendedUserCleanupScheduler`'s `interval: 1 minute`. A
/// request whose deletion keeps failing stays due, and is retried no faster.
const DELETION_PASS_FLOOR: Duration = Duration::from_secs(60);

/// Seconds until the oldest deletion request comes due, or `None` when there
/// are none. Uses this process's clock, as `process_deletion_requests` does.
async fn next_deletion_request_due(state: &AppState) -> anyhow::Result<Option<f64>> {
    let oldest = sqlx::query_scalar!("SELECT min(created_at) FROM account_deletion_requests")
        .fetch_one(&state.db)
        .await?;
    Ok(oldest.map(|created_at| {
        let due = created_at + crate::delete_account::DELAY_TO_DELETION;
        (due - chrono::Utc::now().naive_utc()).num_milliseconds() as f64 / 1000.0
    }))
}

/// `MAX_DELETIONS_PER_JOB`
const MAX_DELETIONS_PER_PASS: i64 = 10;

pub async fn process_deletion_requests(state: &AppState) -> anyhow::Result<()> {
    let cutoff = chrono::Utc::now().naive_utc() - crate::delete_account::DELAY_TO_DELETION;
    // `Admin::AccountDeletionWorker` does nothing for an account no longer
    // unavailable: one unsuspended without its request being removed is left.
    let due: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT r.account_id FROM account_deletion_requests r
           JOIN accounts a ON a.id = r.account_id
           WHERE r.created_at < $1
             AND (a.suspended_at IS NOT NULL OR a.requested_deletion_at IS NOT NULL)
           ORDER BY r.id ASC
           LIMIT $2"#,
        cutoff,
        MAX_DELETIONS_PER_PASS,
    )
    .fetch_all(&state.db)
    .await?;

    for account_id in due {
        // `Admin::AccountDeletionWorker`: both records are kept, only the data goes.
        if let Err(e) = crate::delete_account::call(
            state,
            account_id,
            crate::delete_account::Options::default(),
        )
        .await
        {
            tracing::error!(account_id, error = %e, "scheduled account deletion failed");
        }
    }
    Ok(())
}

#[cfg(test)]
mod idle_backoff_tests {
    use super::IdleBackoff;
    use std::time::Duration;
    use tokio::sync::Notify;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn a_stopped_instance_does_not_sleep_out_its_interval() {
        let wake = Notify::new();
        let stop = CancellationToken::new();
        let hour = Duration::from_secs(3600);
        let mut backoff = IdleBackoff::new(hour, hour);
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(5), backoff.idle(&wake, &stop))
            .await
            .expect("a stop should end an idle sleep at once");
    }

    #[tokio::test]
    async fn empty_passes_back_off_to_the_ceiling_and_work_resets_it() {
        let wake = Notify::new();
        let mut backoff = IdleBackoff::new(Duration::from_millis(1), Duration::from_millis(4));
        for _ in 0..4 {
            backoff.idle(&wake, &CancellationToken::new()).await;
        }
        assert_eq!(backoff.current, Duration::from_millis(4));
        backoff.reset();
        assert_eq!(backoff.current, Duration::from_millis(1));
    }

    #[tokio::test]
    async fn a_job_enqueued_before_the_loop_sleeps_is_not_missed() {
        let wake = Notify::new();
        let hour = Duration::from_secs(3600);
        let mut backoff = IdleBackoff::new(hour, hour);
        wake.notify_one();
        tokio::time::timeout(
            Duration::from_secs(5),
            backoff.idle(&wake, &CancellationToken::new()),
        )
        .await
        .expect("a wake-up raised before the sleep should end it at once");
    }

    #[tokio::test]
    async fn a_wake_up_shortens_the_next_sleep() {
        let wake = Notify::new();
        let mut backoff = IdleBackoff::new(Duration::from_millis(1), Duration::from_secs(3600));
        for _ in 0..3 {
            backoff.idle(&wake, &CancellationToken::new()).await;
        }
        assert_eq!(backoff.current, Duration::from_millis(8));
        wake.notify_one();
        backoff.idle(&wake, &CancellationToken::new()).await;
        assert_eq!(backoff.current, Duration::from_millis(1));
    }
}

#[cfg(test)]
mod timed_task_tests {
    use super::{jittered, timed_task_nap};
    use std::time::Duration;

    const FLOOR: Duration = Duration::from_secs(1);
    const CEILING: Duration = Duration::from_secs(300);
    /// No jitter, so an idle nap comes out exact.
    const NONE: f64 = 0.0;
    /// As much jitter as `rand::random` can produce.
    const MOST: f64 = 0.999_999;

    #[test]
    fn nothing_due_sleeps_for_the_ceiling() {
        assert_eq!(timed_task_nap(None, FLOOR, CEILING, NONE), CEILING);
    }

    #[test]
    fn an_item_due_soon_is_slept_until() {
        assert_eq!(
            timed_task_nap(Some(42.5), FLOOR, CEILING, NONE),
            Duration::from_millis(42_500)
        );
    }

    #[test]
    fn a_distant_item_waits_no_longer_than_the_ceiling() {
        assert_eq!(
            timed_task_nap(Some(86_400.0), FLOOR, CEILING, NONE),
            CEILING
        );
        assert_eq!(
            timed_task_nap(Some(f64::MAX), FLOOR, CEILING, NONE),
            CEILING
        );
    }

    #[test]
    fn an_overdue_item_does_not_spin() {
        assert_eq!(timed_task_nap(Some(-30.0), FLOOR, CEILING, NONE), FLOOR);
        assert_eq!(timed_task_nap(Some(0.0), FLOOR, CEILING, NONE), FLOOR);
        assert_eq!(
            timed_task_nap(Some(f64::NAN), FLOOR, CEILING, NONE),
            CEILING
        );
    }

    #[test]
    fn a_floor_above_the_ceiling_wins() {
        let floor = Duration::from_secs(120);
        let ceiling = Duration::from_secs(60);
        assert_eq!(timed_task_nap(None, floor, ceiling, NONE), floor);
        assert_eq!(timed_task_nap(None, floor, ceiling, MOST), floor);
        assert_eq!(timed_task_nap(Some(5.0), floor, ceiling, MOST), floor);
    }

    #[test]
    fn only_an_idle_nap_is_jittered() {
        let idle = timed_task_nap(None, FLOOR, CEILING, MOST);
        assert!(idle < CEILING, "an idle nap is shortened, got {idle:?}");
        assert!(
            idle >= CEILING.mul_f64(0.75),
            "by no more than a quarter, got {idle:?}"
        );
        assert_eq!(
            timed_task_nap(Some(42.5), FLOOR, CEILING, MOST),
            Duration::from_millis(42_500),
            "an item that falls due is woken for on time",
        );
    }

    #[test]
    fn jitter_only_ever_shortens_by_up_to_a_quarter() {
        let nap = Duration::from_secs(300);
        assert_eq!(jittered(nap, 0.0), nap);
        assert_eq!(jittered(nap, 0.5), Duration::from_millis(262_500));
        assert!(jittered(nap, MOST) >= Duration::from_secs(225));
        assert_eq!(jittered(nap, 7.0), Duration::from_secs(225));
        assert_eq!(jittered(nap, -1.0), nap);
    }
}

#[cfg(test)]
mod schedule_tests {
    use std::time::Duration;

    const MINUTE: Duration = Duration::from_secs(60);
    const HOUR: Duration = Duration::from_secs(60 * 60);
    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    /// The timed tasks run as often as Mastodon 4.7.2's *config/sidekiq.yml*
    /// schedules the scheduler each one ports. Its daily crons, at a random
    /// minute of a random early hour, are a day apart here too.
    #[test]
    fn timed_tasks_keep_sidekiq_yml_intervals() {
        let schedules = [
            // trends_refresh_scheduler: every: ['5m', first_in: '4m']
            (crate::trends::REFRESH_EVERY, 5 * MINUTE),
            (crate::trends::REFRESH_FIRST_IN, 4 * MINUTE),
            // trends_review_notifications_scheduler: every: '6h'
            (crate::trends::REVIEW_EVERY, 6 * HOUR),
            // indexing_scheduler: interval: 1 minute
            (crate::search::elasticsearch::indexing::INTERVAL, MINUTE),
            // vacuum_scheduler, user_cleanup_scheduler, ip_cleanup_scheduler:
            // daily crons
            (crate::vacuum::EVERY, DAY),
            (crate::email_subscriptions::CLEANUP_EVERY, DAY),
            (crate::ip_cleanup::EVERY, DAY),
            // accounts_statuses_cleanup_scheduler: interval: 1 minute
            (crate::statuses_cleanup::EVERY, MINUTE),
            // suspended_user_cleanup_scheduler: interval: 1 minute
            (super::DELETION_PASS_FLOOR, MINUTE),
            // software_update_check_scheduler: interval: 30 minutes
            (crate::software_updates::CHECK_INTERVAL, 30 * MINUTE),
            // auto_close_registrations_scheduler: interval: 1 hour
            (crate::auto_close_registrations::INTERVAL, HOUR),
            // collection_item_cleanup_scheduler: interval: 1 hour
            (crate::collection_item_cleanup::EVERY, HOUR),
            // repair_remote_collections_scheduler: every: ['24h', first_in: '1s']
            (crate::federation::featured_collections::REPAIR_EVERY, DAY),
            (
                crate::federation::featured_collections::REPAIR_FIRST_IN,
                Duration::from_secs(1),
            ),
        ];
        for (index, (ours, mastodon)) in schedules.into_iter().enumerate() {
            assert_eq!(ours, mastodon, "schedule {index}");
        }
    }
}
