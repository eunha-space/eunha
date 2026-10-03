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

/// `DeliveryFailureTracker.reset!(signed_request_actor.inbox_url)`: by the
/// host of the signer's inbox, which need not be its own.
#[tokio::test]
async fn a_server_that_delivers_to_us_is_available_again() {
    let ctx = TestContext::new("delivery-failures-inbound").await;
    let remote = host("back");
    let inbox_host = host("inbox");
    let (private_key, public_key) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    let carol = format!("https://{remote}/users/carol");
    sqlx::query(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'carol', $2, 'carol', '', $3, $3, $4,
                   $5, $3 || '/outbox', now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(&remote)
    .bind(&carol)
    .bind(&public_key)
    .bind(format!("https://{inbox_host}/inbox"))
    .execute(&ctx.db)
    .await
    .unwrap();
    failed_before(&ctx, &remote, 3).await;
    failed_before(&ctx, &inbox_host, 7).await;
    ctx.state
        .delivery_failures
        .track_failure(&inbox_host)
        .await
        .unwrap();
    assert!(is_marked(&ctx, &inbox_host).await);

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

    assert_eq!(failure_days(&ctx, &inbox_host).await, 0);
    assert!(!is_marked(&ctx, &inbox_host).await);
    assert_eq!(
        failure_days(&ctx, &remote).await,
        3,
        "the actor's own host is not the inbox's"
    );
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

#[test]
fn a_delivery_is_retried_on_mastodons_schedule() {
    use eunha::federation::delivery::{retry_in, RETRY};

    // After the n-th failure Sidekiq's count is n - 1, and the wait is
    // count**4 + 15, up to half count**4 more, and up to 10 * (count + 1)
    // more, in whole seconds.
    for failed in 1..=16u32 {
        let count = u64::from(failed - 1);
        let least = count.pow(4) + 15;
        let most = least + (count.pow(4) / 2).max(1) + 10 * (count + 1);
        for _ in 0..50 {
            let wait = retry_in(failed).as_secs();
            assert!((least..most).contains(&wait), "{failed}: {wait}");
        }
    }
    assert!(RETRY.delay(16).is_some(), "retried sixteen times");
    assert!(
        RETRY.delay(17).is_none(),
        "and given up on at the seventeenth"
    );
    let floor: u64 = (1..=16u32)
        .map(|failed| u64::from(failed - 1).pow(4) + 15)
        .sum();
    assert_eq!(floor, 178_552, "two days and a bit before jitter");
}

/// The breaker on deliveries is kept in Redis, as Mastodon's Stoplights are:
/// failures counted by one process hold back another's deliveries, and an
/// instance with another key prefix keeps breakers of its own.
#[tokio::test]
async fn test_the_delivery_breaker_is_shared_through_redis() {
    use eunha::federation::delivery::{RedisBreakers, BREAKER};
    use ojak::deliverer::BreakerStore as _;

    let ctx = TestContext::new("breaker-shared").await;
    let inbox = format!("https://{}.invalid/inbox", ctx.domain);
    let one = RedisBreakers::new(
        ctx.state.redis_coordination.clone(),
        ctx.state.redis_keys.clone(),
    );
    let another = RedisBreakers::new(
        ctx.state.redis_coordination.clone(),
        ctx.state.redis_keys.clone(),
    );
    let elsewhere = RedisBreakers::new(
        ctx.state.redis_coordination.clone(),
        eunha::redis_keys::RedisKeyspace::new(&format!(
            "{}-other",
            ctx.state.config.redis_key_prefix
        ))
        .unwrap(),
    );

    for _ in 0..BREAKER.threshold - 1 {
        one.record(&BREAKER, &inbox, true).await;
    }
    assert_eq!(another.held(&BREAKER, &inbox).await, None);
    another.record(&BREAKER, &inbox, true).await;
    let held = one.held(&BREAKER, &inbox).await.expect("open after ten");
    assert!(held <= BREAKER.cool_off && held > Duration::from_secs(55));
    assert_eq!(elsewhere.held(&BREAKER, &inbox).await, None);

    // One success anywhere closes it.
    another.record(&BREAKER, &inbox, false).await;
    assert_eq!(one.held(&BREAKER, &inbox).await, None);
}

/// `unsalvageable_authorization_failure?`: a 401 is final for a delivery
/// whose sender is deleted or suspended with nothing left to undo, and only
/// for one.
#[tokio::test]
async fn a_401_is_final_only_for_a_sender_that_is_gone() {
    use eunha::federation::delivery::sender_gone;

    let ctx = TestContext::new("delivery-failures-gone").await;
    let key_id = format!("https://{}/users/alice#main-key", ctx.domain);
    let alice: i64 = ctx.alice_id.parse().unwrap();
    assert!(!sender_gone(&ctx.db, &key_id).await, "alice is here");

    // Suspended, with the deletion request that lets it be undone.
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(alice)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO account_deletion_requests (account_id, created_at, updated_at) \
         VALUES ($1, now(), now())",
    )
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();
    assert!(
        !sender_gone(&ctx.db, &key_id).await,
        "a suspension can be undone"
    );

    sqlx::query("DELETE FROM account_deletion_requests WHERE account_id = $1")
        .bind(alice)
        .execute(&ctx.db)
        .await
        .unwrap();
    assert!(sender_gone(&ctx.db, &key_id).await, "nothing left to undo");
    assert!(
        !eunha::federation::delivery::unsalvageable(401),
        "a 401 is final only through `sender_gone`"
    );
}
