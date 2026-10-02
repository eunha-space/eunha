//! `aggregate_reblogs`: `FeedManager#add_to_feed` keeps a second boost of a
//! post out of the home timeline while the first is recent, and
//! `#remove_from_feed` brings it back when the first is undone.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{seed_user, TestContext};

/// What alice's home timeline shows: each entry's id, and what it boosts.
async fn home(ctx: &TestContext) -> Vec<(String, Option<String>)> {
    ctx.api
        .home_timeline(&ctx.alice_token)
        .await
        .into_iter()
        .map(|s| {
            (
                s["id"].as_str().unwrap().to_owned(),
                s["reblog"]["id"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

async fn boost(ctx: &TestContext, token: &str, id: &str) -> String {
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    body["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn test_a_second_boost_waits_for_the_first() {
    let ctx = TestContext::new("aggregate-reblogs").await;
    let (carol_id, carol_token) =
        seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let (_, dave_token) = seed_user(&ctx.db, &ctx.domain, "dave", "dave@test.invalid").await;
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api
        .follow(&ctx.alice_token, &carol_id.to_string())
        .await;
    // The first read fills the feed; what follows arrives by fan-out.
    assert!(home(&ctx).await.is_empty());

    let post = ctx
        .api
        .post_status(&dave_token, "worth boosting twice", "public")
        .await;
    let post_id = post["id"].as_str().unwrap();
    let bobs = boost(&ctx, &ctx.bob_token, post_id).await;
    let carols = boost(&ctx, &carol_token, post_id).await;
    let boosts: Vec<_> = home(&ctx)
        .await
        .into_iter()
        .filter(|(_, of)| of.as_deref() == Some(post_id))
        .map(|(id, _)| id)
        .collect();
    assert_eq!(boosts, std::slice::from_ref(&bobs));

    // Bob undoes his: carol's, held back, takes its place.
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{post_id}/unreblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let boosts: Vec<_> = home(&ctx).await.into_iter().map(|(id, _)| id).collect();
    assert_eq!(boosts, [carols]);
}

#[tokio::test]
async fn test_a_boost_of_a_post_already_there_stays_out() {
    let ctx = TestContext::new("aggregate-reblogs-original").await;
    let (carol_id, carol_token) =
        seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api
        .follow(&ctx.alice_token, &carol_id.to_string())
        .await;
    assert!(home(&ctx).await.is_empty());

    let post = ctx
        .api
        .post_status(&ctx.bob_token, "my own post", "public")
        .await;
    let post_id = post["id"].as_str().unwrap();
    boost(&ctx, &carol_token, post_id).await;
    assert_eq!(home(&ctx).await, [(post_id.to_owned(), None)]);
}

#[tokio::test]
async fn test_every_boost_when_not_aggregating() {
    let ctx = TestContext::new("aggregate-reblogs-off").await;
    let resp = ctx
        .api
        .patch_json(
            "/api/eunha/v1/preferences",
            Some(&ctx.alice_token),
            &json!({ "aggregate_reblogs": false }),
        )
        .await;
    let prefs: Value = resp.json().await.unwrap();
    assert_eq!(prefs["aggregate_reblogs"], false);

    let (carol_id, carol_token) =
        seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let (_, dave_token) = seed_user(&ctx.db, &ctx.domain, "dave", "dave@test.invalid").await;
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api
        .follow(&ctx.alice_token, &carol_id.to_string())
        .await;
    assert!(home(&ctx).await.is_empty());

    let post = ctx.api.post_status(&dave_token, "boost me", "public").await;
    let post_id = post["id"].as_str().unwrap();
    boost(&ctx, &ctx.bob_token, post_id).await;
    boost(&ctx, &carol_token, post_id).await;
    assert_eq!(home(&ctx).await.len(), 2);
}

#[tokio::test]
async fn test_a_rebuilt_feed_aggregates_too() {
    let ctx = TestContext::new("aggregate-reblogs-rebuilt").await;
    let (carol_id, carol_token) =
        seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let (_, dave_token) = seed_user(&ctx.db, &ctx.domain, "dave", "dave@test.invalid").await;
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api
        .follow(&ctx.alice_token, &carol_id.to_string())
        .await;
    let post = ctx.api.post_status(&dave_token, "boost me", "public").await;
    let post_id = post["id"].as_str().unwrap();
    let bobs = boost(&ctx, &ctx.bob_token, post_id).await;
    boost(&ctx, &carol_token, post_id).await;

    // Read from the database while the feed is filled, then from the feed.
    let first: Vec<_> = home(&ctx).await.into_iter().map(|(id, _)| id).collect();
    assert_eq!(first, std::slice::from_ref(&bobs));
    let second: Vec<_> = home(&ctx).await.into_iter().map(|(id, _)| id).collect();
    assert_eq!(second, [bobs]);
}
