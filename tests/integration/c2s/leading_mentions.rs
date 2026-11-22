//! A post that opens with a mention goes where a reply to that account would
//! (`crate::leading_mention`, divergence `leading-mention-is-addressed`).

use serde_json::Value;

use crate::helpers::TestContext;

async fn feed_holds(ctx: &TestContext, status_id: &str) -> bool {
    let mut redis = ctx.state.redis.clone();
    let score: Option<f64> = redis::cmd("ZSCORE")
        .arg(
            ctx.state
                .redis_keys
                .key(format!("feed:home:{}", ctx.alice_id)),
        )
        .arg(status_id)
        .query_async(&mut redis)
        .await
        .unwrap();
    score.is_some()
}

async fn local_timeline(ctx: &TestContext) -> Vec<String> {
    let body: Value = ctx
        .api
        .get(
            "/api/v1/timelines/public?local=true",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    body.as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn test_a_post_opening_with_a_mention_goes_where_a_reply_would() {
    let ctx = TestContext::new("leading-mentions").await;
    let (carol_id, _) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let carol_id = carol_id.to_string();
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    // Build alice's feed, as a signed-in user's is.
    ctx.api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;

    let addressed = ctx
        .api
        .post_status(&ctx.bob_token, "@carol hello", "public")
        .await;
    let dotted = ctx
        .api
        .post_status(&ctx.bob_token, ".@carol hello", "public")
        .await;
    let addressed_id = addressed["id"].as_str().unwrap();
    let dotted_id = dotted["id"].as_str().unwrap();

    // Alice follows bob but not carol: only the post that does not open
    // with the mention reaches her.
    assert!(!feed_holds(&ctx, addressed_id).await);
    assert!(feed_holds(&ctx, dotted_id).await);
    // The public timelines leave it out as they leave out replies.
    let local = local_timeline(&ctx).await;
    assert!(!local.iter().any(|id| id == addressed_id));
    assert!(local.iter().any(|id| id == dotted_id));
    // One that opens by mentioning its own author is an ordinary post.
    let own = ctx
        .api
        .post_status(&ctx.bob_token, "@bob a note to self", "public")
        .await;
    let own_id = own["id"].as_str().unwrap();
    assert!(feed_holds(&ctx, own_id).await);
    assert!(local_timeline(&ctx).await.iter().any(|id| id == own_id));
    // It is still an ordinary post to the API: no reply.
    assert!(addressed["in_reply_to_id"].is_null());

    // Once she follows carol, the next one reaches her.
    ctx.api.follow(&ctx.alice_token, &carol_id).await;
    let again = ctx
        .api
        .post_status(&ctx.bob_token, "@carol again", "public")
        .await;
    assert!(feed_holds(&ctx, again["id"].as_str().unwrap()).await);
}
