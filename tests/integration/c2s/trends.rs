use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::TestContext;

/// Trending statuses excludes statuses from accounts blocked by the viewer.
#[tokio::test]
async fn test_trending_statuses_excludes_blocked_accounts() {
    let ctx = TestContext::new("trends-block").await;
    crate::helpers::open_trends(&ctx.db).await;

    // Bob posts a public status that would trend.
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "trending block post #trendsblock", "public")
        .await;
    let status_id = status["id"].as_str().unwrap();

    // Verify it appears before the block.
    let before: Vec<Value> = ctx
        .api
        .get("/api/v1/trends/statuses", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        before.iter().any(|s| s["id"].as_str() == Some(status_id)),
        "bob's status should appear in trending before block",
    );

    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;

    let after: Vec<Value> = ctx
        .api
        .get("/api/v1/trends/statuses", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !after.iter().any(|s| s["id"].as_str() == Some(status_id)),
        "blocked account's statuses should be hidden from trending statuses",
    );
}

/// Trending statuses excludes statuses from muted accounts (authenticated viewer).
#[tokio::test]
async fn test_trending_statuses_excludes_muted_accounts() {
    let ctx = TestContext::new("trends-mute").await;
    crate::helpers::open_trends(&ctx.db).await;

    let status = ctx
        .api
        .post_status(&ctx.bob_token, "trending mute post #trendsmute", "public")
        .await;
    let status_id = status["id"].as_str().unwrap();

    // Verify it appears before the mute.
    let before: Vec<Value> = ctx
        .api
        .get("/api/v1/trends/statuses", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        before.iter().any(|s| s["id"].as_str() == Some(status_id)),
        "bob's status should appear in trending before mute",
    );

    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;

    let after: Vec<Value> = ctx
        .api
        .get("/api/v1/trends/statuses", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !after.iter().any(|s| s["id"].as_str() == Some(status_id)),
        "muted account's statuses should be hidden from trending statuses",
    );
}

// ── GET /api/v1/trends/tags ───────────────────────────────────────────────────

/// GET /api/v1/trends/tags returns a JSON array (possibly empty).
#[tokio::test]
async fn test_trending_tags_returns_array() {
    let ctx = TestContext::new("trends-tags-arr").await;
    crate::helpers::open_trends(&ctx.db).await;

    let resp = ctx.api.get("/api/v1/trends/tags", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let tags: Value = resp.json().await.unwrap();
    assert!(tags.is_array(), "should return a JSON array");
}

/// GET /api/v1/trends (alias) returns the same shape as /trends/tags.
#[tokio::test]
async fn test_trending_tags_alias() {
    let ctx = TestContext::new("trends-alias").await;
    crate::helpers::open_trends(&ctx.db).await;

    let resp = ctx.api.get("/api/v1/trends", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let tags: Value = resp.json().await.unwrap();
    assert!(tags.is_array(), "alias should return a JSON array");
}

/// A recently used public hashtag appears in trending tags.
#[tokio::test]
async fn test_trending_tags_includes_recent_public_tag() {
    let ctx = TestContext::new("trends-tags-pub").await;
    crate::helpers::open_trends(&ctx.db).await;

    ctx.api
        .post_status(&ctx.alice_token, "Hello #trendingtagtest", "public")
        .await;

    let tags: Vec<Value> = ctx
        .api
        .get("/api/v1/trends/tags", None)
        .await
        .json()
        .await
        .unwrap();

    let found = tags.iter().any(|t| {
        t["name"]
            .as_str()
            .map(|n| n.eq_ignore_ascii_case("trendingtagtest"))
            .unwrap_or(false)
    });
    assert!(found, "recently used public tag should appear in trending");

    // Each tag entry must have the required fields.
    if let Some(tag) = tags.iter().find(|t| {
        t["name"]
            .as_str()
            .map(|n| n.eq_ignore_ascii_case("trendingtagtest"))
            .unwrap_or(false)
    }) {
        assert!(tag["name"].is_string(), "tag.name missing");
        assert!(tag["url"].is_string(), "tag.url missing");
        assert!(tag["history"].is_array(), "tag.history missing");
    }
}

/// Private statuses do not contribute to trending tags.
#[tokio::test]
async fn test_trending_tags_excludes_private_posts() {
    let ctx = TestContext::new("trends-tags-priv").await;
    crate::helpers::open_trends(&ctx.db).await;

    ctx.api
        .post_status(&ctx.alice_token, "Hello #privatetrend", "private")
        .await;

    let tags: Vec<Value> = ctx
        .api
        .get("/api/v1/trends/tags", None)
        .await
        .json()
        .await
        .unwrap();

    let found = tags.iter().any(|t| {
        t["name"]
            .as_str()
            .map(|n| n.eq_ignore_ascii_case("privatetrend"))
            .unwrap_or(false)
    });
    assert!(!found, "tag used only in private posts should not trend");
}

// ── GET /api/v1/trends/links ──────────────────────────────────────────────────

/// GET /api/v1/trends/links returns a JSON array.
#[tokio::test]
async fn test_trending_links_returns_array() {
    let ctx = TestContext::new("trends-links-arr").await;
    crate::helpers::open_trends(&ctx.db).await;

    let resp = ctx.api.get("/api/v1/trends/links", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let links: Value = resp.json().await.unwrap();
    assert!(links.is_array(), "should return a JSON array");
}

/// limit parameter is respected for trending tags.
#[tokio::test]
async fn test_trending_tags_limit_param() {
    let ctx = TestContext::new("trends-tags-limit").await;
    crate::helpers::open_trends(&ctx.db).await;

    // Post statuses with unique tags to ensure some trending entries exist.
    for i in 0..5 {
        ctx.api
            .post_status(
                &ctx.alice_token,
                &format!("Trending limit #{i}trendlimitx"),
                "public",
            )
            .await;
    }

    let tags: Vec<Value> = ctx
        .api
        .get("/api/v1/trends/tags?limit=2", None)
        .await
        .json()
        .await
        .unwrap();
    assert!(
        tags.len() <= 2,
        "limit=2 should cap results at 2, got {}",
        tags.len()
    );
}
