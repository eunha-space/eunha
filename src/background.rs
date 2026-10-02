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
    let mut tasks = vec![
        until_stopped(
            &state,
            "scheduled statuses",
            run_scheduled_statuses(state.clone()),
        ),
        until_stopped(&state, "poll expiry", run_poll_expiry(state.clone())),
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
            "email subscription cleanup",
            crate::email_subscriptions::run_cleanup(state.clone()),
        ),
        until_stopped(
            &state,
            "delivery cleanup",
            crate::federation::delivery::run_delivery_cleanup(state.clone()),
        ),
        until_stopped(
            &state,
            "inbox cleanup",
            crate::api::ap::inbox::run_inbox_cleanup(state.clone()),
        ),
        until_stopped(
            &state,
            "media queue",
            crate::api::mastodon::media::run_media_queue(state.clone()),
        ),
        until_stopped(
            &state,
            "import queue",
            crate::portability::import::run_queue(state.clone()),
        ),
        until_stopped(
            &state,
            "archive queue",
            crate::portability::backup::run_queue(state.clone()),
        ),
        until_stopped(
            &state,
            "import and archive vacuum",
            crate::portability::run_vacuum(state.clone()),
        ),
        until_stopped(&state, "vacuum", crate::vacuum::run(state.clone())),
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
    ];

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
    for index in 0..workers.inbox_workers {
        tasks.push(until_stopped(
            &state,
            "inbox queue",
            crate::api::ap::inbox::run_inbox_queue(state.clone(), index),
        ));
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
        inbox_workers = workers.inbox_workers,
        inbox_concurrency = workers.inbox_concurrency,
        "background queues started"
    );
    tasks
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
    pub inbox: tokio::sync::Notify,
    pub media: tokio::sync::Notify,
    /// A job was queued (crate::jobs).
    pub jobs: tokio::sync::Notify,
    /// A data import was confirmed.
    pub imports: tokio::sync::Notify,
    /// An archive takeout was requested.
    pub backups: tokio::sync::Notify,
    /// A scheduled status was created or moved.
    pub scheduled_statuses: tokio::sync::Notify,
    /// A poll was created, or when it ends changed.
    pub polls: tokio::sync::Notify,
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
/// Scheduled statuses, poll expiry and suspended account cleanup used to run
/// every minute or two whether or not anything was due, and every pass opened a
/// database connection, so an idle tenant was never without one for long. Most
/// tenants have nothing scheduled and no poll running, and for them this is the
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

/// Publish one scheduled status.
///
/// The split at the `statuses` INSERT is what makes retrying safe: everything
/// before it either succeeds or leaves the database untouched, so a failure
/// there can be tried again. Once the row is inserted the post exists and the
/// schedule is spent — every later step is therefore best-effort and logged,
/// never propagated, because returning an error would re-run this function and
/// post the status a second time.
async fn publish_one(
    state: &AppState,
    scheduled_id: i64,
    account_id: i64,
    params: &Option<serde_json::Value>,
) -> Result<(), PublishError> {
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

    let text = params["text"].as_str().unwrap_or("").to_string();
    let visibility = params["visibility"]
        .as_str()
        .unwrap_or("public")
        .to_string();
    let spoiler_text = params["spoiler_text"].as_str().unwrap_or("").to_string();
    let sensitive = params["sensitive"].as_bool().unwrap_or(false);
    let language = params["language"].as_str().map(str::to_string);
    let in_reply_to_id: Option<i64> = params["in_reply_to_id"]
        .as_str()
        .and_then(|s| s.parse::<i64>().ok());

    // Resolve the parent's account for in_reply_to_account_id and replies_count
    let in_reply_to_account_id: Option<i64> = if let Some(parent_id) = in_reply_to_id {
        sqlx::query_scalar!(
            "SELECT account_id FROM statuses WHERE id = $1 AND deleted_at IS NULL",
            parent_id,
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    let is_reply = in_reply_to_id.is_some();

    use crate::api::mastodon::statuses::{
        extract_hashtags, extract_mention_handles, resolve_mention_accounts, store_status_mentions,
        store_statuses_tags,
    };

    let domain = &state.instance.domain;

    let hashtags = extract_hashtags(&text);
    let mention_handles = extract_mention_handles(&text);
    let resolved = resolve_mention_accounts(state, &mention_handles, domain).await;

    let status_id = crate::snowflake::next_id();
    let uri = format!(
        "https://{}/users/{}/statuses/{}",
        domain, account.username, status_id
    );

    let visibility_int = crate::db::models::vis::from_str(&visibility);
    let status = sqlx::query_as!(
        crate::db::models::Status,
        r#"INSERT INTO statuses
             (id, account_id, text, spoiler_text, visibility,
              language, sensitive, in_reply_to_id, in_reply_to_account_id, reply, uri, url, local, created_at, updated_at)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$11, true, now(), now())
           RETURNING *"#,
        status_id, account.id, text, spoiler_text, visibility_int,
        language, sensitive, in_reply_to_id, in_reply_to_account_id, is_reply, uri,
    )
    .fetch_one(&state.db)
    .await
    .map_err(|e| classify_db(e, "insert scheduled status"))?;

    // ── Past this point the status exists; failures are logged, not returned ──

    if let Err(e) = store_statuses_tags(state, status.id, account.id, &hashtags).await {
        tracing::error!(scheduled_id, status_id = status.id, error = %e, "scheduled status published without its hashtags");
    }
    if let Err(e) = store_status_mentions(state, status.id, &resolved).await {
        tracing::error!(scheduled_id, status_id = status.id, error = %e, "scheduled status published without its mentions");
    }
    // `PostStatusService#postprocess_status!`: `Trends.tags.register`.
    crate::trends::register_tags(state, status.id).await;
    // `Status`'s `after_create_commit :trigger_create_webhooks`.
    crate::moderation::webhooks::trigger(
        state,
        "status.created",
        crate::moderation::webhooks::Object::Status(status.id),
    )
    .await;
    crate::fasp::events::status_created(state, status.id).await;

    if let Err(e) = crate::counters::on_status_created(
        &state.db,
        account.id,
        visibility_int,
        in_reply_to_id,
        status.created_at,
    )
    .await
    {
        tracing::error!(scheduled_id, error = %e, "failed to count a published status");
    }
    // `update_index('statuses', :proper)` and the account's stats.
    crate::search::elasticsearch::indexing::status(state, status.id).await;
    crate::search::elasticsearch::indexing::account(state, account.id).await;

    // Attach media ids if any
    if let Some(ids) = params["media_ids"].as_array() {
        for id_val in ids {
            if let Some(id_str) = id_val.as_str() {
                if let Ok(media_id) = id_str.parse::<i64>() {
                    let attached = sqlx::query!(
                        "UPDATE media_attachments SET status_id = $1 WHERE id = $2 AND account_id = $3 AND status_id IS NULL",
                        status.id, media_id, account.id,
                    )
                    .execute(&state.db)
                    .await;
                    if let Err(e) = attached {
                        tracing::error!(scheduled_id, status_id = status.id, media_id, error = %e, "failed to attach media to scheduled status");
                    }
                }
            }
        }
    }

    // Create poll if present
    if let Some(poll) = params["poll"].as_object() {
        if let Some(options) = poll.get("options").and_then(|o| o.as_array()) {
            if options.len() >= 2 {
                let expires_in = poll.get("expires_in").and_then(|v| v.as_i64());
                let multiple = poll
                    .get("multiple")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let hide_totals = poll
                    .get("hide_totals")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let expires_at = expires_in
                    .map(|s| chrono::Utc::now().naive_utc() + chrono::Duration::seconds(s));
                let opts: Vec<String> = options
                    .iter()
                    .filter_map(|o| o.as_str())
                    .map(|o| o.to_string())
                    .collect();
                let poll_created = sqlx::query!(
                    r#"INSERT INTO polls
                         (status_id, account_id, options, multiple, hide_totals, expires_at, created_at, updated_at)
                       VALUES ($1,$2,$3,$4,$5,$6,now(),now())"#,
                    status.id, account.id, &opts as &[String], multiple, hide_totals, expires_at,
                )
                .execute(&state.db)
                .await;
                match poll_created {
                    Ok(_) => state.queues.polls.notify_one(),
                    Err(e) => {
                        tracing::error!(scheduled_id, status_id = status.id, error = %e, "scheduled status published without its poll")
                    }
                }
            }
        }
    }

    // Publish to streaming and fan-out to feeds
    let mut status_with_uri = status.clone();
    status_with_uri.uri = Some(uri);
    // `LinkCrawlWorker.perform_async(@status.id)`.
    crate::preview_card::crawl(state, status_with_uri.id);
    // `PostStatusService#process_email_subscriptions!`, which a scheduled
    // post goes through when it is published.
    crate::email_subscriptions::status_posted(
        state,
        &crate::email_subscriptions::PostedStatus {
            id: status_with_uri.id,
            account_id: account.id,
            visibility: visibility.clone(),
            in_reply_to_id,
            in_reply_to_account_id,
        },
    )
    .await;
    // Fan-out to follower home feeds and list feeds
    let tag_ids: Vec<i64> = sqlx::query_scalar!(
        "SELECT tag_id FROM statuses_tags WHERE status_id = $1",
        status.id,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    let mut redis = state.redis.clone();
    let db = state.db.clone();
    let author_id = account.id;
    let sid = status.id;
    let vis = visibility.clone();
    crate::feed::fanout_new_status(&mut redis, &state.redis_keys, &db, author_id, sid, &tag_ids)
        .await;
    crate::feed::fanout_to_lists(
        &mut redis,
        &state.redis_keys,
        &db,
        author_id,
        sid,
        in_reply_to_account_id,
        &vis,
    )
    .await;
    crate::streaming::fan_out::distribute(state, sid, false).await;

    // Send mention notifications (mirrors post_status)
    let mut notified = std::collections::HashSet::new();
    if let Some(parent_account_id) = in_reply_to_account_id {
        crate::push::create_and_push(
            state,
            parent_account_id,
            account.id,
            "mention",
            Some(status.id),
            format!("{} mentioned you", account.display_name),
            account.acct().clone(),
            crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &account),
        )
        .await;
        notified.insert(parent_account_id);
    }
    for (_, mentioned) in &resolved {
        if mentioned.id == account.id || notified.contains(&mentioned.id) {
            continue;
        }
        crate::push::create_and_push(
            state,
            mentioned.id,
            account.id,
            "mention",
            Some(status.id),
            format!("{} mentioned you", account.display_name),
            account.acct().clone(),
            crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &account),
        )
        .await;
        notified.insert(mentioned.id);
    }

    Ok(())
}

// ── Suspended account cleanup ─────────────────────────────────────────────

/// Mastodon's `Scheduler::SuspendedUserCleanupScheduler`: once a suspension has
/// stood for `DELAY_TO_DELETION`, the account's data is purged for good. Since
/// account deletion is expensive, only a few are processed per pass.
///
/// Between passes it sleeps until the oldest request comes due. It needs no
/// wake-up: a new request falls due `DELAY_TO_DELETION` after it is made, later
/// than anything already waiting and far later than the ceiling.
/// `Scheduler::Trends::RefreshScheduler`: rescore trends every five minutes.
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

async fn run_trends_refresh(state: AppState) {
    while !state.stop.is_cancelled() {
        if let Err(e) = crate::trends::refresh(&state).await {
            tracing::error!(error = %e, "trends refresh failed");
        }
        rest(&state.stop, crate::trends::REFRESH_EVERY).await;
    }
}

/// `Scheduler::Trends::ReviewNotificationsScheduler`: ask staff hourly about
/// trends waiting on a review. The first pass waits its hour, as a newly
/// started Sidekiq scheduler does.
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

/// The two minutes this task always waited between passes. A request whose
/// deletion keeps failing stays due, and is retried no faster than before.
const DELETION_PASS_FLOOR: Duration = Duration::from_secs(120);

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

// ── Poll expiry notifier ──────────────────────────────────────────────────

/// How far the notifier has got: every poll that ended before `expires_at` has
/// been handled, and of those ending exactly then, every one up to `id`.
/// Ordering by both lets a pass stop at its batch limit without skipping or
/// repeating a poll that ends at the same instant as the last one handled.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PollExpiryMark {
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub id: i64,
}

/// Polls handled per pass. A pass that fills it runs again straight away.
const POLL_EXPIRY_BATCH: i64 = 100;

async fn run_poll_expiry(state: AppState) {
    let ceiling = state.config.workers.sanitized().timed_task_idle_poll();
    let mut mark = None;
    while !state.stop.is_cancelled() {
        let floor = match notify_polls_expired_after(&state, mark).await {
            Ok((reached, handled)) => {
                mark = Some(reached);
                if handled as i64 == POLL_EXPIRY_BATCH {
                    continue;
                }
                TIMED_TASK_FLOOR
            }
            Err(e) => {
                tracing::error!(error = %e, "poll expiry task failed");
                TIMED_TASK_FAILURE_FLOOR
            }
        };
        let due = next_poll_expiry(&state).await.unwrap_or_else(|e| {
            tracing::error!(error = %e, "could not find when the next poll ends");
            None
        });
        sleep_or_wake(
            &state.queues.polls,
            &state.stop,
            timed_task_nap(due, floor, ceiling, rand::random()),
        )
        .await;
    }
}

/// Seconds until the next running poll ends, or `None` when none is running.
pub async fn next_poll_expiry(state: &AppState) -> anyhow::Result<Option<f64>> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXTRACT(EPOCH FROM min(expires_at::timestamptz) - now())::float8
           FROM polls
           WHERE expires_at::timestamptz > now()"#,
    )
    .fetch_one(&state.db)
    .await?)
}

/// Notify the author and voters of every poll that ended in the last two
/// minutes.
pub async fn notify_expired_polls(state: &AppState) -> anyhow::Result<()> {
    notify_polls_expired_after(state, None).await.map(|_| ())
}

/// Notify the author and voters of the polls that ended after `mark` and by
/// now, oldest first, and return how far that got and how many polls it
/// handled. A pass that handled `POLL_EXPIRY_BATCH` stopped at the limit.
///
/// Without a mark it starts two minutes back. That window used to be the whole
/// of it: a pass a minute over the last two minutes, so every poll fell inside
/// two passes — the notification was deduplicated, but a local poll's
/// ActivityPub `Update` was enqueued twice — and a pass more than two minutes
/// late would have missed polls outright. Now it is only where a freshly
/// started process begins, so a poll that ended while the process restarted is
/// still notified, and a notifier that sleeps for many minutes still reaches
/// every poll that ended in the meantime, once.
pub async fn notify_polls_expired_after(
    state: &AppState,
    mark: Option<PollExpiryMark>,
) -> anyhow::Result<(PollExpiryMark, usize)> {
    let now = sqlx::query_scalar!(r#"SELECT now() AS "now!""#)
        .fetch_one(&state.db)
        .await?;
    let mark = mark.unwrap_or(PollExpiryMark {
        expires_at: now - chrono::Duration::minutes(2),
        id: i64::MAX,
    });
    let expired = sqlx::query!(
        r#"SELECT p.id, p.status_id, p.account_id, p.expires_at::timestamptz AS "expires_at!"
           FROM polls p
           WHERE (p.expires_at::timestamptz, p.id) > ($1::timestamptz, $2::bigint)
             AND p.expires_at::timestamptz <= $3::timestamptz
           ORDER BY p.expires_at::timestamptz ASC, p.id ASC
           LIMIT $4"#,
        mark.expires_at,
        mark.id,
        now,
        POLL_EXPIRY_BATCH,
    )
    .fetch_all(&state.db)
    .await?;

    let handled = expired.len();
    let full = handled as i64 == POLL_EXPIRY_BATCH;
    let reached = match expired.last() {
        Some(last) if full => PollExpiryMark {
            expires_at: last.expires_at,
            id: last.id,
        },
        _ => PollExpiryMark {
            expires_at: now,
            id: i64::MAX,
        },
    };

    for poll in expired {
        if let Err(e) =
            crate::api::mastodon::polls::federate_poll_update(state, poll.status_id).await
        {
            tracing::warn!(poll_id = poll.id, error = %e, "failed to enqueue expired poll ActivityPub update");
        }

        // Collect recipients: poll author + all voters
        let mut recipients: Vec<i64> = vec![poll.account_id];
        let voters = sqlx::query_scalar!(
            "SELECT DISTINCT account_id FROM poll_votes WHERE poll_id = $1",
            poll.id,
        )
        .fetch_all(&state.db)
        .await?;
        recipients.extend(voters);
        recipients.dedup();

        for recipient_id in recipients {
            crate::push::create_and_push(
                state,
                recipient_id,
                poll.account_id,
                "poll",
                Some(poll.status_id),
                "A poll you voted in has ended".into(),
                "".into(),
                "".into(),
            )
            .await;
        }
    }
    Ok((reached, handled))
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
