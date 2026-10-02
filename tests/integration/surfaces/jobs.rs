//! The job queue (src/jobs): scheduling, Sidekiq's retries and dead set,
//! sidekiq-unique-jobs' locks, and jobs outliving the process that queued
//! them.

use std::time::Duration;

use eunha::jobs::{self, Mode, Probe, UniqueProbe};

use crate::helpers::TestContext;

async fn durable(label: &str) -> TestContext {
    let ctx = TestContext::new(label).await;
    ctx.state.jobs.set_mode(Mode::Durable);
    ctx
}

#[tokio::test]
async fn test_a_job_queued_for_later_waits_until_it_is_due() {
    let ctx = durable("jobs-later").await;
    let probe = Probe {
        name: "later".into(),
        fail_times: 0,
    };
    jobs::perform_in(&ctx.state, Duration::from_secs(3600), probe)
        .await
        .unwrap()
        .expect("queued");

    let queued = jobs::queued(&ctx.state, "Eunha::ProbeWorker")
        .await
        .unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].queue, "default");
    assert!(
        (3590.0..=3600.0).contains(&queued[0].seconds_until_due),
        "{}",
        queued[0].seconds_until_due
    );
    assert_eq!(jobs::drain(&ctx.state).await.unwrap(), 0);
    assert_eq!(jobs::probe_runs(&ctx.state, "later").await, (0, 0));

    jobs::make_due(&ctx.state).await.unwrap();
    assert_eq!(jobs::drain(&ctx.state).await.unwrap(), 1);
    assert_eq!(jobs::probe_runs(&ctx.state, "later").await, (1, 0));
    assert!(jobs::queued(&ctx.state, "Eunha::ProbeWorker")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_a_failing_job_is_retried_on_sidekiqs_schedule_then_buried() {
    let ctx = durable("jobs-retry").await;
    // `retry: 2`: three runs in all.
    let probe = Probe {
        name: "retry".into(),
        fail_times: 10,
    };
    jobs::perform_async(&ctx.state, probe).await.unwrap();

    assert_eq!(jobs::drain(&ctx.state).await.unwrap(), 1);
    let job = &jobs::queued(&ctx.state, "Eunha::ProbeWorker")
        .await
        .unwrap()[0];
    assert_eq!(job.attempts, 1);
    assert!(!job.dead);
    // `(0**4) + 15 + rand(10) * 1`.
    assert!(
        (14.0..=24.0).contains(&job.seconds_until_due),
        "{}",
        job.seconds_until_due
    );
    assert!(job
        .last_error
        .as_deref()
        .is_some_and(|e| e.contains("failing as asked")));
    // Not due yet.
    assert_eq!(jobs::drain(&ctx.state).await.unwrap(), 0);

    jobs::make_due(&ctx.state).await.unwrap();
    assert_eq!(jobs::drain(&ctx.state).await.unwrap(), 1);
    let job = &jobs::queued(&ctx.state, "Eunha::ProbeWorker")
        .await
        .unwrap()[0];
    assert_eq!(job.attempts, 2);
    // `(1**4) + 15 + rand(10) * 2`.
    assert!(
        (15.0..=34.0).contains(&job.seconds_until_due),
        "{}",
        job.seconds_until_due
    );

    jobs::make_due(&ctx.state).await.unwrap();
    assert_eq!(jobs::drain(&ctx.state).await.unwrap(), 1);
    let job = &jobs::queued(&ctx.state, "Eunha::ProbeWorker")
        .await
        .unwrap()[0];
    assert!(job.dead, "kept in the dead set");
    assert_eq!(job.attempts, 3);
    assert_eq!(jobs::probe_runs(&ctx.state, "retry").await, (3, 1));

    // The dead set is not run again.
    jobs::make_due(&ctx.state).await.unwrap();
    assert_eq!(jobs::drain(&ctx.state).await.unwrap(), 0);
}

#[tokio::test]
async fn test_a_job_that_succeeds_on_retry_is_gone() {
    let ctx = durable("jobs-recover").await;
    let probe = Probe {
        name: "recover".into(),
        fail_times: 1,
    };
    jobs::perform_async(&ctx.state, probe).await.unwrap();
    jobs::drain(&ctx.state).await.unwrap();
    jobs::make_due(&ctx.state).await.unwrap();
    jobs::drain(&ctx.state).await.unwrap();
    assert_eq!(jobs::probe_runs(&ctx.state, "recover").await, (2, 0));
    assert!(jobs::queued(&ctx.state, "Eunha::ProbeWorker")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_a_unique_job_is_queued_once_until_it_has_run() {
    let ctx = durable("jobs-unique").await;
    let probe = || UniqueProbe {
        name: "unique".into(),
        fail_times: 0,
    };
    let first = jobs::perform_async(&ctx.state, probe()).await.unwrap();
    assert!(first.is_some());
    assert_eq!(
        jobs::perform_async(&ctx.state, probe()).await.unwrap(),
        None
    );
    // Different arguments are a different lock.
    let other = UniqueProbe {
        name: "unique-other".into(),
        fail_times: 0,
    };
    assert!(jobs::perform_async(&ctx.state, other)
        .await
        .unwrap()
        .is_some());

    assert_eq!(jobs::drain(&ctx.state).await.unwrap(), 2);
    assert_eq!(jobs::probe_runs(&ctx.state, "unique").await, (1, 0));
    // Run, so the lock is free again.
    assert!(jobs::perform_async(&ctx.state, probe())
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_a_unique_job_that_fails_for_good_gives_up_its_lock() {
    let ctx = durable("jobs-unique-fail").await;
    let probe = || UniqueProbe {
        name: "unique-fail".into(),
        fail_times: 1,
    };
    jobs::perform_async(&ctx.state, probe()).await.unwrap();
    assert_eq!(jobs::drain(&ctx.state).await.unwrap(), 1);
    // `retry: 0, dead: false`: forgotten, not buried.
    assert!(jobs::queued(&ctx.state, "Eunha::UniqueProbeWorker")
        .await
        .unwrap()
        .is_empty());
    assert!(jobs::perform_async(&ctx.state, probe())
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn test_a_unique_lock_past_its_ttl_is_taken_over() {
    let ctx = durable("jobs-unique-ttl").await;
    let probe = || UniqueProbe {
        name: "ttl".into(),
        fail_times: 0,
    };
    jobs::perform_in(&ctx.state, Duration::from_secs(60), probe())
        .await
        .unwrap();
    sqlx::query("UPDATE eunha.jobs SET unique_until = now() - interval '1 second'")
        .execute(&ctx.db)
        .await
        .unwrap();
    assert!(jobs::perform_async(&ctx.state, probe())
        .await
        .unwrap()
        .is_some());
    assert_eq!(
        jobs::queued(&ctx.state, "Eunha::UniqueProbeWorker")
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn test_a_queued_job_survives_a_restart() {
    let ctx = durable("jobs-restart").await;
    let probe = Probe {
        name: "restart".into(),
        fail_times: 0,
    };
    jobs::perform_async(&ctx.state, probe).await.unwrap();

    // A new process on the same database and Redis, which never saw the job
    // queued.
    let restarted = eunha::state::AppState::new(ctx.state.db.clone(), (*ctx.state.config).clone())
        .await
        .unwrap();
    assert_eq!(restarted.jobs.mode(), Mode::Durable);
    assert_eq!(jobs::drain(&restarted).await.unwrap(), 1);
    assert_eq!(jobs::probe_runs(&ctx.state, "restart").await, (1, 0));
}

#[tokio::test]
async fn test_a_job_claimed_by_a_process_that_died_is_taken_up_again() {
    let ctx = durable("jobs-lease").await;
    let probe = Probe {
        name: "lease".into(),
        fail_times: 0,
    };
    jobs::perform_async(&ctx.state, probe).await.unwrap();
    // Claimed by a worker that went away a while ago.
    sqlx::query(
        "UPDATE eunha.jobs SET locked_by = 'gone', locked_at = now() - interval '1 minute'",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        jobs::drain(&ctx.state).await.unwrap(),
        0,
        "lease still held"
    );
    sqlx::query("UPDATE eunha.jobs SET locked_at = now() - interval '10 minutes'")
        .execute(&ctx.db)
        .await
        .unwrap();
    assert_eq!(jobs::drain(&ctx.state).await.unwrap(), 1);
    assert_eq!(jobs::probe_runs(&ctx.state, "lease").await, (1, 0));
}

#[tokio::test]
async fn test_the_job_loop_runs_what_is_queued() {
    let ctx = durable("jobs-loop").await;
    let worker = eunha::tenants::spawn(jobs::run(ctx.state.clone(), 0));
    for n in 0..3 {
        let probe = Probe {
            name: format!("loop-{n}"),
            fail_times: 0,
        };
        jobs::perform_async(&ctx.state, probe).await.unwrap();
    }
    for _ in 0..100 {
        if jobs::queued(&ctx.state, "Eunha::ProbeWorker")
            .await
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for n in 0..3 {
        assert_eq!(
            jobs::probe_runs(&ctx.state, &format!("loop-{n}")).await,
            (1, 0)
        );
    }
    ctx.state.stop.cancel();
    tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .expect("the loop stops with the instance")
        .unwrap();
}

#[tokio::test]
async fn test_mail_is_delivered_later_through_the_queue() {
    let ctx = durable("jobs-mail").await;
    ctx.state
        .mailer()
        .send_two_factor_disabled("someone@test.invalid", &ctx.domain)
        .await
        .unwrap();
    assert!(ctx.sent_to("someone@test.invalid").is_empty());
    let queued = jobs::queued(&ctx.state, "ActionMailer::MailDeliveryJob")
        .await
        .unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].queue, "mailers");

    assert_eq!(jobs::drain(&ctx.state).await.unwrap(), 1);
    assert_eq!(ctx.sent_to("someone@test.invalid").len(), 1);
}

#[tokio::test]
async fn test_immediate_mode_runs_a_job_as_it_is_queued() {
    let ctx = TestContext::new("jobs-immediate").await;
    assert_eq!(ctx.state.jobs.mode(), Mode::Immediate);
    let probe = Probe {
        name: "immediate".into(),
        fail_times: 0,
    };
    // Even one queued for later.
    jobs::perform_in(&ctx.state, Duration::from_secs(600), probe)
        .await
        .unwrap();
    ctx.state.jobs.settle().await;
    assert_eq!(jobs::probe_runs(&ctx.state, "immediate").await, (1, 0));
}
