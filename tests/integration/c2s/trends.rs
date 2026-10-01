use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::TestContext;

/// Trending statuses excludes statuses from accounts blocked by the viewer.
#[tokio::test]
async fn test_trending_statuses_excludes_blocked_accounts() {
    let ctx = TestContext::new("trends-block").await;
    crate::helpers::open_trends(&ctx.db).await;

    // Bob posts a public status that trends.
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "trending block post #trendsblock", "public")
        .await;
    let status_id = status["id"].as_str().unwrap();
    crate::helpers::favourited_by_crowd(&ctx, status_id).await;
    crate::helpers::refresh_trends(&ctx).await;

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
    crate::helpers::favourited_by_crowd(&ctx, status_id).await;
    crate::helpers::refresh_trends(&ctx).await;

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

/// A hashtag five people used today trends, with its history counted.
#[tokio::test]
async fn test_trending_tags_includes_recent_public_tag() {
    let ctx = TestContext::new("trends-tags-pub").await;
    crate::helpers::open_trends(&ctx.db).await;

    crate::helpers::posted_by_crowd(&ctx, "Hello #trendingtagtest").await;
    crate::helpers::refresh_trends(&ctx).await;

    let tags: Vec<Value> = ctx
        .api
        .get("/api/v1/trends/tags", None)
        .await
        .json()
        .await
        .unwrap();
    let tag = tags
        .iter()
        .find(|t| t["name"] == "trendingtagtest")
        .unwrap_or_else(|| panic!("a tag five people used should trend: {tags:?}"));
    assert!(tag["url"].is_string(), "tag.url missing");
    let history = tag["history"].as_array().expect("tag.history missing");
    assert_eq!(history.len(), 7);
    assert_eq!(history[0]["accounts"], "5");
    assert_eq!(history[0]["uses"], "5");

    // `score = (observed - expected)**2 / expected`: five people today
    // against yesterday's none, counted as one, scores 16.
    let (score, rank): (f64, i32) = sqlx::query_as(
        "SELECT tt.score, tt.rank FROM tag_trends tt JOIN tags t ON t.id = tt.tag_id
         WHERE t.name = 'trendingtagtest'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!((score - 16.0).abs() < 0.01, "score {score}");
    assert_eq!(rank, 1);
}

/// One person using a hashtag, however often, is under the threshold.
#[tokio::test]
async fn test_trending_tags_need_five_people() {
    let ctx = TestContext::new("trends-tags-few").await;
    crate::helpers::open_trends(&ctx.db).await;

    for _ in 0..6 {
        ctx.api
            .post_status(&ctx.alice_token, "Again #lonelytrend", "public")
            .await;
    }
    crate::helpers::refresh_trends(&ctx).await;

    let tags: Vec<Value> = ctx
        .api
        .get("/api/v1/trends/tags", None)
        .await
        .json()
        .await
        .unwrap();
    assert!(
        tags.is_empty(),
        "one person does not make a trend: {tags:?}"
    );
}

/// Private statuses do not contribute to trending tags.
#[tokio::test]
async fn test_trending_tags_excludes_private_posts() {
    let ctx = TestContext::new("trends-tags-priv").await;
    crate::helpers::open_trends(&ctx.db).await;

    for (_, token) in crate::helpers::crowd(&ctx, 5).await {
        ctx.api
            .post_status(&token, "Hello #privatetrend", "private")
            .await;
    }
    crate::helpers::refresh_trends(&ctx).await;

    let tags: Vec<Value> = ctx
        .api
        .get("/api/v1/trends/tags", None)
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !tags.iter().any(|t| t["name"] == "privatetrend"),
        "tag used only in private posts should not trend"
    );
}

/// A post trends on five favourites, decayed by its age, and only an
/// account's best post is shown.
#[tokio::test]
async fn test_trending_statuses_score_and_one_per_account() {
    let ctx = TestContext::new("trends-statuses").await;
    crate::helpers::open_trends(&ctx.db).await;

    let first = ctx
        .api
        .post_status(&ctx.bob_token, "first trending post", "public")
        .await;
    let second = ctx
        .api
        .post_status(&ctx.bob_token, "second trending post", "public")
        .await;
    let first_id = first["id"].as_str().unwrap();
    let second_id = second["id"].as_str().unwrap();
    crate::helpers::favourited_by_crowd(&ctx, first_id).await;
    crate::helpers::favourited_by_crowd(&ctx, second_id).await;
    // One more for the second, so it scores higher.
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{second_id}/favourite"),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;
    crate::helpers::refresh_trends(&ctx).await;

    let scores: Vec<(i64, f64)> =
        sqlx::query_as("SELECT status_id, score FROM status_trends ORDER BY rank")
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    assert_eq!(scores.len(), 2, "{scores:?}");
    // Freshly posted, barely decayed: (6 - 1)² = 25, then (5 - 1)² = 16.
    assert_eq!(scores[0].0.to_string(), second_id);
    assert!((scores[0].1 - 25.0).abs() < 0.1, "{scores:?}");
    assert!((scores[1].1 - 16.0).abs() < 0.1, "{scores:?}");

    let trending: Vec<Value> = ctx
        .api
        .get("/api/v1/trends/statuses", None)
        .await
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = trending.iter().filter_map(|s| s["id"].as_str()).collect();
    assert_eq!(ids, vec![second_id], "only bob's best post is shown");
}

/// A reply, or a post behind a content warning, never trends.
#[tokio::test]
async fn test_trending_statuses_skip_ineligible_posts() {
    let ctx = TestContext::new("trends-ineligible").await;
    crate::helpers::open_trends(&ctx.db).await;

    let warned: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &serde_json::json!({"status": "spoilers", "spoiler_text": "cw", "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    crate::helpers::favourited_by_crowd(&ctx, warned["id"].as_str().unwrap()).await;
    crate::helpers::refresh_trends(&ctx).await;

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM status_trends")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
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

    crate::helpers::posted_by_crowd(&ctx, "Trending #trendlimita #trendlimitb #trendlimitc").await;
    crate::helpers::refresh_trends(&ctx).await;

    let tags: Vec<Value> = ctx
        .api
        .get("/api/v1/trends/tags?limit=2", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(tags.len(), 2, "limit=2 should cap results at 2: {tags:?}");
}
