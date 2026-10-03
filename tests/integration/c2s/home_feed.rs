//! Home feed regeneration: `HomeFeed#regenerating?`, `User#regenerate_feed!`
//! on a returning user's sign-in, `FollowService#mark_home_feed_as_partial!`
//! and `Vacuum::FeedsVacuum`; and that reading a feed never fills it.

use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::TestContext;

fn home_key(ctx: &TestContext, suffix: &str) -> String {
    ctx.state
        .redis_keys
        .key(format!("feed:home:{}{suffix}", ctx.alice_id))
}

async fn feed_holds(ctx: &TestContext, status_id: &str) -> bool {
    let mut redis = ctx.state.redis.clone();
    let score: Option<f64> = redis::cmd("ZSCORE")
        .arg(home_key(ctx, ""))
        .arg(status_id)
        .query_async(&mut redis)
        .await
        .unwrap();
    score.is_some()
}

async fn feed_exists(ctx: &TestContext) -> bool {
    let mut redis = ctx.state.redis.clone();
    let exists: i64 = redis::cmd("EXISTS")
        .arg(home_key(ctx, ""))
        .query_async(&mut redis)
        .await
        .unwrap();
    exists == 1
}

fn contents(body: &Value) -> Vec<String> {
    body.as_array()
        .unwrap()
        .iter()
        .map(|s| s["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// While the feed regenerates, the home timeline answers `206` with what the
/// feed holds and a `Mastodon-Async-Refresh` header asking for a retry in five
/// seconds, whose id polls as running; once it finishes, `200` and no header.
#[tokio::test]
async fn test_home_timeline_is_partial_while_the_feed_regenerates() {
    let ctx = TestContext::new("home-feed-partial").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    // Build the feed first, as a signed-in user's is.
    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("mastodon-async-refresh").is_none());

    eunha::home_feed::regeneration_in_progress(&ctx.state, alice).await;
    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    let header = resp
        .headers()
        .get("mastodon-async-refresh")
        .expect("a refresh header")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(header.ends_with(", retry=5"), "{header}");
    let id = header
        .strip_prefix("id=\"")
        .and_then(|rest| rest.split_once('"'))
        .map(|(id, _)| id.to_owned())
        .unwrap();
    let body: Value = ctx
        .api
        .get(
            &format!("/api/v1_alpha/async_refreshes/{id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["async_refresh"]["status"], "running");

    eunha::home_feed::regeneration_finished(&ctx.state, alice).await;
    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("mastodon-async-refresh").is_none());
}

/// A regeneration key an older Mastodon left as a string reads as running,
/// replaced by a refresh hash (`HomeFeed#upgrade_redis_key!`).
#[tokio::test]
async fn test_a_string_regeneration_key_is_upgraded() {
    let ctx = TestContext::new("home-feed-upgrade").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let key = ctx
        .state
        .redis_keys
        .key(eunha::home_feed::regeneration_key(alice));
    let mut redis = ctx.state.redis_coordination.clone();
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg("true")
        .query_async(&mut redis)
        .await
        .unwrap();
    assert!(eunha::home_feed::regenerating(&ctx.state, alice).await);
    let status: Option<String> = redis::cmd("HGET")
        .arg(&key)
        .arg("status")
        .query_async(&mut redis)
        .await
        .unwrap();
    assert_eq!(status.as_deref(), Some("running"));
}

/// The feeds vacuum removes the feed of a user who has not signed in for a
/// week, the fan-out skips that user, and the user's next request — the
/// first in a day, so `require_user!` records the sign-in — regenerates it
/// with what was missed.
#[tokio::test]
async fn test_a_returning_users_feed_is_regenerated() {
    let ctx = TestContext::new("home-feed-returning").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let first = ctx
        .api
        .post_status(&ctx.bob_token, "before the break", "public")
        .await;
    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(feed_holds(&ctx, first["id"].as_str().unwrap()).await);

    // Alice goes away for eight days.
    sqlx::query(
        "UPDATE users SET current_sign_in_at = now() - interval '8 days',
                          last_sign_in_at = now() - interval '9 days'
         WHERE account_id = $1",
    )
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();
    let missed = ctx
        .api
        .post_status(&ctx.bob_token, "while you were away", "public")
        .await;
    assert!(
        !feed_holds(&ctx, missed["id"].as_str().unwrap()).await,
        "an inactive user's feed is not fanned out to"
    );
    let mut redis = ctx.state.redis.clone();
    eunha::feed::vacuum_inactive_feeds(&mut redis, &ctx.state.redis_keys, &ctx.db)
        .await
        .unwrap();
    assert!(!feed_exists(&ctx).await, "the vacuum removed the feed");

    // Tests run the regeneration in the request, so it has finished by the
    // time the timeline is read.
    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let texts = contents(&body);
    assert!(texts.iter().any(|t| t.contains("while you were away")));
    assert!(texts.iter().any(|t| t.contains("before the break")));
    assert!(feed_exists(&ctx).await);

    let (recent, previous): (bool, bool) = sqlx::query_as(
        "SELECT current_sign_in_at > now() - interval '1 minute',
                last_sign_in_at < now() - interval '7 days'
         FROM users WHERE account_id = $1",
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(recent && previous, "the sign-in was moved along");
    let refresh = eunha::home_feed::async_refresh(&ctx.state, alice).await;
    assert!(refresh.is_finished(), "the regeneration ran and finished");
}

/// A user who signed in within the day is not signed in again by every
/// request, and so is not regenerated by one.
#[tokio::test]
async fn test_a_recent_sign_in_is_not_moved_by_every_request() {
    let ctx = TestContext::new("home-feed-sign-in-daily").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    sqlx::query(
        "UPDATE users SET current_sign_in_at = now() - interval '2 hours',
                          last_sign_in_at = now() - interval '30 days'
         WHERE account_id = $1",
    )
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();
    ctx.api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    let moved: bool = sqlx::query_scalar(
        "SELECT current_sign_in_at > now() - interval '1 hour' FROM users WHERE account_id = $1",
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(!moved);
}

/// An account that follows no one and asks to follow a locked account has a
/// partial home feed until the request is authorized and the followed
/// account's posts are merged in.
#[tokio::test]
async fn test_a_first_follow_leaves_the_feed_partial_until_merged() {
    let ctx = TestContext::new("home-feed-first-follow").await;
    let bob: i64 = ctx.bob_id.parse().unwrap();
    ctx.api
        .post_status(&ctx.bob_token, "locked away", "private")
        .await;
    sqlx::query("UPDATE accounts SET locked = true WHERE id = $1")
        .bind(bob)
        .execute(&ctx.db)
        .await
        .unwrap();
    let relationship = ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    assert_eq!(relationship["requested"], true);

    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert!(resp.headers().get("mastodon-async-refresh").is_some());

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/follow_requests/{}/authorize", ctx.alice_id),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(contents(&body).iter().any(|t| t.contains("locked away")));
}

/// A new account's home feed is empty, and reading it answers `200` with
/// nothing in it: `HomeController#show` regenerates nothing, and only a
/// running regeneration makes the answer partial.
#[tokio::test]
async fn test_an_empty_feed_is_read_as_it_is() {
    let ctx = TestContext::new("home-feed-empty").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("mastodon-async-refresh").is_none());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, serde_json::json!([]));
    assert!(!feed_exists(&ctx).await, "reading built nothing");
    let refresh = eunha::home_feed::async_refresh(&ctx.state, alice).await;
    assert!(!refresh.is_running() && !refresh.is_finished());
}

/// A feed Redis lost is not rebuilt by reading it: what was in it is gone
/// until a regeneration, and what is posted from then on goes in.
#[tokio::test]
async fn test_a_lost_feed_is_not_rebuilt_by_reading_it() {
    let ctx = TestContext::new("home-feed-lost").await;
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let lost = ctx
        .api
        .post_status(&ctx.bob_token, "lost with the feed", "public")
        .await;
    assert!(feed_holds(&ctx, lost["id"].as_str().unwrap()).await);
    let mut redis = ctx.state.redis.clone();
    let _: () = redis::cmd("DEL")
        .arg(home_key(&ctx, ""))
        .query_async(&mut redis)
        .await
        .unwrap();

    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, serde_json::json!([]));

    let after = ctx
        .api
        .post_status(&ctx.bob_token, "after the loss", "public")
        .await;
    assert!(feed_holds(&ctx, after["id"].as_str().unwrap()).await);
    let body: Value = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(contents(&body).len(), 1);
    assert!(contents(&body)[0].contains("after the loss"));
}

/// A feed another process built under Mastodon's keys alone is read and fed
/// as eunha's own: there is nothing else to mark it.
#[tokio::test]
async fn test_a_feed_built_elsewhere_is_fed_and_read() {
    let ctx = TestContext::new("home-feed-shared").await;
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let older = ctx
        .api
        .post_status(&ctx.bob_token, "built elsewhere", "public")
        .await;
    let older_id: i64 = older["id"].as_str().unwrap().parse().unwrap();
    // Rebuild the feed as a Mastodon sharing the Redis would leave it.
    let mut redis = ctx.state.redis.clone();
    let _: () = redis::cmd("DEL")
        .arg(home_key(&ctx, ""))
        .query_async(&mut redis)
        .await
        .unwrap();
    let _: () = redis::cmd("ZADD")
        .arg(home_key(&ctx, ""))
        .arg(older_id)
        .arg(older_id)
        .query_async(&mut redis)
        .await
        .unwrap();

    let newer = ctx
        .api
        .post_status(&ctx.bob_token, "fanned out here", "public")
        .await;
    assert!(feed_holds(&ctx, newer["id"].as_str().unwrap()).await);
    let body: Value = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let texts = contents(&body);
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(texts[0].contains("fanned out here"));
    assert!(texts[1].contains("built elsewhere"));
}
