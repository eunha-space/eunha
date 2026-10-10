//! The timed tasks' schedule (src/scheduled_tasks.rs): one run per slot across
//! the processes serving an instance, none at once, and a missed run made up
//! after a restart. Each test's two "processes" are two connection pools on
//! the instance's database, the server's and the test's own, which share
//! nothing but that database, as two eunha processes do.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use eunha::scheduled_tasks::{self as tasks, Outcome, Slot};

use crate::helpers::TestContext;

/// A run that takes a moment, long enough for another process to try the
/// same task while it is under way.
async fn counted(runs: &AtomicUsize) {
    runs.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn test_two_processes_run_a_cron_slot_once() {
    let ctx = TestContext::new("sched-cron").await;
    let slot = Slot::Cron(Utc.with_ymd_and_hms(2026, 10, 10, 4, 17, 0).unwrap());
    let runs = AtomicUsize::new(0);

    let (one, other) = tokio::join!(
        tasks::attempt(&ctx.state.db, "vacuum_scheduler", slot, counted(&runs)),
        tasks::attempt(&ctx.db, "vacuum_scheduler", slot, counted(&runs)),
    );
    let outcomes = [one.unwrap(), other.unwrap()];
    assert_eq!(
        outcomes.iter().filter(|o| **o == Outcome::Ran).count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // Once it has finished, neither process runs that slot again…
    for db in [&ctx.state.db, &ctx.db] {
        let again = tasks::attempt(db, "vacuum_scheduler", slot, counted(&runs))
            .await
            .unwrap();
        assert_eq!(again, Outcome::Done);
    }
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // …but the next day's is run, by whichever gets to it.
    let tomorrow = Slot::Cron(Utc.with_ymd_and_hms(2026, 10, 11, 4, 17, 0).unwrap());
    let next = tasks::attempt(&ctx.db, "vacuum_scheduler", tomorrow, counted(&runs))
        .await
        .unwrap();
    assert_eq!(next, Outcome::Ran);
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_two_processes_run_an_interval_once_per_period() {
    let ctx = TestContext::new("sched-every").await;
    let period = Slot::Every(Duration::from_secs(3600));
    let runs = AtomicUsize::new(0);

    let (one, other) = tokio::join!(
        tasks::attempt(
            &ctx.state.db,
            "collection_item_cleanup_scheduler",
            period,
            counted(&runs)
        ),
        tasks::attempt(
            &ctx.db,
            "collection_item_cleanup_scheduler",
            period,
            counted(&runs)
        ),
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1, "{one:?} {other:?}");
    // The other process's beat comes round within the hour: not again.
    let again = tasks::attempt(
        &ctx.db,
        "collection_item_cleanup_scheduler",
        period,
        counted(&runs),
    )
    .await
    .unwrap();
    assert_ne!(again, Outcome::Ran);
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // An hour on, it is due again.
    sqlx::query(
        "UPDATE eunha.scheduled_tasks SET last_slot = last_slot - interval '1 hour'
         WHERE name = 'collection_item_cleanup_scheduler'",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let due = tasks::attempt(
        &ctx.db,
        "collection_item_cleanup_scheduler",
        period,
        counted(&runs),
    )
    .await
    .unwrap();
    assert_eq!(due, Outcome::Ran);
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_a_run_whose_process_died_is_taken_up_once_its_lease_lapses() {
    let ctx = TestContext::new("sched-lease").await;
    let slot = Slot::Cron(Utc.with_ymd_and_hms(2026, 10, 10, 4, 17, 0).unwrap());
    let runs = AtomicUsize::new(0);

    // A process claims the slot, and dies without finishing.
    let lease = tasks::claim(&ctx.state.db, "ip_cleanup_scheduler", slot)
        .await
        .unwrap()
        .expect("claimed");
    let busy = tasks::attempt(&ctx.db, "ip_cleanup_scheduler", slot, counted(&runs))
        .await
        .unwrap();
    assert_eq!(busy, Outcome::Busy, "the lease holds while it lasts");
    drop(lease);

    sqlx::query(
        "UPDATE eunha.scheduled_tasks SET leased_until = now() - interval '1 second'
         WHERE name = 'ip_cleanup_scheduler'",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let taken = tasks::attempt(&ctx.db, "ip_cleanup_scheduler", slot, counted(&runs))
        .await
        .unwrap();
    assert_eq!(taken, Outcome::Ran);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn test_a_task_last_run_before_the_downtime_is_overdue() {
    let ctx = TestContext::new("sched-overdue").await;
    let day = Duration::from_secs(24 * 60 * 60);
    let name = "fasp_follow_recommendation_cleanup_scheduler";

    // Never run: overdue, so a starting process makes it up soon.
    assert!(tasks::overdue(&ctx.db, name, day).await.unwrap());
    assert_eq!(tasks::first_every_wait(day, true), tasks::CATCH_UP_DELAY);

    let ran = tasks::attempt(&ctx.db, name, Slot::Every(day), async {})
        .await
        .unwrap();
    assert_eq!(ran, Outcome::Ran);
    assert!(!tasks::overdue(&ctx.db, name, day).await.unwrap());

    // The process restarts every few hours, and the last run is now more
    // than a day old: overdue again, rather than waiting out another day.
    sqlx::query(
        "UPDATE eunha.scheduled_tasks SET last_slot = now() - interval '25 hours'
         WHERE name = $1",
    )
    .bind(name)
    .execute(&ctx.db)
    .await
    .unwrap();
    assert!(tasks::overdue(&ctx.db, name, day).await.unwrap());
    let caught_up = tasks::attempt(&ctx.db, name, Slot::Every(day), async {})
        .await
        .unwrap();
    assert_eq!(caught_up, Outcome::Ran);
}

#[tokio::test]
async fn test_a_missed_cron_slot_is_run_after_a_restart() {
    let ctx = TestContext::new("sched-missed").await;
    let vacuum = tasks::Cron {
        minute: 17,
        hour: Some(4),
    };
    let runs = AtomicUsize::new(0);
    // Yesterday's run finished.
    let yesterday = Utc.with_ymd_and_hms(2026, 10, 9, 4, 17, 0).unwrap();
    tasks::attempt(&ctx.db, "vacuum_scheduler", Slot::Cron(yesterday), async {})
        .await
        .unwrap();

    // Down over today's 04:17, the process starts again at noon. It looks
    // two minutes in, and today's slot has not been run.
    let started = Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, 0).unwrap();
    let wake = tasks::first_cron_wake(vacuum, started);
    assert_eq!(wake, started + chrono::Duration::minutes(2));
    let slot = vacuum.previous(wake);
    assert_eq!(slot, Utc.with_ymd_and_hms(2026, 10, 10, 4, 17, 0).unwrap());
    let outcome = tasks::attempt(
        &ctx.db,
        "vacuum_scheduler",
        Slot::Cron(slot),
        counted(&runs),
    )
    .await
    .unwrap();
    assert_eq!(outcome, Outcome::Ran);

    // Another restart the same afternoon finds it done.
    let outcome = tasks::attempt(
        &ctx.state.db,
        "vacuum_scheduler",
        Slot::Cron(slot),
        counted(&runs),
    )
    .await
    .unwrap();
    assert_eq!(outcome, Outcome::Done);
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

/// Every connection eunha opens to PostgreSQL works in UTC. Mastodon's
/// `timestamp without time zone` columns hold UTC, and some of what eunha
/// writes into them is `now()` cast by the server, which casts in the
/// session's `TimeZone`. sqlx sends `TimeZone=UTC` in every connection's
/// startup packet, which outranks the server's own default — production's
/// *postgresql.conf* says Asia/Seoul — and a database's. Every pool is
/// opened by `eunha::tenants::connect` (the server's, the pooler's, the
/// CLI's), and every other connection through sqlx's `PgConnectOptions`
/// too; there is no other path to the database.
#[tokio::test]
async fn test_every_pooled_connection_works_in_utc() {
    let ctx = TestContext::new("sched-tz").await;
    let database: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    // As production's server is set, here for this database alone.
    sqlx::query(&format!(
        "ALTER DATABASE \"{database}\" SET TimeZone TO 'Asia/Seoul'"
    ))
    .execute(&ctx.db)
    .await
    .unwrap();

    let fresh = eunha::tenants::connect(
        &ctx.state.config.database_url,
        &eunha::config::DatabasePoolConfig {
            max_connections: 4,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    for pool in [&ctx.state.db, &fresh] {
        // Hold every connection the pool may open at once, so each is asked.
        let size = pool.options().get_max_connections();
        let mut held = Vec::new();
        for _ in 0..size {
            held.push(pool.acquire().await.unwrap());
        }
        for connection in &mut held {
            let zone: String = sqlx::query_scalar("SHOW TimeZone")
                .fetch_one(&mut **connection)
                .await
                .unwrap();
            assert_eq!(zone, "UTC");
            let skew: f64 = sqlx::query_scalar(
                "SELECT abs(EXTRACT(EPOCH FROM (now()::timestamp - (now() AT TIME ZONE 'UTC'))))::float8",
            )
            .fetch_one(&mut **connection)
            .await
            .unwrap();
            assert_eq!(skew, 0.0, "now() written to a timestamp column is UTC");
        }
    }
    // A connection outside any pool, as `eunha` opens to size its pools.
    use sqlx::Connection;
    let mut single = sqlx::PgConnection::connect(&ctx.state.config.database_url)
        .await
        .unwrap();
    let zone: String = sqlx::query_scalar("SHOW TimeZone")
        .fetch_one(&mut single)
        .await
        .unwrap();
    assert_eq!(zone, "UTC");
    fresh.close().await;
}
