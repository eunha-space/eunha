//! Follow suggestions from Mastodon's sources, and
//! `Admin::FollowRecommendationsController`.

use super::*;

const MANAGE_TAXONOMIES: i64 = 1 << 8;

async fn discoverable(ctx: &TestContext, account_id: i64) {
    sqlx::query("UPDATE accounts SET discoverable = true WHERE id = $1")
        .bind(account_id)
        .execute(&ctx.db)
        .await
        .unwrap();
}

async fn suggested(ctx: &TestContext) -> Vec<Value> {
    json_ok(
        ctx.api
            .get("/api/v2/suggestions", Some(&ctx.alice_token))
            .await,
    )
    .await
    .as_array()
    .unwrap()
    .clone()
}

fn find<'a>(suggestions: &'a [Value], id: &str) -> Option<&'a Value> {
    suggestions.iter().find(|s| s["account"]["id"] == id)
}

/// Suggestions come from the featured accounts, friends of friends, and the
/// global recommendations the daily refresh computes, each with its source;
/// a moderator suppresses an account from the recommendations.
#[tokio::test]
async fn test_follow_recommendations() {
    let ctx = TestContext::new("srv-follow-recs").await;
    let (carol, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let (dave, dave_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "dave", "dave@test.invalid").await;
    discoverable(&ctx, carol).await;
    discoverable(&ctx, dave).await;

    // Alice follows bob, who follows carol: carol is a friend of a friend.
    // Bob shows whom he follows; Mastodon's query passes over an account
    // whose `hide_collections` is unset as well as one that hides them.
    sqlx::query("UPDATE accounts SET hide_collections = false WHERE id = $1")
        .bind(id(&ctx.bob_id))
        .execute(&ctx.db)
        .await
        .unwrap();
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.bob_token, &carol.to_string()).await;
    let carol_id = carol.to_string();
    let found = suggested(&ctx).await;
    let carol_suggestion = find(&found, &carol_id).expect("carol is suggested");
    assert_eq!(carol_suggestion["sources"], json!(["friends_of_friends"]));
    assert_eq!(carol_suggestion["source"], "past_interactions");
    // Following someone takes them out of the suggestions.
    assert!(find(&found, &ctx.bob_id).is_none());

    // Five recently active people follow dave, who posts: the refresh
    // recommends him as most followed.
    ctx.api.post_status(&dave_token, "hello", "public").await;
    for (follower, token) in crate::helpers::crowd(&ctx, 5).await {
        ctx.api.follow(&token, &dave.to_string()).await;
        sqlx::query("UPDATE users SET current_sign_in_at = now() WHERE account_id = $1")
            .bind(follower)
            .execute(&ctx.db)
            .await
            .unwrap();
    }
    eunha::suggestions::refresh(&ctx.state).await.unwrap();
    let dave_id = dave.to_string();
    let found = suggested(&ctx).await;
    let dave_suggestion = find(&found, &dave_id).expect("dave is recommended");
    assert_eq!(dave_suggestion["sources"], json!(["most_followed"]));
    assert_eq!(dave_suggestion["source"], "global");

    // The moderator's view, and suppressing him.
    give_role(&ctx, &ctx.bob_id, 10, MANAGE_TAXONOMIES).await;
    let recommended = json_ok(
        ctx.api
            .get("/api/v1/admin/follow_recommendations", Some(&ctx.bob_token))
            .await,
    )
    .await;
    assert_eq!(recommended[0]["account"]["id"], dave_id.as_str());
    assert_eq!(recommended[0]["reason"], json!(["most_followed"]));
    json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/follow_recommendations/suppress",
                Some(&ctx.bob_token),
                &json!({"account_ids": [dave_id]}),
            )
            .await,
    )
    .await;
    let recommended = json_ok(
        ctx.api
            .get("/api/v1/admin/follow_recommendations", Some(&ctx.bob_token))
            .await,
    )
    .await;
    assert_eq!(recommended, json!([]));
    let suppressed = json_ok(
        ctx.api
            .get(
                "/api/v1/admin/follow_recommendations?status=suppressed",
                Some(&ctx.bob_token),
            )
            .await,
    )
    .await;
    assert_eq!(suppressed[0]["account"]["id"], dave_id.as_str());
    assert_eq!(suppressed[0]["suppressed"], true);
    assert!(find(&suggested(&ctx).await, &dave_id).is_none());

    json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/follow_recommendations/unsuppress",
                Some(&ctx.bob_token),
                &json!({"account_ids": [dave_id]}),
            )
            .await,
    )
    .await;
    assert!(find(&suggested(&ctx).await, &dave_id).is_some());
    let _ = carol_token;
    assert!(logs(&ctx).await.is_empty());
}
