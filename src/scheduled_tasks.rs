//! When the timed tasks run: Mastodon's *config/sidekiq.yml* schedule, kept in
//! a way that survives restarts and is shared by every process serving the
//! instance.
//!
//! Mastodon hands the schedule to sidekiq-scheduler, which hands it to
//! rufus-scheduler in every Sidekiq process:
//!
//!  -  an `every:` or `interval:` entry fires a period after the process
//!     starts (or after its `first_in`), then every period;
//!  -  a `cron:` entry fires on the wall clock, in the process's local time
//!     zone, whenever the process happens to be running. Several of them pick
//!     their minute and hour at random when *sidekiq.yml* is read at boot.
//!
//! Each scheduler is a sidekiq-unique-jobs `until_executed` job, so a second
//! one is not queued while the first waits or runs. Nothing remembers when a
//! job last ran: a cron slot passed while Sidekiq was down is not caught up,
//! and an interval starts again on every boot.
//!
//! Eunha's processes restart far more often than Mastodon's Sidekiq does —
//! every deploy swaps a blue and a green process — and a daily task whose
//! period starts again at each restart never ran at all. So eunha keeps the
//! schedule's kinds but remembers, in `eunha.scheduled_tasks`, which run each
//! task last finished:
//!
//!  -  A cron entry fires at its time of day in UTC, at a minute and hour
//!     picked from the entry's ranges once for the instance, from its domain
//!     ([`Cron::for_instance`]), so that every process and every restart
//!     agree on it. A slot that passed while no process was running is run
//!     [`CATCH_UP_DELAY`] after the next one starts.
//!  -  An interval entry fires `first_in` after the process starts, then
//!     every period, as rufus fires it; one whose last finished run is older
//!     than its period — a daily interval cut short by restarts — runs
//!     [`CATCH_UP_DELAY`] after start instead.
//!  -  A run takes a lease on its task's row ([`attempt`]): no other process
//!     runs the task while it is held, as the `until_executed` lock keeps a
//!     second scheduler from running, and the lease is renewed while the run
//!     lasts, so a process that dies mid-run lets another take the task up
//!     five minutes later rather than when a day's lock lapses. A cron slot
//!     already finished, or an interval run finished less than a period ago,
//!     is not run again by another process.
//!
//! The rows are kept in PostgreSQL rather than Redis because they are the
//! instance's memory of what it has done: losing them to an eviction or a
//! flushed Redis would make every task run at once, and every process serving
//! an instance shares its database.

use std::future::Future;
use std::time::Duration;

use chrono::{DateTime, DurationRound, Timelike, Utc};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::state::AppState;

/// How long after a process starts a missed or overdue run is made up: long
/// enough that a starting instance has settled, and that the process it
/// replaces during a deploy has stopped.
pub const CATCH_UP_DELAY: Duration = Duration::from_secs(2 * 60);

/// How long a run's lease lasts without being renewed. A process that dies
/// mid-run lets another take the task up after this.
pub const LEASE: Duration = Duration::from_secs(5 * 60);

/// How often a run in progress renews its lease.
const RENEW_EVERY: Duration = Duration::from_secs(60);

/// The longest a loop sleeps on the monotonic clock before looking at the wall
/// clock again, so that a host that was suspended, or whose clock was set,
/// still fires its cron entries on time.
const WALL_CLOCK_CHECK: Duration = Duration::from_secs(10 * 60);

/// A field of a cron entry: a fixed value, or `<%= Random.rand(lo..hi) %>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    At(u32),
    Random(u32, u32),
}

/// An entry's schedule, as *sidekiq.yml* writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schedule {
    /// `every:` or `interval:`: the first run `first_in` after start (the
    /// period itself unless the entry says otherwise), then every period.
    Every { every: Duration, first_in: Duration },
    /// `cron: 'M H * * *'`, or `'M * * * *'` without an hour.
    Cron { minute: Field, hour: Option<Field> },
}

/// How eunha runs an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runs {
    /// On the entry's schedule, through [`run`].
    OnSchedule,
    /// Woken when its next item falls due rather than on a fixed beat, which
    /// is when the jobs Mastodon's scheduler queues with `perform_at` run;
    /// each pass is still held to one process at a time by [`exclusive`].
    WhenDue,
    /// Once for the whole process, not per instance.
    ForTheProcess,
    /// Not at all; the divergence of this id says why.
    Not(&'static str),
}

/// One of the scheduled tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Task {
    /// The entry's key in *sidekiq.yml*, which also names its row in
    /// `eunha.scheduled_tasks`.
    pub name: &'static str,
    pub schedule: Schedule,
    pub runs: Runs,
}

const fn minutes(n: u64) -> Duration {
    Duration::from_secs(n * 60)
}

const fn every(period: Duration) -> Schedule {
    Schedule::Every {
        every: period,
        first_in: period,
    }
}

const fn daily(hours: (u32, u32)) -> Schedule {
    Schedule::Cron {
        minute: Field::Random(0, 59),
        hour: Some(Field::Random(hours.0, hours.1)),
    }
}

const fn task(name: &'static str, schedule: Schedule) -> Task {
    Task {
        name,
        schedule,
        runs: Runs::OnSchedule,
    }
}

pub const SCHEDULED_STATUSES: Task = Task {
    name: "scheduled_statuses_scheduler",
    schedule: every(minutes(5)),
    runs: Runs::WhenDue,
};
pub const TRENDS_REFRESH: Task = task(
    "trends_refresh_scheduler",
    Schedule::Every {
        every: minutes(5),
        first_in: minutes(4),
    },
);
pub const TRENDS_REVIEW_NOTIFICATIONS: Task = task(
    "trends_review_notifications_scheduler",
    every(minutes(6 * 60)),
);
pub const INDEXING: Task = task("indexing_scheduler", every(minutes(1)));
pub const VACUUM: Task = task("vacuum_scheduler", daily((3, 5)));
pub const FOLLOW_RECOMMENDATIONS: Task = task("follow_recommendations_scheduler", daily((6, 9)));
pub const USER_CLEANUP: Task = task("user_cleanup_scheduler", daily((4, 6)));
pub const IP_CLEANUP: Task = task("ip_cleanup_scheduler", daily((3, 5)));
pub const PGHERO: Task = Task {
    name: "pghero_scheduler",
    schedule: Schedule::Cron {
        minute: Field::At(0),
        hour: Some(Field::At(0)),
    },
    runs: Runs::Not("no-pghero-space-stats"),
};
pub const INSTANCE_REFRESH: Task = task(
    "instance_refresh_scheduler",
    Schedule::Cron {
        minute: Field::At(0),
        hour: None,
    },
);
pub const ACCOUNTS_STATUSES_CLEANUP: Task =
    task("accounts_statuses_cleanup_scheduler", every(minutes(1)));
pub const SUSPENDED_USER_CLEANUP: Task = Task {
    name: "suspended_user_cleanup_scheduler",
    schedule: every(minutes(1)),
    runs: Runs::WhenDue,
};
pub const SOFTWARE_UPDATE_CHECK: Task = Task {
    name: "software_update_check_scheduler",
    schedule: every(minutes(30)),
    runs: Runs::ForTheProcess,
};
pub const AUTO_CLOSE_REGISTRATIONS: Task =
    task("auto_close_registrations_scheduler", every(minutes(60)));
pub const FASP_FOLLOW_RECOMMENDATION_CLEANUP: Task = task(
    "fasp_follow_recommendation_cleanup_scheduler",
    every(minutes(24 * 60)),
);
pub const COLLECTION_ITEM_CLEANUP: Task =
    task("collection_item_cleanup_scheduler", every(minutes(60)));
pub const REPAIR_REMOTE_COLLECTIONS: Task = task(
    "repair_remote_collections_scheduler",
    Schedule::Every {
        every: minutes(24 * 60),
        first_in: Duration::from_secs(1),
    },
);

/// Every entry of Mastodon's *config/sidekiq.yml*, in its order.
pub const SIDEKIQ_YML: [Task; 17] = [
    SCHEDULED_STATUSES,
    TRENDS_REFRESH,
    TRENDS_REVIEW_NOTIFICATIONS,
    INDEXING,
    VACUUM,
    FOLLOW_RECOMMENDATIONS,
    USER_CLEANUP,
    IP_CLEANUP,
    PGHERO,
    INSTANCE_REFRESH,
    ACCOUNTS_STATUSES_CLEANUP,
    SUSPENDED_USER_CLEANUP,
    SOFTWARE_UPDATE_CHECK,
    AUTO_CLOSE_REGISTRATIONS,
    FASP_FOLLOW_RECOMMENDATION_CLEANUP,
    COLLECTION_ITEM_CLEANUP,
    REPAIR_REMOTE_COLLECTIONS,
];

/// What *config/initializers/sidekiq.rb* schedules instead of all of the above
/// on a self-destructing instance: `interval: ['1m']`.
pub const SELF_DESTRUCT: Task = task("self_destruct_scheduler", every(minutes(1)));

/// The announcements `ScheduledStatusesScheduler` publishes and unpublishes.
/// Mastodon queues a publication for its `scheduled_at`; eunha looks every
/// minute.
pub const SCHEDULED_ANNOUNCEMENTS: Task = task("eunha_scheduled_announcements", every(minutes(1)));

// ── Cron entries on the wall clock ─────────────────────────────────────────

/// A cron entry with its minute and hour settled for one instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cron {
    pub minute: u32,
    /// `None` for an entry that fires every hour.
    pub hour: Option<u32>,
}

impl Cron {
    /// The entry's minute and hour for the instance at `domain`. Where
    /// *sidekiq.yml* picks one at random, this picks it from the same range
    /// by a hash of the domain and the entry, so that the instance's
    /// processes, and the same process after a restart, all fire it at the
    /// same time, while instances sharing a host spread out over the range.
    pub fn for_instance(domain: &str, task: &Task) -> Option<Self> {
        let Schedule::Cron { minute, hour } = task.schedule else {
            return None;
        };
        let settle = |field: Field, which: &str| match field {
            Field::At(value) => value,
            Field::Random(lo, hi) => pick(domain, task.name, which, lo, hi),
        };
        Some(Self {
            minute: settle(minute, "minute"),
            hour: hour.map(|hour| settle(hour, "hour")),
        })
    }

    fn period(self) -> chrono::Duration {
        match self.hour {
            Some(_) => chrono::Duration::days(1),
            None => chrono::Duration::hours(1),
        }
    }

    /// The latest time at or before `now` this entry fires.
    pub fn previous(self, now: DateTime<Utc>) -> DateTime<Utc> {
        let base = match self.hour {
            Some(hour) => now
                .duration_trunc(chrono::Duration::days(1))
                .expect("a day divides a timestamp")
                .with_hour(hour)
                .expect("an hour of the day"),
            None => now
                .duration_trunc(chrono::Duration::hours(1))
                .expect("an hour divides a timestamp"),
        };
        let candidate = base.with_minute(self.minute).expect("a minute of the hour");
        if candidate > now {
            candidate - self.period()
        } else {
            candidate
        }
    }

    /// The first time after `now` this entry fires.
    pub fn next(self, now: DateTime<Utc>) -> DateTime<Utc> {
        self.previous(now) + self.period()
    }
}

/// A value in `lo..=hi`, the same every time for the same instance, entry and
/// field.
fn pick(domain: &str, task: &str, field: &str, lo: u32, hi: u32) -> u32 {
    let digest = Sha256::digest(format!("{domain}\n{task}\n{field}"));
    let mut head = [0u8; 8];
    head.copy_from_slice(&digest[..8]);
    let spread = u64::from(hi.saturating_sub(lo)) + 1;
    lo + u32::try_from(u64::from_be_bytes(head) % spread).expect("within a u32 range")
}

/// When a process started at `started` first looks at a cron entry: when it
/// next fires, or [`CATCH_UP_DELAY`] in, whichever is sooner, so that a slot
/// that passed while the instance was down is made up then.
pub fn first_cron_wake(cron: Cron, started: DateTime<Utc>) -> DateTime<Utc> {
    cron.next(started).min(started + delta(CATCH_UP_DELAY))
}

/// How long a process waits before an interval entry's first run: its
/// `first_in`, or no more than [`CATCH_UP_DELAY`] when the last run finished
/// more than a period ago, or never did.
pub fn first_every_wait(first_in: Duration, overdue: bool) -> Duration {
    if overdue {
        first_in.min(CATCH_UP_DELAY)
    } else {
        first_in
    }
}

/// How much sooner than a full period after the last run an interval entry
/// may run again: a tenth of the period, up to a minute. Two processes' beats
/// drift by the time a run takes; without this, the one whose beat came a
/// moment early would skip the run, and so would the other.
fn slack(period: Duration) -> Duration {
    (period / 10).min(Duration::from_secs(60))
}

fn delta(duration: Duration) -> chrono::Duration {
    chrono::Duration::from_std(duration).unwrap_or(chrono::Duration::MAX)
}

// ── Leases ────────────────────────────────────────────────────────────────

/// Which run a process is trying to make.
#[derive(Debug, Clone, Copy)]
pub enum Slot {
    /// A cron entry's run for this time; once one has finished, it is never
    /// run again.
    Cron(DateTime<Utc>),
    /// An interval entry's run, unless another finished less than about this
    /// period ago.
    Every(Duration),
    /// A run now, whenever the last one was.
    Any,
}

/// What came of an [`attempt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// This process ran it.
    Ran,
    /// The run had already been made.
    Done,
    /// Another process is running the task.
    Busy,
}

/// A lease on a task's row, taken by [`claim`].
#[derive(Debug)]
pub struct Lease {
    name: String,
    token: i64,
}

/// Take the lease on `name`'s row for the run `slot` names, if no process holds
/// it and that run has not been made.
pub async fn claim(db: &PgPool, name: &str, slot: Slot) -> sqlx::Result<Option<Lease>> {
    let token: i64 = rand::random();
    // The run's slot is the cron time, or for an interval run the time it
    // started. It is claimed if the last finished slot is older than `$2`,
    // or than `now() - $3` seconds.
    let (cron, seconds) = match slot {
        Slot::Cron(at) => (Some(at), None),
        Slot::Every(period) => (None, Some((period - slack(period)).as_secs_f64())),
        Slot::Any => (None, Some(0.0)),
    };
    let claimed: Option<i64> = sqlx::query_scalar(
        r#"INSERT INTO eunha.scheduled_tasks AS t
             (name, running_slot, leased_until, lease_token, last_started_at)
           VALUES ($1, COALESCE($2, now()), now() + make_interval(secs => $4), $5, now())
           ON CONFLICT (name) DO UPDATE
             SET running_slot = EXCLUDED.running_slot,
                 leased_until = EXCLUDED.leased_until,
                 lease_token = EXCLUDED.lease_token,
                 last_started_at = EXCLUDED.last_started_at
             WHERE (t.leased_until IS NULL OR t.leased_until < now())
               AND (t.last_slot IS NULL
                    OR t.last_slot < COALESCE($2, now() - make_interval(secs => $3)))
           RETURNING lease_token"#,
    )
    .bind(name)
    .bind(cron)
    .bind(seconds)
    .bind(LEASE.as_secs_f64())
    .bind(token)
    .fetch_optional(db)
    .await?;
    Ok(claimed.map(|token| Lease {
        name: name.to_owned(),
        token,
    }))
}

impl Lease {
    /// Keep the lease for another [`LEASE`]; false once it has been lost.
    pub async fn renew(&self, db: &PgPool) -> sqlx::Result<bool> {
        let renewed = sqlx::query(
            "UPDATE eunha.scheduled_tasks
             SET leased_until = now() + make_interval(secs => $3)
             WHERE name = $1 AND lease_token = $2",
        )
        .bind(&self.name)
        .bind(self.token)
        .bind(LEASE.as_secs_f64())
        .execute(db)
        .await?;
        Ok(renewed.rows_affected() == 1)
    }

    /// Record the run as finished, and let the lease go.
    pub async fn finish(self, db: &PgPool) -> sqlx::Result<()> {
        sqlx::query(
            "UPDATE eunha.scheduled_tasks
             SET last_slot = running_slot, last_finished_at = now(),
                 running_slot = NULL, leased_until = NULL, lease_token = NULL
             WHERE name = $1 AND lease_token = $2",
        )
        .bind(&self.name)
        .bind(self.token)
        .execute(db)
        .await?;
        Ok(())
    }
}

/// Whether the cron run for `at` has finished.
async fn finished(db: &PgPool, name: &str, at: DateTime<Utc>) -> sqlx::Result<bool> {
    let finished: Option<bool> =
        sqlx::query_scalar("SELECT last_slot >= $2 FROM eunha.scheduled_tasks WHERE name = $1")
            .bind(name)
            .bind(at)
            .fetch_optional(db)
            .await?
            .flatten();
    Ok(finished.unwrap_or(false))
}

/// Whether `name` last finished a run more than `period` ago, or never has.
pub async fn overdue(db: &PgPool, name: &str, period: Duration) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        r#"SELECT NOT EXISTS (
             SELECT 1 FROM eunha.scheduled_tasks
             WHERE name = $1 AND last_slot >= now() - make_interval(secs => $2))"#,
    )
    .bind(name)
    .bind(period.as_secs_f64())
    .fetch_one(db)
    .await
}

/// Run `work` as the run `slot` names, if this process can claim it: with the
/// lease renewed while it lasts, and recorded as finished once it has, whether
/// or not it succeeded — a scheduler is `retry: 0`, and its next run is the
/// retry.
pub async fn attempt(
    db: &PgPool,
    name: &str,
    slot: Slot,
    work: impl Future<Output = ()>,
) -> sqlx::Result<Outcome> {
    let Some(lease) = claim(db, name, slot).await? else {
        let done = match slot {
            Slot::Cron(at) => finished(db, name, at).await?,
            Slot::Every(_) | Slot::Any => false,
        };
        return Ok(if done { Outcome::Done } else { Outcome::Busy });
    };
    let renew = async {
        loop {
            tokio::time::sleep(RENEW_EVERY).await;
            match lease.renew(db).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(task = %lease.name, "lost the lease of a scheduled task still running");
                }
                Err(error) => {
                    tracing::warn!(task = %lease.name, %error, "could not renew a scheduled task's lease");
                }
            }
        }
    };
    tokio::select! {
        () = work => {}
        () = renew => {}
    }
    lease.finish(db).await?;
    Ok(Outcome::Ran)
}

/// Run one pass of a task that is woken when something falls due, unless
/// another process is running one: the `until_executed` lock alone.
pub async fn exclusive(state: &AppState, task: &Task, work: impl Future<Output = ()>) {
    if let Err(error) = attempt(&state.db, task.name, Slot::Any, work).await {
        tracing::error!(task = task.name, %error, "could not take a scheduled task's lease");
    }
}

// ── The loops ─────────────────────────────────────────────────────────────

/// Run `task` on its schedule until the instance stops, each run `work`.
pub async fn run<F, Fut>(state: AppState, task: &'static Task, work: F)
where
    F: Fn(AppState) -> Fut,
    Fut: Future<Output = ()>,
{
    match task.schedule {
        Schedule::Every { every, first_in } => {
            run_every(&state, task.name, every, first_in, work).await;
        }
        Schedule::Cron { .. } => {
            let cron = Cron::for_instance(&state.instance.domain, task).expect("a cron entry");
            tracing::debug!(
                task = task.name,
                minute = cron.minute,
                hour = cron.hour,
                "scheduled on the UTC clock"
            );
            run_cron(&state, task.name, cron, work).await;
        }
    }
}

async fn run_every<F, Fut>(
    state: &AppState,
    name: &str,
    every: Duration,
    first_in: Duration,
    work: F,
) where
    F: Fn(AppState) -> Fut,
    Fut: Future<Output = ()>,
{
    let overdue = overdue(&state.db, name, every)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(task = name, %error, "could not tell when a scheduled task last ran");
            false
        });
    crate::background::rest(&state.stop, first_every_wait(first_in, overdue)).await;
    while !state.stop.is_cancelled() {
        if let Err(error) = attempt(&state.db, name, Slot::Every(every), work(state.clone())).await
        {
            tracing::error!(task = name, %error, "could not take a scheduled task's lease");
        }
        crate::background::rest(&state.stop, every).await;
    }
}

async fn run_cron<F, Fut>(state: &AppState, name: &str, cron: Cron, work: F)
where
    F: Fn(AppState) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut wake = first_cron_wake(cron, Utc::now());
    loop {
        sleep_until(&state.stop, wake).await;
        if state.stop.is_cancelled() {
            break;
        }
        let now = Utc::now();
        let slot = cron.previous(now);
        wake = match attempt(&state.db, name, Slot::Cron(slot), work(state.clone())).await {
            Ok(Outcome::Ran | Outcome::Done) => cron.next(now),
            // Look again once the other process's lease would have lapsed, in
            // case it died before finishing.
            Ok(Outcome::Busy) => cron.next(now).min(now + delta(LEASE)),
            Err(error) => {
                tracing::error!(task = name, %error, "could not take a scheduled task's lease");
                cron.next(now).min(now + delta(LEASE))
            }
        };
    }
}

/// Sleep until the wall clock reads `at`, or until the instance is stopped.
async fn sleep_until(stop: &tokio_util::sync::CancellationToken, at: DateTime<Utc>) {
    while !stop.is_cancelled() {
        let Ok(left) = (at - Utc::now()).to_std() else {
            return;
        };
        if left.is_zero() {
            return;
        }
        crate::background::rest(stop, left.min(WALL_CLOCK_CHECK)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 10, h, m, s).unwrap()
    }

    const VACUUM_AT_4_17: Cron = Cron {
        minute: 17,
        hour: Some(4),
    };

    #[test]
    fn a_daily_entry_fires_at_its_minute_of_its_hour() {
        assert_eq!(VACUUM_AT_4_17.next(at(3, 0, 0)), at(4, 17, 0));
        assert_eq!(VACUUM_AT_4_17.next(at(4, 16, 59)), at(4, 17, 0));
        // At the very minute, the next one is tomorrow's.
        assert_eq!(
            VACUUM_AT_4_17.next(at(4, 17, 0)),
            at(4, 17, 0) + chrono::Duration::days(1)
        );
        assert_eq!(
            VACUUM_AT_4_17.next(at(23, 59, 59)),
            at(4, 17, 0) + chrono::Duration::days(1)
        );
        assert_eq!(VACUUM_AT_4_17.previous(at(4, 17, 0)), at(4, 17, 0));
        assert_eq!(
            VACUUM_AT_4_17.previous(at(4, 16, 59)),
            at(4, 17, 0) - chrono::Duration::days(1)
        );
        assert_eq!(VACUUM_AT_4_17.previous(at(12, 0, 0)), at(4, 17, 0));
    }

    #[test]
    fn an_hourly_entry_fires_on_the_hour() {
        let hourly = Cron::for_instance("example.com", &INSTANCE_REFRESH).unwrap();
        assert_eq!(
            hourly,
            Cron {
                minute: 0,
                hour: None
            }
        );
        assert_eq!(hourly.next(at(3, 0, 0)), at(4, 0, 0));
        assert_eq!(hourly.next(at(3, 59, 59)), at(4, 0, 0));
        assert_eq!(hourly.previous(at(3, 59, 59)), at(3, 0, 0));
        assert_eq!(
            hourly.next(at(23, 30, 0)),
            at(0, 0, 0) + chrono::Duration::days(1)
        );
    }

    #[test]
    fn an_instance_keeps_its_picked_time_and_instances_spread_out() {
        let first = Cron::for_instance("example.com", &VACUUM).unwrap();
        assert_eq!(first, Cron::for_instance("example.com", &VACUUM).unwrap());
        let mut minutes = std::collections::BTreeSet::new();
        let mut hours = std::collections::BTreeSet::new();
        for n in 0..500 {
            let cron = Cron::for_instance(&format!("i{n}.example"), &VACUUM).unwrap();
            assert!(cron.minute <= 59, "{cron:?}");
            let hour = cron.hour.unwrap();
            assert!((3..=5).contains(&hour), "{cron:?}");
            minutes.insert(cron.minute);
            hours.insert(hour);
        }
        assert_eq!(hours.len(), 3, "every hour of the range is used");
        assert!(minutes.len() > 50, "minutes spread: {}", minutes.len());
        // Entries with the same range are picked apart, as two Random.rand
        // calls are.
        let differ = (0..50).any(|n| {
            let domain = format!("i{n}.example");
            Cron::for_instance(&domain, &VACUUM) != Cron::for_instance(&domain, &IP_CLEANUP)
        });
        assert!(differ);
    }

    #[test]
    fn a_missed_slot_is_made_up_soon_after_start() {
        // Started at noon, the 04:17 slot long past: look in two minutes.
        assert_eq!(first_cron_wake(VACUUM_AT_4_17, at(12, 0, 0)), at(12, 2, 0));
        // Started a minute before the slot: it fires on time.
        assert_eq!(first_cron_wake(VACUUM_AT_4_17, at(4, 16, 0)), at(4, 17, 0));
    }

    #[test]
    fn an_overdue_interval_runs_soon_after_start() {
        let day = minutes(24 * 60);
        assert_eq!(first_every_wait(day, true), CATCH_UP_DELAY);
        assert_eq!(first_every_wait(day, false), day);
        // A sooner first_in is kept either way.
        let second = Duration::from_secs(1);
        assert_eq!(first_every_wait(second, true), second);
        assert_eq!(first_every_wait(minutes(1), true), minutes(1));
    }

    #[test]
    fn an_interval_may_run_a_little_early() {
        assert_eq!(slack(minutes(1)), Duration::from_secs(6));
        assert_eq!(slack(minutes(24 * 60)), minutes(1));
    }

    /// Every entry of Mastodon 4.7.3's *config/sidekiq.yml*, in its order,
    /// with its schedule as written there.
    #[test]
    fn timed_tasks_keep_sidekiq_yml_schedules() {
        let random = |lo, hi| Field::Random(lo, hi);
        let yml = [
            // every: '5m'
            ("scheduled_statuses_scheduler", every(minutes(5))),
            // every: ['5m', first_in: '4m']
            (
                "trends_refresh_scheduler",
                Schedule::Every {
                    every: minutes(5),
                    first_in: minutes(4),
                },
            ),
            // every: '6h'
            ("trends_review_notifications_scheduler", every(minutes(360))),
            // interval: 1 minute
            ("indexing_scheduler", every(minutes(1))),
            // cron: '<%= Random.rand(0..59) %> <%= Random.rand(3..5) %> * * *'
            (
                "vacuum_scheduler",
                Schedule::Cron {
                    minute: random(0, 59),
                    hour: Some(random(3, 5)),
                },
            ),
            // cron: '<%= Random.rand(0..59) %> <%= Random.rand(6..9) %> * * *'
            (
                "follow_recommendations_scheduler",
                Schedule::Cron {
                    minute: random(0, 59),
                    hour: Some(random(6, 9)),
                },
            ),
            // cron: '<%= Random.rand(0..59) %> <%= Random.rand(4..6) %> * * *'
            (
                "user_cleanup_scheduler",
                Schedule::Cron {
                    minute: random(0, 59),
                    hour: Some(random(4, 6)),
                },
            ),
            // cron: '<%= Random.rand(0..59) %> <%= Random.rand(3..5) %> * * *'
            (
                "ip_cleanup_scheduler",
                Schedule::Cron {
                    minute: random(0, 59),
                    hour: Some(random(3, 5)),
                },
            ),
            // cron: '0 0 * * *'
            (
                "pghero_scheduler",
                Schedule::Cron {
                    minute: Field::At(0),
                    hour: Some(Field::At(0)),
                },
            ),
            // cron: '0 * * * *'
            (
                "instance_refresh_scheduler",
                Schedule::Cron {
                    minute: Field::At(0),
                    hour: None,
                },
            ),
            // interval: 1 minute
            ("accounts_statuses_cleanup_scheduler", every(minutes(1))),
            // interval: 1 minute
            ("suspended_user_cleanup_scheduler", every(minutes(1))),
            // interval: 30 minutes
            ("software_update_check_scheduler", every(minutes(30))),
            // interval: 1 hour
            ("auto_close_registrations_scheduler", every(minutes(60))),
            // interval: 1 day
            (
                "fasp_follow_recommendation_cleanup_scheduler",
                every(minutes(24 * 60)),
            ),
            // interval: 1 hour
            ("collection_item_cleanup_scheduler", every(minutes(60))),
            // every: ['24h', first_in: '1s']
            (
                "repair_remote_collections_scheduler",
                Schedule::Every {
                    every: minutes(24 * 60),
                    first_in: Duration::from_secs(1),
                },
            ),
        ];
        for ((name, schedule), task) in yml.into_iter().zip(SIDEKIQ_YML) {
            assert_eq!(task.name, name);
            assert_eq!(task.schedule, schedule, "{name}");
        }
        // config/initializers/sidekiq.rb: interval: ['1m']
        assert_eq!(SELF_DESTRUCT.schedule, every(minutes(1)));
        // Run once for the process (src/software_updates.rs), at its interval.
        assert_eq!(SOFTWARE_UPDATE_CHECK.runs, Runs::ForTheProcess);
        assert_eq!(crate::software_updates::CHECK_INTERVAL, minutes(30));
    }
}
