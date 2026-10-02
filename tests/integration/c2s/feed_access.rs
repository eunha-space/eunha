//! The `*_feed_access` settings: who may read the live, hashtag and link
//! feeds, as `PublicFeed` and the timeline controllers decide.

use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::{set_setting, TestContext};

async fn ids(response: reqwest::Response) -> Vec<String> {
    assert_eq!(response.status(), StatusCode::OK);
    let body: Vec<Value> = response.json().await.unwrap();
    body.iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn test_live_feeds_follow_their_access_settings() {
    let ctx = TestContext::new("feed-access-live").await;
    let post = ctx
        .api
        .post_status(&ctx.alice_token, "local post", "public")
        .await;
    let id = post["id"].as_str().unwrap().to_owned();

    set_setting(&ctx.db, "local_live_feed_access", "authenticated").await;
    // `require_auth?`: the local feed, or both, now need a user.
    for path in [
        "/api/v1/timelines/public?local=true",
        "/api/v1/timelines/public",
    ] {
        let response = ctx.api.get(path, None).await;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{path}"
        );
    }
    // The remote feed alone is still public, and has no local posts.
    let remote = ids(ctx
        .api
        .get("/api/v1/timelines/public?remote=true", None)
        .await)
    .await;
    assert!(!remote.contains(&id));
    // A functional user sees everything.
    let signed_in = ids(ctx
        .api
        .get("/api/v1/timelines/public", Some(&ctx.bob_token))
        .await)
    .await;
    assert!(signed_in.contains(&id));

    // `disabled`: only those who may `view_feeds`.
    set_setting(&ctx.db, "local_live_feed_access", "disabled").await;
    let local = ids(ctx
        .api
        .get("/api/v1/timelines/public?local=true", Some(&ctx.bob_token))
        .await)
    .await;
    assert!(local.is_empty(), "{local:?}");
    // Without `local`, a user without access gets the remote posts only.
    let mixed = ids(ctx
        .api
        .get("/api/v1/timelines/public", Some(&ctx.bob_token))
        .await)
    .await;
    assert!(!mixed.contains(&id));

    let instance: Value = ctx
        .api
        .get("/api/v2/instance", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        instance["configuration"]["timelines_access"]["live_feeds"]["local"],
        "disabled"
    );
}

#[tokio::test]
async fn test_hashtag_feeds_follow_their_access_settings() {
    let ctx = TestContext::new("feed-access-tag").await;
    ctx.api
        .post_status(&ctx.alice_token, "about #accesstag", "public")
        .await;
    set_setting(&ctx.db, "local_topic_feed_access", "authenticated").await;
    let anonymous = ctx.api.get("/api/v1/timelines/tag/accesstag", None).await;
    assert_eq!(anonymous.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let remote = ids(ctx
        .api
        .get("/api/v1/timelines/tag/accesstag?remote=true", None)
        .await)
    .await;
    assert!(remote.is_empty());
    let signed_in = ids(ctx
        .api
        .get("/api/v1/timelines/tag/accesstag", Some(&ctx.bob_token))
        .await)
    .await;
    assert_eq!(signed_in.len(), 1);
}

/// The link feed only answers for a link that is trending and allowed.
#[tokio::test]
async fn test_link_feed_needs_an_allowed_trending_link() {
    let ctx = TestContext::new("feed-access-link").await;
    let card: i64 = sqlx::query_scalar(
        "INSERT INTO preview_cards (url, title, created_at, updated_at)
         VALUES ('https://news.test/a', 'A', now(), now()) RETURNING id",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let path = "/api/v1/timelines/link?url=https%3A%2F%2Fnews.test%2Fa";
    assert_eq!(
        ctx.api.get(path, None).await.status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query(
        "INSERT INTO preview_card_trends (preview_card_id, score, language, allowed)
         VALUES ($1, 2, 'en', true)",
    )
    .bind(card)
    .execute(&ctx.db)
    .await
    .unwrap();
    assert_eq!(ctx.api.get(path, None).await.status(), StatusCode::OK);
    assert_eq!(
        ctx.api.get("/api/v1/timelines/link", None).await.status(),
        StatusCode::NOT_FOUND
    );
}
