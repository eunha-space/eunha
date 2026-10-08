//! The timed clean-ups Mastodon's schedulers run: automated post deletion,
//! IP retention, expired tokens, and spent collection items.

use reqwest::StatusCode;
use serde_json::json;

use crate::helpers::{user_id_for, TestContext};

/// A snowflake id for a post made `days` ago.
fn id_days_ago(days: i64, n: i64) -> i64 {
    let ms = (chrono::Utc::now() - chrono::Duration::days(days)).timestamp_millis();
    (ms << 16) + n
}

async fn insert_status(ctx: &TestContext, account_id: i64, id: i64, visibility: i32) {
    sqlx::query(
        r#"INSERT INTO statuses (id, account_id, text, visibility, local, uri, created_at, updated_at)
           VALUES ($1, $2, 'old', $3, true, $4, now(), now())"#,
    )
    .bind(id)
    .bind(account_id)
    .bind(visibility)
    .bind(format!("https://{}/statuses/{id}", ctx.domain))
    .execute(&ctx.db)
    .await
    .unwrap();
}

async fn deleted(ctx: &TestContext, id: i64) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT deleted_at IS NOT NULL FROM statuses WHERE id = $1")
        .bind(id)
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

/// A policy deletes the old posts it does not keep, and a post it kept for
/// its author's favourite goes once the favourite is taken back.
#[tokio::test]
async fn statuses_cleanup_deletes_what_the_policy_does_not_keep() {
    let ctx = TestContext::new("statuses-cleanup").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let plain = id_days_ago(30, 1);
    let direct = id_days_ago(30, 2);
    let pinned = id_days_ago(30, 3);
    let favourited = id_days_ago(30, 4);
    let recent = id_days_ago(1, 5);
    for (id, visibility) in [
        (plain, 0),
        (direct, 3),
        (pinned, 0),
        (favourited, 0),
        (recent, 0),
    ] {
        insert_status(&ctx, alice, id, visibility).await;
    }
    sqlx::query(
        "INSERT INTO status_pins (account_id, status_id, created_at, updated_at) VALUES ($1, $2, now(), now())",
    )
    .bind(alice)
    .bind(pinned)
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO favourites (account_id, status_id, created_at, updated_at) VALUES ($1, $2, now(), now())",
    )
    .bind(alice)
    .bind(favourited)
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO account_statuses_cleanup_policies (account_id, created_at, updated_at) VALUES ($1, now(), now())",
    )
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();

    eunha::statuses_cleanup::perform(&ctx.state).await.unwrap();
    assert!(deleted(&ctx, plain).await);
    for kept in [direct, pinned, favourited, recent] {
        assert!(!deleted(&ctx, kept).await, "{kept} should be kept");
    }

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{favourited}/unfavourite"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    eunha::statuses_cleanup::perform(&ctx.state).await.unwrap();
    assert!(deleted(&ctx, favourited).await);
    assert!(!deleted(&ctx, pinned).await);
}

/// A disabled policy deletes nothing.
#[tokio::test]
async fn a_disabled_policy_deletes_nothing() {
    let ctx = TestContext::new("statuses-cleanup-off").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let old = id_days_ago(30, 1);
    insert_status(&ctx, alice, old, 0).await;
    sqlx::query(
        "INSERT INTO account_statuses_cleanup_policies (account_id, enabled, created_at, updated_at) VALUES ($1, false, now(), now())",
    )
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();
    eunha::statuses_cleanup::perform(&ctx.state).await.unwrap();
    assert!(!deleted(&ctx, old).await);
}

/// What is kept of people's addresses is forgotten after a year.
#[tokio::test]
async fn ip_cleanup_forgets_addresses_after_a_year() {
    let ctx = TestContext::new("ip-cleanup").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let user = user_id_for(&ctx.db, alice).await;
    sqlx::query(
        "UPDATE users SET sign_up_ip = '192.0.2.1', current_sign_in_at = now() - interval '2 years' WHERE id = $1",
    )
    .bind(user)
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO login_activities (user_id, success, ip, created_at)
           VALUES ($1, true, '192.0.2.1', now() - interval '2 years'),
                  ($1, true, '192.0.2.2', now())"#,
    )
    .bind(user)
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO session_activations (session_id, user_id, ip, created_at, updated_at)
           VALUES ('stale', $1, '192.0.2.1', now() - interval '2 years', now() - interval '2 years')"#,
    )
    .bind(user)
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO ip_blocks (ip, severity, expires_at, created_at, updated_at)
           VALUES ('198.51.100.0/24', 9999, now() - interval '1 day', now(), now()),
                  ('203.0.113.0/24', 9999, NULL, now(), now())"#,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    eunha::ip_cleanup::perform(&ctx.state).await.unwrap();

    let sign_up_ip: Option<String> =
        sqlx::query_scalar("SELECT host(sign_up_ip) FROM users WHERE id = $1")
            .bind(user)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(sign_up_ip, None);
    let logins: i64 =
        sqlx::query_scalar("SELECT count(*) FROM login_activities WHERE user_id = $1")
            .bind(user)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(logins, 1);
    let stale: i64 =
        sqlx::query_scalar("SELECT count(*) FROM session_activations WHERE session_id = 'stale'")
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(stale, 0);
    let blocks: Vec<String> = sqlx::query_scalar("SELECT host(ip) FROM ip_blocks")
        .fetch_all(&ctx.db)
        .await
        .unwrap();
    assert_eq!(blocks, vec!["203.0.113.0".to_owned()]);
}

/// Expired and revoked access tokens go; live ones stay.
#[tokio::test]
async fn access_tokens_vacuum_deletes_spent_tokens() {
    let ctx = TestContext::new("tokens-vacuum").await;
    sqlx::query(
        r#"INSERT INTO oauth_access_tokens (token, created_at, expires_in, revoked_at)
           VALUES ('expired-token', now() - interval '2 hours', 60, NULL),
                  ('revoked-token', now(), NULL, now() - interval '1 minute')"#,
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_access_tokens")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let deleted = eunha::vacuum::vacuum_access_tokens(&ctx.db).await.unwrap();
    assert_eq!(deleted, 2);
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_access_tokens")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(after, before - 2);
    // The test accounts' own tokens still work.
    let resp = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Rejected and revoked collection items go a day after they were, and each
/// one destroyed comes off its collection's `item_count` counter cache.
#[tokio::test]
async fn collection_item_cleanup_deletes_spent_items() {
    let ctx = TestContext::new("collection-items").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    let collection: i64 = sqlx::query_scalar(
        r#"INSERT INTO collections (account_id, name, local, sensitive, discoverable, item_count, created_at, updated_at)
           VALUES ($1, 'c', true, false, true, 3, now(), now()) RETURNING id"#,
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO collection_items (collection_id, account_id, state, position, created_at, updated_at)
           VALUES ($1, $2, 1, 1, now(), now()),
                  ($1, NULL, 3, 2, now(), now() - interval '2 days'),
                  ($1, NULL, 2, 3, now(), now() - interval '1 hour')"#,
    )
    .bind(collection)
    .bind(bob)
    .execute(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        eunha::collection_item_cleanup::perform(&ctx.db)
            .await
            .unwrap(),
        1
    );
    let states: Vec<i32> =
        sqlx::query_scalar("SELECT state FROM collection_items ORDER BY position")
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    assert_eq!(states, vec![1, 2]);
    let count: i32 = sqlx::query_scalar("SELECT item_count FROM collections WHERE id = $1")
        .bind(collection)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

/// Unfollowing a hashtag takes its posts out of the home feed, but not the
/// ones from an account still followed.
#[tokio::test]
async fn unfollowing_a_hashtag_unmerges_it_from_home() {
    let ctx = TestContext::new("tag-unmerge").await;
    let resp = ctx
        .api
        .post_json(
            "/api/v1/tags/eunhatest/follow",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let tagged = ctx
        .api
        .post_status(&ctx.bob_token, "hello #eunhatest", "public")
        .await;
    let tagged_id = tagged["id"].as_str().unwrap().to_owned();
    let mut seen = false;
    for _ in 0..50 {
        let home = ctx.api.home_timeline(&ctx.alice_token).await;
        if home.iter().any(|s| s["id"].as_str() == Some(&tagged_id)) {
            seen = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(seen, "the tagged post should reach the home feed");

    let resp = ctx
        .api
        .post_json(
            "/api/v1/tags/eunhatest/unfollow",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut gone = false;
    for _ in 0..50 {
        let home = ctx.api.home_timeline(&ctx.alice_token).await;
        if !home.iter().any(|s| s["id"].as_str() == Some(&tagged_id)) {
            gone = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(gone, "the tagged post should leave the home feed");
}
