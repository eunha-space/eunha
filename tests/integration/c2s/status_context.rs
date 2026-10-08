//! `GET /api/v1/statuses/:id/context`, as `ContextsController` and
//! `Status::ThreadingConcern` answer it.

use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn reply(ctx: &TestContext, token: &str, text: &str, parent: Option<&str>) -> String {
    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(token),
            &json!({"status": text, "in_reply_to_id": parent, "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    status["id"].as_str().unwrap().to_owned()
}

async fn context(ctx: &TestContext, id: &str, token: Option<&str>) -> (Vec<String>, Vec<String>) {
    let body: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/context"), token)
        .await
        .json()
        .await
        .unwrap();
    let ids = |key: &str| {
        body[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["content"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    (ids("ancestors"), ids("descendants"))
}

/// `permitted_statuses_from_ids` runs each post through `StatusFilter`: the
/// viewer's mutes take a post out of the context, and so does a silenced
/// author the viewer does not follow, for anyone signed out too.
#[tokio::test]
async fn test_the_context_is_filtered_as_status_filter_filters() {
    let ctx = TestContext::new("context-filter").await;
    let (carol_id, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let root = reply(&ctx, &ctx.alice_token, "root", None).await;
    reply(&ctx, &ctx.bob_token, "from bob", Some(&root)).await;
    reply(&ctx, &carol_token, "from carol", Some(&root)).await;

    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{carol_id}/mute"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    let (_, descendants) = context(&ctx, &root, Some(&ctx.alice_token)).await;
    assert_eq!(descendants, ["<p>from bob</p>"]);
    let (_, descendants) = context(&ctx, &root, Some(&ctx.bob_token)).await;
    assert_eq!(descendants, ["<p>from bob</p>", "<p>from carol</p>"]);

    sqlx::query("UPDATE accounts SET silenced_at = now() WHERE username = 'bob'")
        .execute(&ctx.db)
        .await
        .unwrap();
    let (_, descendants) = context(&ctx, &root, None).await;
    assert_eq!(descendants, ["<p>from carol</p>"]);
    let (_, descendants) = context(&ctx, &root, Some(&carol_token)).await;
    assert_eq!(descendants, ["<p>from carol</p>"]);
    // Bob sees his own post, and whoever follows him sees it too.
    let (_, descendants) = context(&ctx, &root, Some(&ctx.bob_token)).await;
    assert_eq!(descendants, ["<p>from bob</p>", "<p>from carol</p>"]);
    ctx.api.follow(&carol_token, &ctx.bob_id).await;
    let (_, descendants) = context(&ctx, &root, Some(&carol_token)).await;
    assert_eq!(descendants, ["<p>from bob</p>", "<p>from carol</p>"]);
}

/// `ancestor_ids` walks up from the post replied to, keeping the nearest
/// forty for someone signed out, root first; the walk goes through a post
/// since deleted, which is not shown.
#[tokio::test]
async fn test_ancestors_keep_the_nearest_and_walk_past_deleted_posts() {
    let ctx = TestContext::new("context-ancestors").await;
    let mut parent: Option<String> = None;
    let mut ids = vec![];
    for n in 0..43 {
        let id = reply(
            &ctx,
            &ctx.alice_token,
            &format!("post {n}"),
            parent.as_deref(),
        )
        .await;
        ids.push(id.clone());
        parent = Some(id);
    }
    let last = ids.last().unwrap();
    let (ancestors, _) = context(&ctx, last, None).await;
    let expected: Vec<String> = (2..42).map(|n| format!("<p>post {n}</p>")).collect();
    assert_eq!(ancestors, expected);

    // Discarded, as a post a moderator removed is kept for a while.
    sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1")
        .bind(ids[1].parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let (ancestors, _) = context(&ctx, &ids[3], Some(&ctx.alice_token)).await;
    assert_eq!(ancestors, ["<p>post 0</p>", "<p>post 2</p>"]);
    let (_, descendants) = context(&ctx, &ids[0], Some(&ctx.alice_token)).await;
    assert_eq!(descendants.len(), 41, "the replies under the deleted post");
}
