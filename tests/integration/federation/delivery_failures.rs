//! Which servers have stopped answering, kept as Mastodon 4.7's
//! `DeliveryFailureTracker` keeps it, and what a delivery that fails does, as
//! `ActivityPub::DeliveryWorker` has it.

use ojak::deliverer::{AttemptOutcome, DeliveryAttempt};
use serde_json::json;
use std::time::Duration;

use crate::helpers::TestContext;

/// A host of its own for each test: the Redis the tests share outlives them.
fn host(label: &str) -> String {
    format!("{label}-{}.invalid", &uuid::Uuid::new_v4().to_string()[..8])
}

fn key(ctx: &TestContext, host: &str) -> String {
    ctx.state
        .redis_keys
        .key(format!("exhausted_deliveries:{host}"))
}

async fn failure_days(ctx: &TestContext, host: &str) -> usize {
    let mut redis = ctx.state.redis_coordination.clone();
    redis::cmd("SCARD")
        .arg(key(ctx, host))
        .query_async(&mut redis)
        .await
        .unwrap()
}

/// Days with failures before today, as earlier deliveries would have left.
async fn failed_before(ctx: &TestContext, host: &str, days: i64) {
    let mut redis = ctx.state.redis_coordination.clone();
    for ago in 1..=days {
        let day = (chrono::Utc::now() - chrono::Duration::days(ago))
            .format("%Y%m%d")
            .to_string();
        let _: i64 = redis::cmd("SADD")
            .arg(key(ctx, host))
            .arg(day)
            .query_async(&mut redis)
            .await
            .unwrap();
    }
}

async fn is_marked(ctx: &TestContext, host: &str) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM unavailable_domains WHERE domain = $1)",
    )
    .bind(host)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

/// Wait for what an attempt's outcome does in the background.
async fn eventually(mut done: impl AsyncFnMut() -> bool) {
    for _ in 0..100 {
        if done().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("it did not happen");
}

#[tokio::test]
async fn failures_on_seven_days_mark_a_server_unavailable_and_a_delivery_clears_it() {
    let ctx = TestContext::new("delivery-failures-days").await;
    let tracker = &ctx.state.delivery_failures;
    let host = host("down");

    // However many fail on one day, it is one day.
    for _ in 0..3 {
        tracker.track_failure(&host).await.unwrap();
    }
    assert_eq!(failure_days(&ctx, &host).await, 1);
    assert!(!is_marked(&ctx, &host).await);

    failed_before(&ctx, &host, 5).await;
    tracker.track_failure(&host).await.unwrap();
    assert!(!is_marked(&ctx, &host).await, "six days are not seven");
    failed_before(&ctx, &host, 6).await;
    tracker.track_failure(&host).await.unwrap();
    assert!(is_marked(&ctx, &host).await, "seven days are");
    eventually(async || tracker.is_unavailable(&host)).await;

    tracker.track_success(&host).await.unwrap();
    assert_eq!(failure_days(&ctx, &host).await, 0);
    assert!(!is_marked(&ctx, &host).await);
    eventually(async || !tracker.is_unavailable(&host)).await;
}

#[tokio::test]
async fn an_attempt_counts_as_mastodon_counts_it() {
    let ctx = TestContext::new("delivery-failures-attempts").await;
    let tracker = &ctx.state.delivery_failures;
    let attempt = |host: &str, outcome| DeliveryAttempt {
        inbox: url::Url::parse(&format!("https://{host}/inbox")).unwrap(),
        sender: "https://example.invalid/users/alice#main-key".into(),
        outcome,
    };

    // A failure that may pass counts, and so does one the breaker held.
    let failing = host("failing");
    tracker.record(&attempt(
        &failing,
        AttemptOutcome::Failed {
            status: Some(503),
            permanent: false,
            error: "HTTP 503".into(),
        },
    ));
    eventually(async || failure_days(&ctx, &failing).await == 1).await;
    let held = host("held");
    tracker.record(&attempt(&held, AttemptOutcome::Held));
    eventually(async || failure_days(&ctx, &held).await == 1).await;

    // An answer that will not change counts for nothing: Mastodon's
    // `@unsalvageable` is neither a success nor a failure.
    let gone = host("gone");
    failed_before(&ctx, &gone, 2).await;
    tracker.record(&attempt(
        &gone,
        AttemptOutcome::Failed {
            status: Some(410),
            permanent: true,
            error: "HTTP 410".into(),
        },
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(failure_days(&ctx, &gone).await, 2);
    assert!(!is_marked(&ctx, &gone).await, "a 410 marks nothing");

    // One that goes through clears the rest.
    tracker.record(&attempt(&failing, AttemptOutcome::Delivered));
    eventually(async || failure_days(&ctx, &failing).await == 0).await;
}

#[tokio::test]
async fn a_server_that_delivers_to_us_is_available_again() {
    let ctx = TestContext::new("delivery-failures-inbound").await;
    let remote = host("back");
    let (private_key, public_key) = eunha::crypto::generate_rsa_keypair().unwrap();
    let carol = format!("https://{remote}/users/carol");
    sqlx::query(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'carol', $2, 'carol', '', $3, $3, $4,
                   $3 || '/inbox', $3 || '/outbox', now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(&remote)
    .bind(&carol)
    .bind(&public_key)
    .execute(&ctx.db)
    .await
    .unwrap();
    failed_before(&ctx, &remote, 7).await;
    ctx.state
        .delivery_failures
        .track_failure(&remote)
        .await
        .unwrap();
    assert!(is_marked(&ctx, &remote).await);

    let follow = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("https://{remote}/activities/1"),
        "type": "Follow",
        "actor": carol,
        "object": format!("https://{}/users/alice", ctx.domain),
    });
    let response = ctx
        .api
        .post_signed(
            "/inbox",
            &follow,
            &format!("{carol}#main-key"),
            &private_key,
        )
        .await;
    assert!(response.status().is_success(), "{}", response.status());

    assert_eq!(failure_days(&ctx, &remote).await, 0);
    assert!(!is_marked(&ctx, &remote).await);
}

#[test]
fn a_delivery_fails_for_good_on_what_mastodon_gives_up_on() {
    use eunha::federation::delivery::unsalvageable;

    for status in [400, 403, 404, 410, 422, 501] {
        assert!(unsalvageable(status), "{status}");
    }
    for status in [401, 408, 429, 500, 502, 503, 504] {
        assert!(!unsalvageable(status), "{status}");
    }
    let breaker = eunha::federation::delivery::BREAKER;
    assert_eq!(breaker.threshold, 10);
    assert_eq!(breaker.cool_off, Duration::from_secs(60));
    assert_eq!(breaker.scope, ojak::deliverer::BreakerScope::Inbox);
}
