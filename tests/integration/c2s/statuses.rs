use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

// ── status access control ────────────────────────────────────────────────────

/// GET a private status as an unauthenticated stranger → 404.
#[tokio::test]
async fn test_get_private_status_unauthenticated() {
    let ctx = TestContext::new("prv-unauth").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice private", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx.api.get(&format!("/api/v1/statuses/{id}"), None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET a private status as a non-follower → 404.
#[tokio::test]
async fn test_get_private_status_non_follower() {
    let ctx = TestContext::new("prv-stranger").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice private", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.bob_token))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET a private status as a mentioned non-follower -> 200, matching Mastodon.
#[tokio::test]
async fn test_get_private_status_mentioned_non_follower() {
    let ctx = TestContext::new("prv-mentioned").await;

    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "@bob alice private mention",
                "visibility": "private",
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.bob_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// GET a private status as an accepted follower → 200.
#[tokio::test]
async fn test_get_private_status_accepted_follower() {
    let ctx = TestContext::new("prv-follower").await;

    // Bob follows Alice.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice private", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.bob_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// GET a private status as the author → 200.
#[tokio::test]
async fn test_get_private_status_author() {
    let ctx = TestContext::new("prv-author").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice private", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// GET a direct status as an accepted follower → 404.
#[tokio::test]
async fn test_get_direct_status_follower() {
    let ctx = TestContext::new("dir-follower").await;

    // Bob follows Alice.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice direct", "direct")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.bob_token))
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "follower should not see direct status"
    );
}

/// GET a direct status as the author → 200.
#[tokio::test]
async fn test_get_direct_status_author() {
    let ctx = TestContext::new("dir-author").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice direct", "direct")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// GET a direct status as a mentioned recipient → 200.
#[tokio::test]
async fn test_get_direct_status_mentioned_recipient() {
    let ctx = TestContext::new("dir-recipient").await;

    // Alice DMs Bob.
    let status: serde_json::Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "@bob hello dm",
                "visibility": "direct",
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let id = status["id"].as_str().unwrap();

    // Bob (mentioned recipient) should be able to see it.
    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.bob_token))
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "mentioned recipient should see direct status"
    );
}

/// GET a direct status as a non-mentioned third party → 404.
#[tokio::test]
async fn test_get_direct_status_non_recipient() {
    let ctx = TestContext::new("dir-non-recipient").await;

    // Alice DMs Bob (NOT Charlie).
    let status: serde_json::Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "@bob private hello",
                "visibility": "direct",
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let id = status["id"].as_str().unwrap();

    // Charlie is not mentioned → should not see it.
    let (_, charlie_token) = crate::helpers::seed_user(
        &ctx.db,
        &ctx.domain,
        "charlie-dir-nonrec",
        "charlie-dir-nonrec@test.invalid",
    )
    .await;
    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&charlie_token))
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "non-recipient should not see direct status"
    );
}

/// A status with internal "limited" visibility (value=4) is serialized as "private"
/// in API responses, matching Mastodon's behaviour (limited is masked behind private
/// to avoid API contract changes on clients that don't know the limited type).
#[tokio::test]
async fn test_limited_visibility_serialized_as_private() {
    let ctx = TestContext::new("limited-vis").await;

    // Post a normal status, then forcefully set its visibility to 4 (limited) in the DB.
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "limited post", "private")
        .await;
    let id: i64 = status["id"].as_str().unwrap().parse().unwrap();

    sqlx::query!("UPDATE statuses SET visibility = 4 WHERE id = $1", id)
        .execute(&ctx.db)
        .await
        .unwrap();

    let body: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["visibility"].as_str(),
        Some("private"),
        "limited visibility must be serialized as private; got {:?}",
        body["visibility"]
    );
}

// ── block visibility ─────────────────────────────────────────────────────────

/// GET a public status when the viewer has been blocked by the author → 404.
#[tokio::test]
async fn test_get_status_blocked_by_author_returns_404() {
    let ctx = TestContext::new("status-blocked-by-author").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "public post", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    // Alice blocks Bob.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.bob_token))
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "blocked user should not be able to view the blocker's status"
    );
}

/// GET a public status when the viewer has blocked the author → 404.
#[tokio::test]
async fn test_get_status_viewer_blocked_author_returns_404() {
    let ctx = TestContext::new("status-viewer-blocks").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "public post", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    // Bob blocks Alice.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.alice_id),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.bob_token))
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "blocker should not see blocked account's status via direct lookup"
    );
}

// ── reblog restrictions ───────────────────────────────────────────────────────

/// Reblogging a private status of a non-followed account → 404 (status not visible).
#[tokio::test]
async fn test_reblog_private_status_of_stranger_returns_404() {
    let ctx = TestContext::new("reblog-prv-stranger").await;

    // Alice posts private; Bob does NOT follow Alice.
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "private from stranger", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Unreblogs of a private status from a non-followed account → 404.
#[tokio::test]
async fn test_unreblog_private_status_of_stranger_returns_404() {
    let ctx = TestContext::new("unreblog-prv-stranger").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "private never reblogged", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/unreblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Reblogging a private status → 403.
#[tokio::test]
async fn test_reblog_private_returns_403() {
    let ctx = TestContext::new("reblog-prv").await;

    // Bob follows Alice so he can see her private status.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice private rb", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// Author can reblog their own private status (returns 200, not 403).
#[tokio::test]
async fn test_reblog_own_private_status_allowed() {
    let ctx = TestContext::new("reblog-own-prv").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice own private rb", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "owner should be able to reblog their own private status"
    );
    let reblog: Value = resp.json().await.unwrap();
    assert_eq!(
        reblog["reblog"]["id"].as_str(),
        Some(id),
        "reblog should wrap the original"
    );
}

/// Reblogging a direct status by a non-recipient → 404 (status not visible).
#[tokio::test]
async fn test_reblog_direct_returns_404() {
    let ctx = TestContext::new("reblog-dir").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice direct rb", "direct")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    // Direct messages are hidden from non-recipients, so the server returns 404.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Author cannot reblog their own direct message (direct messages are never rebloggable).
#[tokio::test]
async fn test_reblog_own_direct_returns_403() {
    let ctx = TestContext::new("reblog-own-dir").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice own direct", "direct")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "direct messages should never be rebloggable, even by the author"
    );
}

// ── authentication requirements ──────────────────────────────────────────────

/// POST /api/v1/statuses without a token → 401.
#[tokio::test]
async fn test_post_status_requires_auth() {
    let ctx = TestContext::new("auth-post").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            None,
            &json!({"status": "no auth", "visibility": "public"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// DELETE /api/v1/statuses/:id without a token → 401.
#[tokio::test]
async fn test_delete_status_requires_auth() {
    let ctx = TestContext::new("auth-del").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "to delete", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .http
        .delete(ctx.api.url(&format!("/api/v1/statuses/{id}")))
        .header("host", &ctx.domain)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ── soft delete ───────────────────────────────────────────────────────────────

/// Deleted status returns 404 on GET.
#[tokio::test]
async fn test_deleted_status_returns_410() {
    let ctx = TestContext::new("del-410").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "to be deleted", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let del_resp = ctx
        .api
        .delete(&format!("/api/v1/statuses/{id}"), &ctx.alice_token)
        .await;
    assert_eq!(del_resp.status(), StatusCode::OK);

    let get_resp = ctx.api.get(&format!("/api/v1/statuses/{id}"), None).await;
    assert_eq!(get_resp.status(), StatusCode::GONE);
}

/// Non-existent status returns 404 (distinct from deleted which returns 410).
#[tokio::test]
async fn test_nonexistent_status_returns_404() {
    let ctx = TestContext::new("nonexist-404").await;
    let get_resp = ctx.api.get("/api/v1/statuses/9999999999", None).await;
    assert_eq!(get_resp.status(), StatusCode::NOT_FOUND);
}

/// Deleted status is absent from the public timeline.
#[tokio::test]
async fn test_deleted_status_absent_from_public_timeline() {
    let ctx = TestContext::new("del-timeline").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "delete from timeline", "public")
        .await;
    let id = status["id"].as_str().unwrap().to_string();

    ctx.api
        .delete(&format!("/api/v1/statuses/{id}"), &ctx.alice_token)
        .await;

    let timeline = ctx.api.public_timeline().await;
    let ids: Vec<&str> = timeline.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        !ids.contains(&id.as_str()),
        "deleted status still appears in public timeline"
    );
}

/// DELETE /api/v1/statuses/:id response includes the text field (supports delete-and-redraft).
#[tokio::test]
async fn test_delete_status_response_includes_text() {
    let ctx = TestContext::new("del-text").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "delete and redraft me", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let del_resp = ctx
        .api
        .delete(&format!("/api/v1/statuses/{id}"), &ctx.alice_token)
        .await;
    assert_eq!(del_resp.status(), StatusCode::OK);
    let body: Value = del_resp.json().await.unwrap();
    let text = body["text"].as_str().unwrap_or("");
    assert!(
        text.contains("delete and redraft"),
        "deleted status response should include original text"
    );
}

/// Only the author can delete their own status; another user gets 404
/// (Mastodon scopes deletes to current_account.statuses).
#[tokio::test]
async fn test_delete_status_by_non_author_returns_404() {
    let ctx = TestContext::new("del-author").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice status to del", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .delete(&format!("/api/v1/statuses/{id}"), &ctx.bob_token)
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ── favourites ────────────────────────────────────────────────────────────────

/// Favouriting a private status of a non-followed account → 404.
#[tokio::test]
async fn test_favourite_private_status_of_stranger_returns_404() {
    let ctx = TestContext::new("fav-prv-stranger").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice private fav", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/favourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Favouriting a private status of a followed account → 200.
#[tokio::test]
async fn test_favourite_private_status_of_followed_returns_200() {
    let ctx = TestContext::new("fav-prv-followed").await;

    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice private fav followed", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/favourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["favourited"].as_bool(), Some(true));
}

/// Favouriting without auth → 401.
#[tokio::test]
async fn test_favourite_requires_auth() {
    let ctx = TestContext::new("fav-401").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "fav auth test", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/favourite"),
            None,
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Unfavouriting a status that was not favourited returns 200 (idempotent).
#[tokio::test]
async fn test_unfavourite_not_favourited_is_200() {
    let ctx = TestContext::new("unfav-noop").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "never favourited", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/unfavourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["favourited"].as_bool(), Some(false));
}

/// Unfavouriting a private status of a non-followed account → 404.
#[tokio::test]
async fn test_unfavourite_private_status_of_stranger_returns_404() {
    let ctx = TestContext::new("unfav-prv-stranger").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice private unfav", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/unfavourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Favouriting a status increments its favourites_count by 1.
#[tokio::test]
async fn test_favourite_increments_count() {
    let ctx = TestContext::new("fav-inc").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "favourable", "public")
        .await;
    let id = status["id"].as_str().unwrap();
    let before: i64 = status["favourites_count"].as_i64().unwrap_or(0);

    let fav_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/favourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(fav_resp.status(), StatusCode::OK);
    let after: Value = fav_resp.json().await.unwrap();
    assert_eq!(after["favourites_count"].as_i64().unwrap_or(0), before + 1);
}

/// Unfavouriting a status decrements its favourites_count by 1.
#[tokio::test]
async fn test_unfavourite_decrements_count() {
    let ctx = TestContext::new("unfav-dec").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "unfavourable", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    // First favourite it.
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/favourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    let unfav_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/unfavourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(unfav_resp.status(), StatusCode::OK);
    let after: Value = unfav_resp.json().await.unwrap();
    assert_eq!(after["favourites_count"].as_i64().unwrap_or(0), 0);
}

/// Double-favouriting doesn't inflate the count.
#[tokio::test]
async fn test_favourite_is_idempotent() {
    let ctx = TestContext::new("fav-idem").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "fav twice", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/favourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    let second = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/favourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    let body: Value = second.json().await.unwrap();
    assert_eq!(body["favourites_count"].as_i64().unwrap_or(-1), 1);
}

// ── reblog count ──────────────────────────────────────────────────────────────

/// Reblogging a public status increments reblogs_count.
#[tokio::test]
async fn test_reblog_increments_count() {
    let ctx = TestContext::new("reblog-cnt").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "rebloggable", "public")
        .await;
    let id = status["id"].as_str().unwrap();
    let before = status["reblogs_count"].as_i64().unwrap_or(0);

    let rb_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(rb_resp.status(), StatusCode::OK);

    // Fetch the original to check the count.
    let updated: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(updated["reblogs_count"].as_i64().unwrap_or(0), before + 1);
}

/// Reblog response wraps the original in a `reblog` field with reblogged=true.
#[tokio::test]
async fn test_reblog_response_shape() {
    let ctx = TestContext::new("reblog-shape").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "reblog shape", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(
        resp["reblog"]["id"].as_str(),
        Some(id),
        "reblog.id should be the original status id"
    );
    assert_eq!(
        resp["reblog"]["reblogged"], true,
        "reblog.reblogged should be true"
    );
    assert_eq!(resp["reblog"]["reblogs_count"].as_i64(), Some(1));
}

/// Reblogging the same status twice is idempotent — count stays at 1.
#[tokio::test]
async fn test_reblog_idempotent() {
    let ctx = TestContext::new("reblog-idem").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "idempotent reblog", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    // First boost
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    // Second boost of same status
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    let updated: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        updated["reblogs_count"].as_i64(),
        Some(1),
        "double-reblog should not increment reblogs_count twice",
    );
}

/// Reblogging a reblog boosts the original status.
#[tokio::test]
async fn test_reblog_of_reblog_boosts_original() {
    let ctx = TestContext::new("reblog-chain").await;

    // Alice posts; Bob boosts it
    let original = ctx
        .api
        .post_status(&ctx.alice_token, "original post", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    let bob_boost: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{original_id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    let bob_boost_id = bob_boost["id"].as_str().unwrap();

    // Alice then boosts Bob's boost — should be boosting the original
    let alice_boost: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{bob_boost_id}/reblog"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(
        alice_boost["reblog"]["id"].as_str(),
        Some(original_id),
        "boosting a boost should produce a boost of the original",
    );
}

/// Unreblog response is the original status at the top level with reblogged=false.
#[tokio::test]
async fn test_unreblog_response_shape() {
    let ctx = TestContext::new("unreblog-shape").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "unreblog shape", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    let resp: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/unreblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(
        resp["id"].as_str(),
        Some(id),
        "unreblog should return the original status"
    );
    assert_eq!(
        resp["reblogged"], false,
        "reblogged should be false after unreblog"
    );
    assert_eq!(resp["reblogs_count"].as_i64(), Some(0));
}

// ── conversation mute ─────────────────────────────────────────────────────────

/// POST /api/v1/statuses/:id/mute sets muted=true on the status.
#[tokio::test]
async fn test_conversation_mute() {
    let ctx = TestContext::new("conv-mute").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "mutable conv", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/mute"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["muted"], true,
        "status should be muted after POST .../mute"
    );
}

/// POST /api/v1/statuses/:id/unmute sets muted=false on the status.
#[tokio::test]
async fn test_conversation_unmute() {
    let ctx = TestContext::new("conv-unmute").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "unmutable conv", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/mute"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/unmute"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["muted"], false,
        "status should be unmuted after POST .../unmute"
    );
}

// ── status thread & context ──────────────────────────────────────────────────

/// A reply has in_reply_to_id set to the parent status id.
#[tokio::test]
async fn test_reply_sets_in_reply_to_id() {
    let ctx = TestContext::new("reply-id").await;

    let parent = ctx
        .api
        .post_status(&ctx.alice_token, "parent post", "public")
        .await;
    let parent_id = parent["id"].as_str().unwrap();

    let reply_resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "reply text", "in_reply_to_id": parent_id, "visibility": "public"}),
        )
        .await;
    assert_eq!(reply_resp.status(), StatusCode::OK);
    let reply: Value = reply_resp.json().await.unwrap();
    assert_eq!(reply["in_reply_to_id"].as_str(), Some(parent_id));
}

/// A cross-account reply sets in_reply_to_account_id to the parent author's id.
#[tokio::test]
async fn test_reply_sets_in_reply_to_account_id() {
    let ctx = TestContext::new("reply-acct-id").await;

    let parent = ctx
        .api
        .post_status(&ctx.alice_token, "parent for account id test", "public")
        .await;
    let parent_id = parent["id"].as_str().unwrap();

    let reply: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "reply for acct-id", "in_reply_to_id": parent_id, "visibility": "public"}),
    ).await.json().await.unwrap();

    assert_eq!(
        reply["in_reply_to_account_id"].as_str(),
        Some(ctx.alice_id.as_str()),
        "in_reply_to_account_id should be alice's id",
    );
}

/// GET /api/v1/statuses/:id/context returns ancestors and descendants.
#[tokio::test]
async fn test_status_context_ancestors_and_descendants() {
    let ctx = TestContext::new("ctx-thread").await;

    let grandparent = ctx
        .api
        .post_status(&ctx.alice_token, "grandparent", "public")
        .await;
    let gp_id = grandparent["id"].as_str().unwrap();

    let parent: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "parent", "in_reply_to_id": gp_id, "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let p_id = parent["id"].as_str().unwrap();

    let child: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "child", "in_reply_to_id": p_id, "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let c_id = child["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{p_id}/context"), None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let ctx_body: Value = resp.json().await.unwrap();

    let ancestor_ids: Vec<&str> = ctx_body["ancestors"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    let descendant_ids: Vec<&str> = ctx_body["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();

    assert!(
        ancestor_ids.contains(&gp_id),
        "grandparent not in ancestors"
    );
    assert!(descendant_ids.contains(&c_id), "child not in descendants");
}

/// Descendants promote the author's self-replies to the front of the list
/// (Mastodon `descendants(..., promote: true)`), regardless of post time.
#[tokio::test]
async fn test_status_context_promotes_self_replies() {
    let ctx = TestContext::new("ctx-promote").await;

    // Alice posts a root; Bob replies first, then Alice self-replies later.
    let root = ctx
        .api
        .post_status(&ctx.alice_token, "root", "public")
        .await;
    let root_id = root["id"].as_str().unwrap();

    let bob_reply: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({ "status": "bob reply", "in_reply_to_id": root_id, "visibility": "public" }),
        )
        .await
        .json()
        .await
        .unwrap();
    let bob_id = bob_reply["id"].as_str().unwrap().to_string();

    let alice_reply: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({ "status": "alice self-reply", "in_reply_to_id": root_id, "visibility": "public" }),
    ).await.json().await.unwrap();
    let alice_id = alice_reply["id"].as_str().unwrap().to_string();

    let context: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{root_id}/context"), None)
        .await
        .json()
        .await
        .unwrap();
    let descendant_ids: Vec<&str> = context["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();

    // Alice's self-reply (posted later, higher id) must come before Bob's reply.
    let alice_pos = descendant_ids.iter().position(|&d| d == alice_id);
    let bob_pos = descendant_ids.iter().position(|&d| d == bob_id);
    assert!(
        alice_pos < bob_pos,
        "author self-reply should be promoted before other replies: {descendant_ids:?}",
    );
}

// ── status edit & history ────────────────────────────────────────────────────

/// Editing a reblog returns 422 Unprocessable Entity.
#[tokio::test]
async fn test_edit_reblog_returns_422() {
    let ctx = TestContext::new("edit-reblog-422").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "original to boost", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    let rb_resp: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{original_id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    let reblog_id = rb_resp["id"].as_str().unwrap();

    let edit_resp = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{reblog_id}"),
            Some(&ctx.bob_token),
            &json!({"status": "edited reblog", "visibility": "public"}),
        )
        .await;
    assert_eq!(edit_resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Editing a status changes its content.
#[tokio::test]
async fn test_edit_status_changes_content() {
    let ctx = TestContext::new("edit-content").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "original text", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let edit_resp = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({"status": "edited text", "visibility": "public"}),
        )
        .await;
    assert_eq!(edit_resp.status(), StatusCode::OK);
    let edited: Value = edit_resp.json().await.unwrap();
    // content is HTML — spaces may be encoded as &#32;
    let content = edited["content"].as_str().unwrap_or("");
    assert!(
        content.contains("edited"),
        "edited content not found: {content:?}"
    );
}

/// GET /api/v1/statuses/:id/history returns at least two entries after an edit.
#[tokio::test]
async fn test_status_history_after_edit() {
    let ctx = TestContext::new("edit-history").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "v1 text", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({"status": "v2 text", "visibility": "public"}),
        )
        .await;

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/history"), None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let history: Vec<Value> = resp.json().await.unwrap();
    assert!(
        history.len() >= 2,
        "expected at least 2 history entries, got {}",
        history.len()
    );
}

/// Mastodon records an attachment without a description as NULL inside an
/// edit's `media_descriptions`, and the history of every such status answered
/// 500: the array was read as strings. Each version's attachments carry the
/// descriptions they had then, none where they had none.
#[tokio::test]
async fn test_status_history_keeps_each_versions_descriptions() {
    let ctx = TestContext::new("edit-history-descriptions").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "with a picture", "public")
        .await;
    let id: i64 = status["id"].as_str().unwrap().parse().unwrap();
    let account_id: i64 = ctx.alice_id.parse().unwrap();
    let media_id = eunha::snowflake::next_id();
    sqlx::query(
        "INSERT INTO media_attachments (id, status_id, account_id, remote_url, type, description, created_at, updated_at)
         VALUES ($1, $2, $3, 'https://files.example/a.png', 0, 'described now', now(), now())",
    )
    .bind(media_id)
    .bind(id)
    .bind(account_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query("UPDATE statuses SET ordered_media_attachment_ids = $2 WHERE id = $1")
        .bind(id)
        .bind(vec![media_id])
        .execute(&ctx.db)
        .await
        .unwrap();
    // Two past versions as Mastodon writes them: undescribed, then described.
    for (text, description, ago) in [
        ("first", None::<&str>, 2),
        ("second", Some("described then"), 1),
    ] {
        sqlx::query(
            "INSERT INTO status_edits (status_id, account_id, text, spoiler_text, ordered_media_attachment_ids, media_descriptions, created_at, updated_at)
             VALUES ($1, $2, $3, '', $4, $5, now() - make_interval(hours => $6), now())",
        )
        .bind(id)
        .bind(account_id)
        .bind(text)
        .bind(vec![media_id])
        .bind(vec![description])
        .bind(ago)
        .execute(&ctx.db)
        .await
        .unwrap();
    }

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/history"), None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let history: Vec<Value> = resp.json().await.unwrap();
    let descriptions: Vec<&Value> = history
        .iter()
        .map(|version| &version["media_attachments"][0]["description"])
        .collect();
    assert_eq!(
        descriptions,
        [
            &Value::Null,
            &json!("described then"),
            &json!("described now")
        ]
    );
}

/// Every version in the history is rendered, not just the current one.
///
/// Upstream's `StatusEditSerializer#content` is `status_content_format(object)`
/// — the same helper a status goes through — so a past version's newlines and
/// links come back as markup. Eunha used to clean the stored source text for
/// historical entries and render only the current one, which returned a past
/// version as one run-on line with a bare URL in it. The count assertion above
/// could not see that, which is how it lasted.
#[tokio::test]
async fn test_status_history_renders_every_version() {
    let ctx = TestContext::new("edit-history-render").await;

    let status = ctx
        .api
        .post_status(
            &ctx.alice_token,
            "first line\nsecond line with https://example.com/a",
            "public",
        )
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({"status": "rewritten\nstill two lines", "visibility": "public"}),
        )
        .await;

    let history: Vec<Value> = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/history"), None)
        .await
        .json()
        .await
        .unwrap();
    assert!(history.len() >= 2, "expected the original and the edit");

    for (i, version) in history.iter().enumerate() {
        let content = version["content"].as_str().unwrap_or("");
        assert!(
            content.starts_with("<p>"),
            "version {i} is not rendered: {content:?}"
        );
        assert!(
            content.contains("<br />"),
            "version {i} lost its line break: {content:?}"
        );
    }

    // The oldest version is the one that used to come back as source text.
    let original = history[0]["content"].as_str().unwrap_or("");
    assert!(
        original.contains(r#"<a href="https://example.com/a""#),
        "the original version's link was not rendered: {original:?}"
    );
}

/// A no-op edit (identical content) does not record a revision or set edited_at.
#[tokio::test]
async fn test_noop_edit_does_not_record_revision() {
    let ctx = TestContext::new("edit-noop").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "unchanged text", "public")
        .await;
    let id = status["id"].as_str().unwrap();
    assert!(
        status["edited_at"].is_null(),
        "fresh status should have null edited_at"
    );

    // Re-submit the same text/visibility — nothing actually changes.
    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({"status": "unchanged text", "visibility": "public"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let edited: Value = resp.json().await.unwrap();
    assert!(
        edited["edited_at"].is_null(),
        "no-op edit must not set edited_at"
    );

    // History stays at the single (current) version — no extra revision recorded.
    let history: Vec<Value> = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/history"), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        history.len(),
        1,
        "no-op edit must not add a history entry, got {}",
        history.len()
    );

    // A real change still records a revision and sets edited_at.
    let resp2 = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({"status": "actually changed", "visibility": "public"}),
        )
        .await;
    let edited2: Value = resp2.json().await.unwrap();
    assert!(
        !edited2["edited_at"].is_null(),
        "real edit must set edited_at"
    );
    let history2: Vec<Value> = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/history"), None)
        .await
        .json()
        .await
        .unwrap();
    assert!(
        history2.len() >= 2,
        "real edit must add a history entry, got {}",
        history2.len()
    );
}

/// GET /api/v1/statuses/:id/source returns the original plaintext.
#[tokio::test]
async fn test_status_source_returns_text() {
    let ctx = TestContext::new("status-src").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "source text here", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{id}/source"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["text"]
            .as_str()
            .unwrap_or("")
            .contains("source text here"),
        "source text not returned"
    );
}

/// Any authenticated user can read the source of a public status (not just the author).
#[tokio::test]
async fn test_status_source_readable_by_any_authenticated_user() {
    let ctx = TestContext::new("status-src-public").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice's text", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    // Bob is not the author but should still get 200.
    let resp = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{id}/source"),
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["text"].as_str(), Some("alice's text"));
}

/// Source of a private status is visible to followers, 404 to strangers.
#[tokio::test]
async fn test_status_source_private_visible_to_follower() {
    let ctx = TestContext::new("status-src-private").await;

    // alice follows bob
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "bob's private text", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    // Alice (follower) can read it.
    let resp = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{id}/source"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["text"].as_str(), Some("bob's private text"));

    // Carol (a stranger who doesn't follow bob) gets 404.
    let (_, carol_token) = crate::helpers::seed_user(
        &ctx.db,
        &ctx.domain,
        "carol-src-prv",
        "carol-src-prv@test.invalid",
    )
    .await;
    let resp2 = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/source"), Some(&carol_token))
        .await;
    assert_eq!(resp2.status(), StatusCode::NOT_FOUND);
}

/// GET /api/v1/statuses/:id/source after edit returns the updated text.
#[tokio::test]
async fn test_status_source_reflects_edit() {
    let ctx = TestContext::new("status-src-edit").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "original text", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({"status": "updated text after edit"}),
        )
        .await;

    let src: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{id}/source"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        src["text"]
            .as_str()
            .unwrap_or("")
            .contains("updated text after edit"),
        "source should reflect the updated text after editing",
    );
}

/// Editing a status owned by another user returns 404 (Mastodon scopes edits to
/// current_account.statuses).
#[tokio::test]
async fn test_edit_status_by_non_author_returns_404() {
    let ctx = TestContext::new("edit-non-author").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice's status", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.bob_token),
            &json!({"status": "bob tries to edit alice's status"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Favouriting a nonexistent status returns 404.
#[tokio::test]
async fn test_favourite_nonexistent_status_returns_404() {
    let ctx = TestContext::new("fav-nonexist").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses/999999999/favourite",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Reblogging a nonexistent status returns 404.
#[tokio::test]
async fn test_reblog_nonexistent_status_returns_404() {
    let ctx = TestContext::new("reblog-nonexist").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses/999999999/reblog",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Status content warning (spoiler_text) round-trips correctly.
#[tokio::test]
async fn test_spoiler_text_preserved() {
    let ctx = TestContext::new("cw").await;

    let resp = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({"status": "body text", "spoiler_text": "content warning", "visibility": "public"}),
    ).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let status: Value = resp.json().await.unwrap();
    assert_eq!(status["spoiler_text"].as_str(), Some("content warning"));
}

// ── bookmarks ────────────────────────────────────────────────────────────────

/// Bookmarking a private status of a non-followed account → 404.
#[tokio::test]
async fn test_bookmark_private_status_of_stranger_returns_404() {
    let ctx = TestContext::new("bk-prv-stranger").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice private bk", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/bookmark"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Bookmarking a private status of a followed account → 200.
#[tokio::test]
async fn test_bookmark_private_status_of_followed_returns_200() {
    let ctx = TestContext::new("bk-prv-followed").await;

    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice private bk followed", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/bookmark"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["bookmarked"].as_bool(), Some(true));
}

/// Bookmarking a nonexistent status → 404.
#[tokio::test]
async fn test_bookmark_nonexistent_returns_404() {
    let ctx = TestContext::new("bk-404").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses/999999999/bookmark",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Bookmarking without auth → 401.
#[tokio::test]
async fn test_bookmark_requires_auth() {
    let ctx = TestContext::new("bk-401").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "bk auth test", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(&format!("/api/v1/statuses/{id}/bookmark"), None, &json!({}))
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Unbookmarking a status that was not bookmarked returns 200 (idempotent).
#[tokio::test]
async fn test_unbookmark_not_bookmarked_is_200() {
    let ctx = TestContext::new("ubk-noop").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "never bookmarked", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/unbookmark"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["bookmarked"].as_bool(), Some(false));
}

/// Unbookmarking a private status of a non-followed account → 404.
#[tokio::test]
async fn test_unbookmark_private_status_of_stranger_returns_404() {
    let ctx = TestContext::new("ubk-prv-stranger").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice private ubk", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/unbookmark"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Bookmarking and unbookmarking a status.
#[tokio::test]
async fn test_bookmark_and_unbookmark() {
    let ctx = TestContext::new("bookmark").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "bookmarkable", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let bk_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/bookmark"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(bk_resp.status(), StatusCode::OK);
    let bk: Value = bk_resp.json().await.unwrap();
    assert_eq!(bk["bookmarked"].as_bool(), Some(true));

    let ubk_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/unbookmark"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(ubk_resp.status(), StatusCode::OK);
    let ubk: Value = ubk_resp.json().await.unwrap();
    assert_eq!(ubk["bookmarked"].as_bool(), Some(false));
}

/// GET /api/v1/bookmarks returns bookmarked statuses.
#[tokio::test]
async fn test_bookmarks_list() {
    let ctx = TestContext::new("bookmarks-list").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "bookmark me", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/bookmark"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let resp = ctx
        .api
        .get("/api/v1/bookmarks", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(list.iter().any(|s| s["id"].as_str() == Some(id)));
}

// ── pin / unpin ───────────────────────────────────────────────────────────────

/// Pinning and unpinning a status.
#[tokio::test]
async fn test_pin_and_unpin() {
    let ctx = TestContext::new("pin").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "pin me", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let pin_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/pin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(pin_resp.status(), StatusCode::OK);
    let pinned: Value = pin_resp.json().await.unwrap();
    assert_eq!(pinned["pinned"].as_bool(), Some(true));

    let unpin_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/unpin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(unpin_resp.status(), StatusCode::OK);
    let unpinned: Value = unpin_resp.json().await.unwrap();
    assert_eq!(unpinned["pinned"].as_bool(), Some(false));
}

/// Pinning a private (own) status succeeds.
#[tokio::test]
async fn test_pin_private_status() {
    let ctx = TestContext::new("pin-private").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "pin my private post", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/pin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["pinned"].as_bool(), Some(true));
}

/// Pinning another user's status returns 422.
#[tokio::test]
async fn test_pin_other_users_status_returns_422() {
    let ctx = TestContext::new("pin-other").await;

    let status = ctx
        .api
        .post_status(&ctx.bob_token, "bob's pinnable post", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/pin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Pinning a nonexistent status returns 404.
#[tokio::test]
async fn test_pin_nonexistent_status_returns_404() {
    let ctx = TestContext::new("pin-404").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses/999999999/pin",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Pinning without auth returns 401.
#[tokio::test]
async fn test_pin_requires_auth() {
    let ctx = TestContext::new("pin-401").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "pin auth test", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(&format!("/api/v1/statuses/{id}/pin"), None, &json!({}))
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Unpinning a status that is not pinned returns 200 (idempotent).
#[tokio::test]
async fn test_unpin_not_pinned_is_200() {
    let ctx = TestContext::new("unpin-noop").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "never pinned", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/unpin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["pinned"].as_bool(), Some(false));
}

/// Unpinning a nonexistent status returns 404.
#[tokio::test]
async fn test_unpin_nonexistent_status_returns_404() {
    let ctx = TestContext::new("unpin-404").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses/999999999/unpin",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Unpinning without auth returns 401.
#[tokio::test]
async fn test_unpin_requires_auth() {
    let ctx = TestContext::new("unpin-401").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "unpin auth test", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(&format!("/api/v1/statuses/{id}/unpin"), None, &json!({}))
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ── GET /api/v1/accounts/:id/pins ────────────────────────────────────────────

/// Pinned statuses appear in GET /api/v1/accounts/{id}/pins.
#[tokio::test]
async fn test_account_pins_endpoint() {
    let ctx = TestContext::new("acct-pins").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "pinnable post", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/pin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let resp = ctx
        .api
        .get(&format!("/api/v1/accounts/{}/pins", ctx.alice_id), None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let pins: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(pins.len(), 1);
    assert_eq!(pins[0]["id"].as_str(), Some(id));
    assert_eq!(pins[0]["pinned"].as_bool(), Some(true));
}

/// Unpinning removes the status from GET /api/v1/accounts/{id}/pins.
#[tokio::test]
async fn test_account_pins_removes_after_unpin() {
    let ctx = TestContext::new("acct-pins-rm").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "pin then remove", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/pin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/unpin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let pins: Vec<Value> = ctx
        .api
        .get(&format!("/api/v1/accounts/{}/pins", ctx.alice_id), None)
        .await
        .json()
        .await
        .unwrap();
    assert!(pins.is_empty());
}

/// GET /api/v1/accounts/{id}/pins returns empty array for an account with no pins.
#[tokio::test]
async fn test_account_pins_empty() {
    let ctx = TestContext::new("acct-pins-empty").await;

    let pins: Vec<Value> = ctx
        .api
        .get(&format!("/api/v1/accounts/{}/pins", ctx.alice_id), None)
        .await
        .json()
        .await
        .unwrap();
    assert!(pins.is_empty());
}

// ── favourited_by / reblogged_by ──────────────────────────────────────────────

/// GET /api/v1/statuses/:id/favourited_by on a private status (unauthenticated) → 404.
#[tokio::test]
async fn test_favourited_by_private_status_unauthenticated_is_404() {
    let ctx = TestContext::new("fav-by-prv").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice private favby", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/favourited_by"), None)
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET /api/v1/statuses/:id/favourited_by excludes accounts blocked by the viewer.
#[tokio::test]
async fn test_favourited_by_excludes_blocked_accounts() {
    let ctx = TestContext::new("fav-by-block").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "fav-by block test", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    // Bob favourites Alice's status.
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/favourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    // Alice blocks Bob.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Alice views favourited_by — Bob should not appear.
    let list: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{id}/favourited_by"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !list
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())),
        "blocked account appeared in favourited_by",
    );
}

/// GET /api/v1/statuses/:id/favourited_by lists accounts that favourited.
#[tokio::test]
async fn test_favourited_by_list() {
    let ctx = TestContext::new("fav-by").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "fav me bob", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/favourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{id}/favourited_by"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(list
        .iter()
        .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())));
}

/// GET /api/v1/statuses/:id/reblogged_by on a private status (unauthenticated) → 404.
#[tokio::test]
async fn test_reblogged_by_private_status_unauthenticated_is_404() {
    let ctx = TestContext::new("rb-by-prv").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice private rbby", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/reblogged_by"), None)
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET /api/v1/statuses/:id/reblogged_by excludes accounts blocked by the viewer.
#[tokio::test]
async fn test_reblogged_by_excludes_blocked_accounts() {
    let ctx = TestContext::new("rb-by-block").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "rb-by block test", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    // Bob reblogs Alice's status.
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    // Alice blocks Bob.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Alice views reblogged_by — Bob should not appear.
    let list: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{id}/reblogged_by"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !list
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())),
        "blocked account appeared in reblogged_by",
    );
}

/// GET /api/v1/statuses/:id/reblogged_by lists accounts that reblogged.
#[tokio::test]
async fn test_reblogged_by_list() {
    let ctx = TestContext::new("rb-by").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "reblog me bob", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{id}/reblogged_by"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(list
        .iter()
        .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())));
}

/// Unreblog removes the reblog from Bob's statuses.
#[tokio::test]
async fn test_unreblog() {
    let ctx = TestContext::new("unreblog").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice unreblog test", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let rb: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    let rb_id = rb["id"].as_str().unwrap();

    let unrb_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/unreblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(unrb_resp.status(), StatusCode::OK);

    let get_resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{rb_id}"), Some(&ctx.bob_token))
        .await;
    assert_eq!(
        get_resp.status(),
        StatusCode::NOT_FOUND,
        "reblog should be gone after unreblog"
    );
}

/// Unreblgging a status that was never reblogged returns 200 (idempotent).
#[tokio::test]
async fn test_unreblog_not_reblogged_is_200() {
    let ctx = TestContext::new("unreblog-idem").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "never reblogged post", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/unreblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "unreblog of never-reblogged status should return 200"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["reblogged"].as_bool(), Some(false));
}

// ── favourites list ───────────────────────────────────────────────────────────

/// GET /api/v1/favourites returns statuses the user has favourited.
#[tokio::test]
async fn test_favourites_list() {
    let ctx = TestContext::new("favs-list").await;

    let status = ctx
        .api
        .post_status(&ctx.bob_token, "bob's faveable post", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/favourite"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let resp = ctx
        .api
        .get("/api/v1/favourites", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(list.iter().any(|s| s["id"].as_str() == Some(id)));
}

// ── status mute/unmute ────────────────────────────────────────────────────────

/// POST /api/v1/statuses/:id/mute → muted=true; /unmute → muted=false.
#[tokio::test]
async fn test_mute_and_unmute_status() {
    let ctx = TestContext::new("status-mute").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "muteable post", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let mute_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/mute"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(mute_resp.status(), StatusCode::OK);
    let muted: Value = mute_resp.json().await.unwrap();
    assert_eq!(muted["muted"].as_bool(), Some(true));

    let unmute_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/unmute"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(unmute_resp.status(), StatusCode::OK);
    let unmuted: Value = unmute_resp.json().await.unwrap();
    assert_eq!(unmuted["muted"].as_bool(), Some(false));
}

/// Posting with a content warning forces sensitive=true even if not requested
/// (Mastodon `sensitive || spoiler_text.present?`).
#[tokio::test]
async fn test_post_status_spoiler_forces_sensitive() {
    let ctx = TestContext::new("cw-sensitive").await;

    let status: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({ "status": "spoilers within", "spoiler_text": "CW: plot", "visibility": "public" }),
    ).await.json().await.unwrap();
    assert_eq!(
        status["sensitive"].as_bool(),
        Some(true),
        "a CW must force sensitive=true"
    );
    assert_eq!(status["spoiler_text"].as_str(), Some("CW: plot"));
}

/// An unrecognized visibility is rejected, not silently coerced to `direct`.
#[tokio::test]
async fn test_post_status_invalid_visibility_rejected() {
    let ctx = TestContext::new("vis-invalid").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({ "status": "hi", "visibility": "sooper-secret" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// ── polls ──────────────────────────────────────────────────────────────────────

/// POST /api/v1/statuses with a poll returns the poll options in the response.
#[tokio::test]
async fn test_create_status_with_poll() {
    let ctx = TestContext::new("poll-create").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Which do you prefer?",
                "visibility": "public",
                "poll": {
                    "options": ["Option A", "Option B"],
                    "expires_in": 86400
                }
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let status: Value = resp.json().await.unwrap();
    let poll = &status["poll"];
    assert!(poll.is_object(), "poll field missing");
    assert!(poll["id"].as_str().is_some());
    let options = poll["options"].as_array().unwrap();
    assert_eq!(options.len(), 2);
    assert_eq!(options[0]["title"].as_str(), Some("Option A"));
    assert_eq!(options[1]["title"].as_str(), Some("Option B"));
    assert_eq!(poll["expired"].as_bool(), Some(false));
}

/// Mastodon `StatusLengthValidator`: a URL counts as 23 characters, so a status
/// packed with long links well over 500 raw characters is still accepted.
#[tokio::test]
async fn test_status_url_counts_as_23_chars() {
    let ctx = TestContext::new("len-url").await;

    // 19 links × 23 counted chars + 18 spaces = 455 ≤ 500, though the raw text is
    // well over 500 characters.
    let url = "https://example.com/some/really/long/path/that/exceeds/twenty-three/chars";
    let text = vec![url; 19].join(" ");
    assert!(
        text.chars().count() > 500,
        "raw text should exceed 500 chars"
    );

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({ "status": text, "visibility": "public" }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "URLs must count as 23 chars each"
    );
}

/// The content warning / spoiler text counts toward the 500-character limit
/// (Mastodon `combined_text = spoiler_text + countable_text(text)`).
#[tokio::test]
async fn test_spoiler_text_counts_toward_limit() {
    let ctx = TestContext::new("len-spoiler").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "y".repeat(200),
                "spoiler_text": "x".repeat(400),
                "visibility": "public",
            }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "spoiler_text + body over 500 must be rejected",
    );
}

/// Poll option validation parity with Mastodon `PollOptionsValidator` and
/// `PollExpirationValidator`.
#[tokio::test]
async fn test_poll_validation_matches_mastodon() {
    let ctx = TestContext::new("poll-validate").await;

    let post_poll = |poll: Value| {
        let ctx = &ctx;
        async move {
            ctx.api
                .post_json(
                    "/api/v1/statuses",
                    Some(&ctx.alice_token),
                    &json!({ "status": "poll", "visibility": "public", "poll": poll }),
                )
                .await
                .status()
        }
    };

    // Duplicate options.
    assert_eq!(
        post_poll(json!({ "options": ["A", "A"], "expires_in": 86400 })).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "duplicate options must be rejected",
    );
    // Option longer than 50 characters.
    assert_eq!(
        post_poll(json!({ "options": ["a".repeat(51), "B"], "expires_in": 86400 })).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "over-long option must be rejected",
    );
    // Missing expiration (required for local polls).
    assert_eq!(
        post_poll(json!({ "options": ["A", "B"] })).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "missing expires_in must be rejected",
    );
    // Expiration below the 5-minute minimum.
    assert_eq!(
        post_poll(json!({ "options": ["A", "B"], "expires_in": 60 })).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "too-short expiration must be rejected",
    );
    // A valid poll still succeeds.
    assert_eq!(
        post_poll(json!({ "options": ["A", "B"], "expires_in": 86400 })).await,
        StatusCode::OK,
        "valid poll should be accepted",
    );
}

/// A status carrying only a poll (no text, no media) still appears on the
/// author's profile — Mastodon's AccountStatusesFilter does not drop it.
#[tokio::test]
async fn test_poll_only_status_appears_on_profile() {
    let ctx = TestContext::new("poll-profile").await;

    let resp = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({ "poll": { "options": ["A", "B"], "expires_in": 86400 }, "visibility": "public" }),
    ).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let status: Value = resp.json().await.unwrap();
    let sid = status["id"].as_str().unwrap().to_string();
    assert!(
        status["poll"].is_object(),
        "created status should carry a poll"
    );

    let list: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        list.iter().any(|s| s["id"].as_str() == Some(sid.as_str())),
        "poll-only status should appear on the profile timeline",
    );
}

/// When a poll ends, both the poll author and its voters get a `poll`
/// notification — Mastodon exempts `poll` from the self-notification block, so
/// the author is notified about their own poll.
#[tokio::test]
async fn test_poll_ended_notifies_author_and_voters() {
    let ctx = TestContext::new("poll-ended-notif").await;

    let status: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({ "poll": { "options": ["A", "B"], "expires_in": 86400 }, "visibility": "public" }),
    ).await.json().await.unwrap();
    let poll_id = status["poll"]["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/polls/{poll_id}/votes"),
            Some(&ctx.bob_token),
            &json!({ "choices": [0] }),
        )
        .await;

    // Force the poll to have just expired, then run the expiry notifier.
    let poll_id_num: i64 = poll_id.parse().unwrap();
    sqlx::query!(
        "UPDATE polls SET expires_at = now() - interval '10 seconds' WHERE id = $1",
        poll_id_num,
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    eunha::background::notify_expired_polls(&ctx.state)
        .await
        .unwrap();

    let alice_notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        alice_notifs
            .iter()
            .any(|n| n["type"].as_str() == Some("poll")),
        "poll author should be notified when their poll ends",
    );
    let bob_notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        bob_notifs
            .iter()
            .any(|n| n["type"].as_str() == Some("poll")),
        "a voter should be notified when the poll ends",
    );
}

/// The notifier used to look only at polls that had ended in the last two
/// minutes, so a pass that ran late missed them. It now carries forward how far
/// it got, and a pass that slept for a quarter of an hour still reaches a poll
/// that ended ten minutes ago.
#[tokio::test]
async fn test_poll_expiry_reaches_a_poll_that_ended_during_a_long_sleep() {
    let ctx = TestContext::new("poll-ended-long-sleep").await;

    let status: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({ "poll": { "options": ["A", "B"], "expires_in": 86400 }, "visibility": "public" }),
    ).await.json().await.unwrap();
    let poll_id: i64 = status["poll"]["id"].as_str().unwrap().parse().unwrap();
    sqlx::query!(
        "UPDATE polls SET expires_at = now() - interval '10 minutes' WHERE id = $1",
        poll_id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let mark = eunha::background::PollExpiryMark {
        expires_at: chrono::Utc::now() - chrono::Duration::minutes(15),
        id: i64::MAX,
    };
    eunha::background::notify_polls_expired_after(&ctx.state, Some(mark))
        .await
        .unwrap();

    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        notifs.iter().any(|n| n["type"].as_str() == Some("poll")),
        "a poll that ended after the notifier's mark should be notified, however long ago",
    );
}

/// Each pass starts where the last one stopped, so a poll is handled once
/// however often the notifier runs. Counting notifications alone could not show
/// this — `create_and_push` already drops a duplicate — but a second pass used
/// to enqueue the poll's ActivityPub `Update` again, so what is checked is how
/// many polls each pass handled.
#[tokio::test]
async fn test_poll_expiry_handles_each_poll_once() {
    let ctx = TestContext::new("poll-ended-once").await;

    let status: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({ "poll": { "options": ["A", "B"], "expires_in": 86400 }, "visibility": "public" }),
    ).await.json().await.unwrap();
    let poll_id: i64 = status["poll"]["id"].as_str().unwrap().parse().unwrap();
    sqlx::query!(
        "UPDATE polls SET expires_at = now() - interval '10 seconds' WHERE id = $1",
        poll_id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let (mark, handled) = eunha::background::notify_polls_expired_after(&ctx.state, None)
        .await
        .unwrap();
    assert_eq!(handled, 1, "the first pass handles the poll that ended");
    let (next, handled) = eunha::background::notify_polls_expired_after(&ctx.state, Some(mark))
        .await
        .unwrap();
    assert_eq!(
        handled, 0,
        "a second pass must not handle the same poll again"
    );
    assert!(
        next.expires_at >= mark.expires_at,
        "the mark never moves back"
    );

    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let poll_notifications = notifs
        .iter()
        .filter(|n| n["type"].as_str() == Some("poll"))
        .count();
    assert_eq!(
        poll_notifications, 1,
        "a second pass must not notify the same poll again"
    );
}

/// The notifier sleeps until the next running poll ends.
#[tokio::test]
async fn test_next_poll_expiry_is_when_the_next_poll_ends() {
    let ctx = TestContext::new("poll-next-expiry").await;

    assert_eq!(
        eunha::background::next_poll_expiry(&ctx.state)
            .await
            .unwrap(),
        None,
        "with no poll running there is nothing to wake for",
    );

    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({ "poll": { "options": ["A", "B"], "expires_in": 3600 }, "visibility": "public" }),
        )
        .await;
    let seconds = eunha::background::next_poll_expiry(&ctx.state)
        .await
        .unwrap()
        .expect("a running poll has an end");
    assert!(
        (3500.0..=3600.0).contains(&seconds),
        "the poll ends in about an hour, not {seconds}s",
    );
}

/// GET /api/v1/polls/:id returns poll details.
#[tokio::test]
async fn test_get_poll() {
    let ctx = TestContext::new("poll-get").await;

    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Which?",
                "visibility": "public",
                "poll": {"options": ["Yes", "No"], "expires_in": 86400}
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let poll_id = status["poll"]["id"].as_str().unwrap();

    let resp = ctx.api.get(&format!("/api/v1/polls/{poll_id}"), None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let poll: Value = resp.json().await.unwrap();
    assert_eq!(poll["id"].as_str(), Some(poll_id));
}

/// POST /api/v1/polls/:id/votes records the vote and returns voted=true.
#[tokio::test]
async fn test_vote_poll() {
    let ctx = TestContext::new("poll-vote").await;

    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Vote!",
                "visibility": "public",
                "poll": {"options": ["A", "B", "C"], "expires_in": 86400}
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let poll_id = status["poll"]["id"].as_str().unwrap();

    let vote_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{poll_id}/votes"),
            Some(&ctx.bob_token),
            &json!({"choices": [1]}),
        )
        .await;
    assert_eq!(vote_resp.status(), StatusCode::OK);
    let poll: Value = vote_resp.json().await.unwrap();
    assert_eq!(poll["voted"].as_bool(), Some(true));
    assert_eq!(poll["own_votes"].as_array().unwrap(), &[json!(1)]);
}

/// A viewer who has not voted sees `own_votes: []`.
///
/// This asserted `null` until a running Mastodon 4.7.0 was asked and answered
/// `[]`. Both `voted` and `own_votes` are `if: :current_user?` in
/// `REST::PollSerializer`, so an authenticated request gets both, and
/// `Poll#own_votes` returns the choices — an empty list when there are none.
/// A client reading `own_votes.length` found nothing to read.
#[tokio::test]
async fn test_poll_own_votes_null_when_not_voted() {
    let ctx = TestContext::new("poll-own-votes-null").await;

    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Null votes poll",
                "visibility": "public",
                "poll": {"options": ["X", "Y"], "expires_in": 86400}
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let poll_id = status["poll"]["id"].as_str().unwrap();

    // Bob fetches the poll without having voted.
    let poll: Value = ctx
        .api
        .get(&format!("/api/v1/polls/{poll_id}"), Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(
        poll["voted"].as_bool(),
        Some(false),
        "voted should be false before voting"
    );
    assert_eq!(
        poll["own_votes"].as_array().map(Vec::len),
        Some(0),
        "own_votes should be an empty list when the viewer has not voted"
    );

    // And through GET /api/v1/statuses/:id, which builds the poll by a
    // different route — the two agreed on the wrong answer, so both needed
    // fixing.
    let status_id = status["id"].as_str().unwrap();
    let status_resp: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{status_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        status_resp["poll"]["own_votes"].as_array().map(Vec::len),
        Some(0),
        "own_votes should be an empty list here too",
    );
}

/// Voting twice on the same poll returns 422.
#[tokio::test]
async fn test_vote_poll_twice_returns_422() {
    let ctx = TestContext::new("poll-vote2").await;

    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Vote twice?",
                "visibility": "public",
                "poll": {"options": ["A", "B"], "expires_in": 86400}
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let poll_id = status["poll"]["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/polls/{poll_id}/votes"),
            Some(&ctx.bob_token),
            &json!({"choices": [0]}),
        )
        .await;

    let second = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{poll_id}/votes"),
            Some(&ctx.bob_token),
            &json!({"choices": [1]}),
        )
        .await;
    assert_eq!(second.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Voting on a single-choice poll with multiple choices returns 422.
#[tokio::test]
async fn test_vote_poll_multiple_choices_on_single_poll_returns_422() {
    let ctx = TestContext::new("poll-multi-fail").await;

    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Single choice!",
                "visibility": "public",
                "poll": {"options": ["A", "B", "C"], "expires_in": 86400, "multiple": false}
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let poll_id = status["poll"]["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{poll_id}/votes"),
            Some(&ctx.bob_token),
            &json!({"choices": [0, 1]}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Voting with an out-of-bounds choice index returns 422.
#[tokio::test]
async fn test_vote_poll_invalid_choice_index_returns_422() {
    let ctx = TestContext::new("poll-oob").await;

    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Two choices",
                "visibility": "public",
                "poll": {"options": ["A", "B"], "expires_in": 86400}
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let poll_id = status["poll"]["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{poll_id}/votes"),
            Some(&ctx.bob_token),
            &json!({"choices": [99]}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// GET /api/v1/polls/:id for unknown id returns 404.
#[tokio::test]
async fn test_get_poll_not_found() {
    let ctx = TestContext::new("poll-404").await;

    let resp = ctx.api.get("/api/v1/polls/999999999", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Voting on a poll updates per-option votes_count and voters_count.
#[tokio::test]
async fn test_poll_vote_counts() {
    let ctx = TestContext::new("poll-counts").await;

    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Counts poll!",
                "visibility": "public",
                "poll": {"options": ["X", "Y", "Z"], "expires_in": 86400}
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let poll_id = status["poll"]["id"].as_str().unwrap();

    // Bob votes for option Y (index 1).
    ctx.api
        .post_json(
            &format!("/api/v1/polls/{poll_id}/votes"),
            Some(&ctx.bob_token),
            &json!({"choices": [1]}),
        )
        .await;

    let poll: Value = ctx
        .api
        .get(&format!("/api/v1/polls/{poll_id}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(
        poll["votes_count"].as_i64(),
        Some(1),
        "votes_count should be 1"
    );
    // single-choice poll: voters_count must be null per Mastodon spec
    assert!(
        poll["voters_count"].is_null(),
        "single-choice poll voters_count must be null"
    );

    let options = poll["options"].as_array().unwrap();
    assert_eq!(
        options[0]["votes_count"].as_i64(),
        Some(0),
        "option X should have 0 votes"
    );
    assert_eq!(
        options[1]["votes_count"].as_i64(),
        Some(1),
        "option Y should have 1 vote"
    );
    assert_eq!(
        options[2]["votes_count"].as_i64(),
        Some(0),
        "option Z should have 0 votes"
    );
}

/// Per-option votes_count in a status response reflects actual votes (not stale JSON).
#[tokio::test]
async fn test_poll_per_option_counts_in_status_response() {
    let ctx = TestContext::new("poll-status-counts").await;

    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Status poll counts",
                "visibility": "public",
                "poll": {"options": ["P", "Q"], "expires_in": 86400}
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let status_id = status["id"].as_str().unwrap();
    let poll_id = status["poll"]["id"].as_str().unwrap();

    // Bob votes for option Q (index 1).
    ctx.api
        .post_json(
            &format!("/api/v1/polls/{poll_id}/votes"),
            Some(&ctx.bob_token),
            &json!({"choices": [1]}),
        )
        .await;

    // Fetch the status (goes through batch_status_polls, not poll_from_db).
    let fetched: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{status_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    let options = fetched["poll"]["options"].as_array().unwrap();
    assert_eq!(
        options[0]["votes_count"].as_i64(),
        Some(0),
        "option P should have 0 votes in status response"
    );
    assert_eq!(
        options[1]["votes_count"].as_i64(),
        Some(1),
        "option Q should have 1 vote in status response"
    );
    assert_eq!(
        fetched["poll"]["votes_count"].as_i64(),
        Some(1),
        "total votes_count wrong in status response"
    );
}

/// voters_count is null for single-choice polls, non-null for multiple-choice polls.
#[tokio::test]
async fn test_poll_voters_count_nullability() {
    let ctx = TestContext::new("poll-voters-null").await;

    // Single-choice poll → voters_count must be null.
    let s1: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "single choice poll",
                "visibility": "public",
                "poll": {"options": ["A", "B"], "expires_in": 86400, "multiple": false}
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let poll1_id = s1["poll"]["id"].as_str().unwrap();

    // Multiple-choice poll → voters_count must be non-null after a vote.
    let s2: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "multi choice poll",
                "visibility": "public",
                "poll": {"options": ["X", "Y"], "expires_in": 86400, "multiple": true}
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let poll2_id = s2["poll"]["id"].as_str().unwrap();

    // Bob votes on both.
    ctx.api
        .post_json(
            &format!("/api/v1/polls/{poll1_id}/votes"),
            Some(&ctx.bob_token),
            &json!({"choices": [0]}),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/polls/{poll2_id}/votes"),
            Some(&ctx.bob_token),
            &json!({"choices": [0, 1]}),
        )
        .await;

    let p1: Value = ctx
        .api
        .get(&format!("/api/v1/polls/{poll1_id}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        p1["voters_count"].is_null(),
        "single-choice poll voters_count must be null, got: {}",
        p1["voters_count"]
    );

    let p2: Value = ctx
        .api
        .get(&format!("/api/v1/polls/{poll2_id}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        p2["voters_count"].as_i64(),
        Some(1),
        "multiple-choice poll voters_count should be 1 after one voter"
    );
}

/// A multiple-choice poll allows selecting several options.
#[tokio::test]
async fn test_vote_poll_multiple_choice_allowed() {
    let ctx = TestContext::new("poll-multi-ok").await;

    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Multi choice!",
                "visibility": "public",
                "poll": {"options": ["A", "B", "C"], "expires_in": 86400, "multiple": true}
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let poll_id = status["poll"]["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{poll_id}/votes"),
            Some(&ctx.bob_token),
            &json!({"choices": [0, 2]}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let poll: Value = resp.json().await.unwrap();
    assert_eq!(poll["voted"].as_bool(), Some(true));
    let own_votes = poll["own_votes"].as_array().unwrap();
    assert!(own_votes.contains(&json!(0)));
    assert!(own_votes.contains(&json!(2)));
}

/// POST /api/v1/statuses with a poll with only 1 option returns 422.
#[tokio::test]
async fn test_poll_with_one_option_returns_422() {
    let ctx = TestContext::new("poll-1opt").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "single option poll",
                "visibility": "public",
                "poll": {"options": ["Only one"], "expires_in": 86400}
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// POST /api/v1/statuses with a poll with 5 options returns 422.
#[tokio::test]
async fn test_poll_with_five_options_returns_422() {
    let ctx = TestContext::new("poll-5opt").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "five option poll",
                "visibility": "public",
                "poll": {"options": ["A", "B", "C", "D", "E"], "expires_in": 86400}
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// POST /api/v1/statuses with a poll with a blank option returns 422.
#[tokio::test]
async fn test_poll_with_blank_option_returns_422() {
    let ctx = TestContext::new("poll-blank").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "blank option poll",
                "visibility": "public",
                "poll": {"options": ["Valid", ""], "expires_in": 86400}
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// ── default posting visibility ────────────────────────────────────────────────

/// When the user has a default_privacy setting and no visibility is given, that default is used.
#[tokio::test]
async fn test_post_status_uses_default_privacy() {
    let ctx = TestContext::new("default-privacy").await;

    // Set default visibility to "unlisted" via update_credentials.
    let form = reqwest::multipart::Form::new().text("source[privacy]", "unlisted");
    ctx.api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();

    // Post a status without specifying visibility.
    let status: Value = ctx
        .api
        .http
        .post(ctx.api.url("/api/v1/statuses"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"status": "default visibility post"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(
        status["visibility"].as_str(),
        Some("unlisted"),
        "status should use the account's default visibility",
    );
}

/// When the user has default_sensitive=true and no sensitive flag is given, status is sensitive.
#[tokio::test]
async fn test_post_status_uses_default_sensitive() {
    let ctx = TestContext::new("default-sensitive").await;

    // Set default sensitive via update_credentials.
    let form = reqwest::multipart::Form::new().text("source[sensitive]", "true");
    ctx.api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();

    // Post a status without specifying sensitive.
    let status: Value = ctx
        .api
        .http
        .post(ctx.api.url("/api/v1/statuses"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"status": "default sensitive post", "visibility": "public"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(
        status["sensitive"].as_bool(),
        Some(true),
        "status should inherit account's default sensitive setting",
    );
}

// ── status sensitive and language ─────────────────────────────────────────────

/// Status with sensitive=true has sensitive=true in the response.
#[tokio::test]
async fn test_status_sensitive_flag() {
    let ctx = TestContext::new("sensitive").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "CW content",
                "visibility": "public",
                "sensitive": true
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let status: Value = resp.json().await.unwrap();
    assert_eq!(status["sensitive"].as_bool(), Some(true));
}

/// Status with language=es has language=es in the response.
#[tokio::test]
async fn test_status_language_field() {
    let ctx = TestContext::new("lang").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "En español",
                "visibility": "public",
                "language": "es"
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let status: Value = resp.json().await.unwrap();
    assert_eq!(status["language"].as_str(), Some("es"));
}

// ── scheduled statuses ────────────────────────────────────────────────────────

// ── batch GET statuses ────────────────────────────────────────────────────────

/// GET /api/v1/statuses?id[]=... returns the requested statuses.
#[tokio::test]
async fn test_batch_get_statuses() {
    let ctx = TestContext::new("batch-get").await;

    let s1 = ctx
        .api
        .post_status(&ctx.alice_token, "batch status 1", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.alice_token, "batch status 2", "public")
        .await;
    let id1 = s1["id"].as_str().unwrap();
    let id2 = s2["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/statuses?id[]={id1}&id[]={id2}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let statuses: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = statuses.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(ids.contains(&id1), "s1 missing from batch result");
    assert!(ids.contains(&id2), "s2 missing from batch result");
}

/// GET /api/v1/statuses?id[]=... skips statuses from blocked accounts.
#[tokio::test]
async fn test_batch_get_statuses_skips_blocked() {
    let ctx = TestContext::new("batch-blocked").await;

    let blocked_status = ctx
        .api
        .post_status(&ctx.bob_token, "blocked batch status", "public")
        .await;
    let blocked_id = blocked_status["id"].as_str().unwrap();
    let own_status = ctx
        .api
        .post_status(&ctx.alice_token, "own batch status", "public")
        .await;
    let own_id = own_status["id"].as_str().unwrap();

    // Alice blocks Bob.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/statuses?id[]={blocked_id}&id[]={own_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = statuses.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        !ids.contains(&blocked_id),
        "blocked account's status should be skipped in batch"
    );
    assert!(
        ids.contains(&own_id),
        "own status should still appear in batch result"
    );
}

/// GET /api/v1/statuses?id[]=... skips private statuses not visible to viewer.
#[tokio::test]
async fn test_batch_get_statuses_skips_invisible() {
    let ctx = TestContext::new("batch-skip").await;

    let prv = ctx
        .api
        .post_status(&ctx.alice_token, "batch private", "private")
        .await;
    let pub_s = ctx
        .api
        .post_status(&ctx.alice_token, "batch public", "public")
        .await;
    let prv_id = prv["id"].as_str().unwrap();
    let pub_id = pub_s["id"].as_str().unwrap();

    // Bob is not following Alice, so the private status should be silently skipped.
    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/statuses?id[]={prv_id}&id[]={pub_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = statuses.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        !ids.contains(&prv_id),
        "private status should be silently skipped in batch"
    );
    assert!(
        ids.contains(&pub_id),
        "public status should be in batch result"
    );
}

/// GET /api/v1/statuses?id[]=... includes private statuses mentioning the viewer.
#[tokio::test]
async fn test_batch_get_statuses_shows_private_to_mentioned() {
    let ctx = TestContext::new("batch-private-mention").await;

    let prv: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "@bob private batch test",
                "visibility": "private"
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let prv_id = prv["id"].as_str().unwrap();

    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/statuses?id[]={prv_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = statuses.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        ids.contains(&prv_id),
        "mentioned recipient should see private status in batch"
    );
}

/// GET /api/v1/statuses?id[]=... shows a direct status to the mentioned recipient.
#[tokio::test]
async fn test_batch_get_statuses_shows_direct_to_mentioned() {
    let ctx = TestContext::new("batch-direct-mention").await;

    // Alice sends a DM mentioning Bob.
    let dm: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "@bob direct batch test",
                "visibility": "direct"
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let dm_id = dm["id"].as_str().unwrap();

    // Bob (the recipient) can see the DM in a batch request.
    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/statuses?id[]={dm_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = statuses.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        ids.contains(&dm_id),
        "mentioned recipient should see direct status in batch"
    );
}

/// GET /api/v1/statuses?id[]=... with more than 20 IDs returns 422.
#[tokio::test]
async fn test_batch_get_statuses_over_limit_returns_422() {
    let ctx = TestContext::new("batch-limit-422").await;
    let query = (1..=21)
        .map(|i| format!("id[]={i}"))
        .collect::<Vec<_>>()
        .join("&");
    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses?{query}"), Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// ── scheduled statuses ────────────────────────────────────────────────────────

/// POST /api/v1/statuses with scheduled_at returns a scheduled status (201).
#[tokio::test]
async fn test_create_scheduled_status() {
    let ctx = TestContext::new("sched-create").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Scheduled post",
                "visibility": "public",
                "scheduled_at": "2099-01-01T00:00:00Z"
            }),
        )
        .await;
    assert_eq!(
        resp.status().as_u16(),
        201,
        "expected 201 for scheduled status"
    );
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["scheduled_at"].as_str().is_some(),
        "scheduled_at field missing"
    );
    assert!(body["id"].as_str().is_some(), "id field missing");
    assert!(body["params"].is_object(), "params field missing");
}

/// A scheduled_at less than 5 minutes in the future is rejected (Mastodon
/// ScheduledStatus MINIMUM_OFFSET), while a past scheduled_at posts immediately.
#[tokio::test]
async fn test_scheduled_status_offset_rules() {
    let ctx = TestContext::new("sched-offset").await;

    // Two minutes out → too soon → 422.
    let soon = (chrono::Utc::now() + chrono::Duration::minutes(2)).to_rfc3339();
    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({ "status": "too soon", "visibility": "public", "scheduled_at": soon }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "scheduling <5min out must be 422"
    );

    // A past scheduled_at is ignored and the status posts immediately (returns a
    // real Status, not a scheduled one).
    let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({ "status": "post now", "visibility": "public", "scheduled_at": past }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "past scheduled_at should post immediately"
    );
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["scheduled_at"].is_null(),
        "immediate post should not carry scheduled_at"
    );
    assert_eq!(
        body["content"].as_str().map(|c| c.contains("post now")),
        Some(true)
    );
}

/// Posting a status increments statuses_count; deleting decrements it.
#[tokio::test]
async fn test_statuses_count_increments_and_decrements() {
    let ctx = TestContext::new("stat-count").await;

    let before: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.alice_id), None)
        .await
        .json()
        .await
        .unwrap();
    let count_before = before["statuses_count"].as_i64().unwrap_or(0);

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "counting post", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let after_post: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.alice_id), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        after_post["statuses_count"].as_i64().unwrap_or(0),
        count_before + 1,
        "statuses_count should increment after posting",
    );

    ctx.api
        .delete(&format!("/api/v1/statuses/{id}"), &ctx.alice_token)
        .await;

    let after_del: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.alice_id), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        after_del["statuses_count"].as_i64().unwrap_or(0),
        count_before,
        "statuses_count should decrement after deleting",
    );
}

/// GET /api/v1/scheduled_statuses lists previously scheduled statuses.
#[tokio::test]
async fn test_list_scheduled_statuses() {
    let ctx = TestContext::new("sched-list").await;

    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Scheduled for listing",
                "visibility": "public",
                "scheduled_at": "2099-06-01T00:00:00Z"
            }),
        )
        .await;

    let list: Vec<Value> = ctx
        .api
        .get("/api/v1/scheduled_statuses", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!list.is_empty(), "scheduled status not in list");
    assert!(list[0]["scheduled_at"].as_str().is_some());
}

/// GET /api/v1/scheduled_statuses/:id returns a single scheduled status.
#[tokio::test]
async fn test_get_scheduled_status() {
    let ctx = TestContext::new("sched-get").await;

    let created: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "get scheduled",
                "visibility": "public",
                "scheduled_at": "2099-07-01T00:00:00Z"
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/scheduled_statuses/{id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"].as_str(), Some(id));
    assert!(body["scheduled_at"].as_str().is_some());
}

/// GET /api/v1/scheduled_statuses/:id returns 404 for another user's scheduled status.
#[tokio::test]
async fn test_get_scheduled_status_other_user_is_404() {
    let ctx = TestContext::new("sched-get-other").await;

    let created: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "alice scheduled",
                "visibility": "public",
                "scheduled_at": "2099-08-01T00:00:00Z"
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/scheduled_statuses/{id}"),
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// PUT /api/v1/scheduled_statuses/:id updates the scheduled_at time.
#[tokio::test]
async fn test_update_scheduled_status() {
    let ctx = TestContext::new("sched-update").await;

    let created: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "update scheduled",
                "visibility": "public",
                "scheduled_at": "2099-09-01T00:00:00Z"
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();

    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/scheduled_statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({"scheduled_at": "2099-10-01T00:00:00Z"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["scheduled_at"].as_str().unwrap().contains("2099-10"),
        "scheduled_at should be updated"
    );

    // Updating to a time under 5 minutes out is rejected.
    let soon = (chrono::Utc::now() + chrono::Duration::minutes(2)).to_rfc3339();
    let bad = ctx
        .api
        .put_json(
            &format!("/api/v1/scheduled_statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({ "scheduled_at": soon }),
        )
        .await;
    assert_eq!(
        bad.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "updating to <5min out must be rejected"
    );
}

/// DELETE /api/v1/scheduled_statuses/:id cancels a scheduled status.
#[tokio::test]
async fn test_delete_scheduled_status() {
    let ctx = TestContext::new("sched-del").await;

    let created: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "delete scheduled",
                "visibility": "public",
                "scheduled_at": "2099-11-01T00:00:00Z"
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();

    let del_resp = ctx
        .api
        .delete(
            &format!("/api/v1/scheduled_statuses/{id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(del_resp.status(), StatusCode::OK);

    let not_found = ctx
        .api
        .get(
            &format!("/api/v1/scheduled_statuses/{id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(not_found.status(), StatusCode::NOT_FOUND);
}

/// The publisher sleeps until the next schedule is due: at its time, or at its
/// retry time once an attempt has failed, and not at all for a parked one.
#[tokio::test]
async fn test_next_scheduled_status_due() {
    let ctx = TestContext::new("sched-next-due").await;

    assert_eq!(
        eunha::background::next_scheduled_status_due(&ctx.state)
            .await
            .unwrap(),
        None,
        "with nothing scheduled there is nothing to wake for",
    );

    let in_ten_minutes = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();
    let created: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "due in ten minutes",
                "visibility": "public",
                "scheduled_at": in_ten_minutes
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let id: i64 = created["id"].as_str().unwrap().parse().unwrap();
    let seconds = eunha::background::next_scheduled_status_due(&ctx.state)
        .await
        .unwrap()
        .expect("a schedule is waiting");
    assert!(
        (590.0..=600.0).contains(&seconds),
        "the schedule is due in ten minutes, not {seconds}s",
    );

    // Overdue, but backing off after a failed attempt: the retry time counts.
    sqlx::query!(
        "UPDATE scheduled_statuses SET scheduled_at = now() - interval '1 minute' WHERE id = $1",
        id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query!(
        r#"INSERT INTO eunha.scheduled_status_attempts
             (scheduled_status_id, attempts, run_at, last_error, created_at, updated_at)
           VALUES ($1, 1, now() + interval '20 minutes', 'test', now(), now())"#,
        id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let seconds = eunha::background::next_scheduled_status_due(&ctx.state)
        .await
        .unwrap()
        .expect("a retry is waiting");
    assert!(
        (1190.0..=1200.0).contains(&seconds),
        "a schedule backing off is due at its retry, not {seconds}s",
    );

    // Parked after its last attempt: kept, but never due again.
    sqlx::query!(
        "UPDATE eunha.scheduled_status_attempts SET failed_at = now() WHERE scheduled_status_id = $1",
        id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        eunha::background::next_scheduled_status_due(&ctx.state)
            .await
            .unwrap(),
        None,
        "a parked schedule is not waited for",
    );
}

/// A past-due scheduled status is published by the background job and appears in the timeline.
#[tokio::test]
async fn test_scheduled_status_publish_end_to_end() {
    let ctx = TestContext::new("sched-publish").await;

    // Schedule a status validly in the future (Mastodon requires >5 min out).
    let future = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
    let created: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "This was scheduled and should now be published",
                "visibility": "public",
                "scheduled_at": future,
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let sched_id = created["id"].as_str().unwrap();

    // Confirm it's pending.
    let pending = ctx
        .api
        .get(
            &format!("/api/v1/scheduled_statuses/{sched_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(
        pending.status(),
        StatusCode::OK,
        "scheduled status should exist before publish"
    );

    // Force it due, then run the background job.
    let sched_id_num: i64 = sched_id.parse().unwrap();
    sqlx::query!(
        "UPDATE scheduled_statuses SET scheduled_at = now() - interval '1 minute' WHERE id = $1",
        sched_id_num,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    // Run the background job synchronously.
    eunha::background::publish_due_statuses(&ctx.state)
        .await
        .expect("publish_due_statuses failed");

    // The scheduled status entry should be gone.
    let after = ctx
        .api
        .get(
            &format!("/api/v1/scheduled_statuses/{sched_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(
        after.status(),
        StatusCode::NOT_FOUND,
        "scheduled status should be removed after publish"
    );

    // The post should now appear in alice's account statuses.
    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        statuses.iter().any(|s| s["content"]
            .as_str()
            .unwrap_or("")
            .contains("scheduled and should now be published")),
        "published status should appear in account statuses",
    );
}

/// A publish that writes nothing must not destroy the schedule: the post is
/// kept and retried later, rather than vanishing on one bad minute.
#[tokio::test]
async fn test_failed_scheduled_publish_keeps_the_schedule() {
    let ctx = TestContext::new("sched-retry").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();

    // `in_reply_to_id` pointing at a status that does not exist makes the
    // INSERT fail on its foreign key — the publish gets as far as writing and
    // then writes nothing at all.
    let sched_id: i64 = sqlx::query_scalar!(
        r#"INSERT INTO scheduled_statuses (account_id, scheduled_at, params)
           VALUES ($1, now() - interval '1 minute', $2)
           RETURNING id"#,
        alice_id,
        serde_json::json!({
            "text": "this must survive a failed publish",
            "visibility": "public",
            "in_reply_to_id": "999999999999999999",
        }),
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();

    eunha::background::publish_due_statuses(&ctx.state)
        .await
        .expect("a failing publish must not abort the whole run");

    let still_there: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!" FROM scheduled_statuses WHERE id = $1"#,
        sched_id
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        still_there, 1,
        "a failed publish must not delete the schedule"
    );

    let attempt = sqlx::query!(
        "SELECT attempts, failed_at, run_at > now() AS backed_off, last_error
         FROM eunha.scheduled_status_attempts WHERE scheduled_status_id = $1",
        sched_id,
    )
    .fetch_one(&ctx.db)
    .await
    .expect("the failure must be recorded");
    assert_eq!(attempt.attempts, 1, "the attempt must be counted");
    assert!(
        attempt.failed_at.is_none(),
        "one failure must not park the schedule"
    );
    assert_eq!(
        attempt.backed_off,
        Some(true),
        "the retry must be scheduled for later"
    );
    assert!(
        attempt.last_error.is_some(),
        "the failure reason must be kept"
    );

    // Running again immediately must respect the backoff rather than hammering
    // the same doomed row every tick.
    eunha::background::publish_due_statuses(&ctx.state)
        .await
        .unwrap();
    let attempts: i32 = sqlx::query_scalar!(
        "SELECT attempts FROM eunha.scheduled_status_attempts WHERE scheduled_status_id = $1",
        sched_id,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(attempts, 1, "a backed-off schedule must be skipped");

    // Once the backoff elapses it is picked up again.
    sqlx::query!(
        "UPDATE eunha.scheduled_status_attempts SET run_at = now() - interval '1 second'
         WHERE scheduled_status_id = $1",
        sched_id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    eunha::background::publish_due_statuses(&ctx.state)
        .await
        .unwrap();
    let attempts: i32 = sqlx::query_scalar!(
        "SELECT attempts FROM eunha.scheduled_status_attempts WHERE scheduled_status_id = $1",
        sched_id,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        attempts, 2,
        "the schedule must be retried once its backoff expires"
    );
}

/// A schedule that can never produce a status is dropped rather than retried
/// every minute forever.
#[tokio::test]
async fn test_unpublishable_scheduled_status_is_dropped() {
    let ctx = TestContext::new("sched-permanent").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();

    let sched_id: i64 = sqlx::query_scalar!(
        r#"INSERT INTO scheduled_statuses (account_id, scheduled_at, params)
           VALUES ($1, now() - interval '1 minute', NULL)
           RETURNING id"#,
        alice_id,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();

    eunha::background::publish_due_statuses(&ctx.state)
        .await
        .unwrap();

    let remaining: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!" FROM scheduled_statuses WHERE id = $1"#,
        sched_id
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(remaining, 0, "a schedule with no params must be dropped");
}

// ── mentions and tags in status response ──────────────────────────────────────

/// A status with @username mention includes the mentioned account in the mentions array.
#[tokio::test]
async fn test_status_mentions_field() {
    let ctx = TestContext::new("status-mentions").await;

    let status = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": &"@bob hello there".to_string(),
                "visibility": "public"
            }),
        )
        .await
        .json::<Value>()
        .await
        .unwrap();

    let mentions = status["mentions"]
        .as_array()
        .expect("mentions field missing");
    assert!(
        mentions
            .iter()
            .any(|m| m["username"].as_str() == Some("bob")),
        "bob not in mentions: {mentions:?}",
    );
}

/// A status with #hashtag includes the tag in the tags array.
#[tokio::test]
async fn test_status_tags_field() {
    let ctx = TestContext::new("status-tags").await;

    let status = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "testing #uniquetag5555 here",
                "visibility": "public"
            }),
        )
        .await
        .json::<Value>()
        .await
        .unwrap();

    let tags = status["tags"].as_array().expect("tags field missing");
    assert!(
        tags.iter()
            .any(|t| t["name"].as_str() == Some("uniquetag5555")),
        "uniquetag5555 not in tags: {tags:?}",
    );
    // Each tag should have a url field.
    for tag in tags {
        assert!(tag["url"].as_str().is_some(), "tag missing url: {tag:?}");
    }
}

/// GET /api/v1/notifications?since_id=X excludes the anchor and older notifications.
#[tokio::test]
async fn test_notifications_since_id_excludes_anchor() {
    let ctx = TestContext::new("notif-since-anchor").await;

    // First notification: alice follows bob.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let first_notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!first_notifs.is_empty());
    let first_id = first_notifs.last().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Second notification: alice favourites bob's status.
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "since-anchor target", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{sid}/favourite"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // since_id=first_id excludes first_id and anything older.
    let since_notifs: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/notifications?since_id={first_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        !since_notifs
            .iter()
            .any(|n| n["id"].as_str() == Some(&first_id)),
        "since_id anchor itself should be excluded",
    );
    assert!(
        since_notifs
            .iter()
            .any(|n| n["type"].as_str() == Some("favourite")),
        "favourite notification should appear after since_id anchor",
    );
}

/// Reply to a non-existent status returns 422.
#[tokio::test]
async fn test_reply_to_nonexistent_status_returns_422() {
    let ctx = TestContext::new("reply-nonexist").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "this is a reply",
                "visibility": "public",
                "in_reply_to_id": "999999999999"
            }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "replying to non-existent status should return 422",
    );
}

/// Status with no text, no media, and no poll returns 422.
#[tokio::test]
async fn test_post_status_empty_returns_422() {
    let ctx = TestContext::new("status-empty").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "", "visibility": "public"}),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "posting empty status should return 422",
    );
}

/// POST /api/v1/statuses with allowed_mentions rejects posts that mention unlisted accounts.
/// Matches Mastodon's PostStatusService#safeguard_mentions! behaviour.
#[tokio::test]
async fn test_post_status_allowed_mentions_rejects_unexpected() {
    let ctx = TestContext::new("status-allowed-mentions").await;

    // Alice posts mentioning Bob but only lists herself in allowed_mentions.
    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "@bob hello",
                "visibility": "public",
                "allowed_mentions": [ctx.alice_id],
            }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "should reject when a mention is not in allowed_mentions",
    );
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["unexpected_accounts"].is_array(),
        "response should include unexpected_accounts array"
    );
    let unexpected = body["unexpected_accounts"].as_array().unwrap();
    assert!(
        unexpected
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())),
        "Bob should be in unexpected_accounts, got: {unexpected:?}",
    );
}

/// POST /api/v1/statuses with allowed_mentions succeeds when all mentions are listed.
#[tokio::test]
async fn test_post_status_allowed_mentions_accepts_listed() {
    let ctx = TestContext::new("status-allowed-mentions-ok").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "@bob hello",
                "visibility": "public",
                "allowed_mentions": [ctx.bob_id],
            }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "should succeed when all mentions are in allowed_mentions",
    );
}

/// POST /api/v1/statuses with a non-existent media_id returns 422.
#[tokio::test]
async fn test_post_status_invalid_media_id_returns_422() {
    let ctx = TestContext::new("status-bad-media").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "test", "media_ids": ["9999999999999999"]}),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "non-existent media_id should return 422",
    );
}

/// replies_count increments when someone replies and decrements when the reply is deleted.
#[tokio::test]
async fn test_replies_count_increments_and_decrements() {
    let ctx = TestContext::new("replies-count").await;

    let parent = ctx
        .api
        .post_status(&ctx.alice_token, "parent post", "public")
        .await;
    let parent_id = parent["id"].as_str().unwrap();
    let count_before = parent["replies_count"].as_i64().unwrap_or(0);

    // Bob replies to Alice's status.
    let reply: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({
                "status": "replying here",
                "visibility": "public",
                "in_reply_to_id": parent_id
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let reply_id = reply["id"].as_str().unwrap();

    let after_reply: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{parent_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        after_reply["replies_count"].as_i64().unwrap_or(0),
        count_before + 1,
        "replies_count should increment after a reply",
    );

    // Delete the reply.
    ctx.api
        .delete(&format!("/api/v1/statuses/{reply_id}"), &ctx.bob_token)
        .await;

    let after_delete: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{parent_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        after_delete["replies_count"].as_i64().unwrap_or(0),
        count_before,
        "replies_count should decrement after deleting the reply",
    );
}

/// Status exceeding 500 characters returns 422.
#[tokio::test]
async fn test_post_status_over_char_limit_returns_422() {
    let ctx = TestContext::new("status-toolong").await;

    let long_text = "a".repeat(501);
    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": long_text, "visibility": "public"}),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "status over 500 chars should return 422",
    );
}

/// Status of exactly 500 characters succeeds.
#[tokio::test]
async fn test_post_status_at_char_limit_succeeds() {
    let ctx = TestContext::new("status-at-limit").await;

    let exact_text = "a".repeat(500);
    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": exact_text, "visibility": "public"}),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "status of exactly 500 chars should succeed",
    );
}

/// Status response has a filtered field that is an array (not null).
#[tokio::test]
async fn test_status_filtered_field_is_array() {
    let ctx = TestContext::new("status-filtered").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "check filtered field", "public")
        .await;
    assert!(
        status["filtered"].is_array(),
        "filtered field should be an array, got: {:?}",
        status["filtered"],
    );
}

/// Pinning a reblog returns 422.
#[tokio::test]
async fn test_pin_reblog_returns_422() {
    let ctx = TestContext::new("pin-reblog-422").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "original to boost and pin", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    let rb: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{original_id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    let reblog_id = rb["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{reblog_id}/pin"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "pinning a reblog should return 422"
    );
}

/// Pinning more than 5 statuses returns 422.
#[tokio::test]
async fn test_pin_limit_is_five() {
    let ctx = TestContext::new("pin-limit").await;

    let mut ids = Vec::new();
    for i in 0..5 {
        let s = ctx
            .api
            .post_status(&ctx.alice_token, &format!("pin #{i}"), "public")
            .await;
        ids.push(s["id"].as_str().unwrap().to_string());
    }

    // Pin all 5
    for id in &ids {
        let resp = ctx
            .api
            .post_json(
                &format!("/api/v1/statuses/{id}/pin"),
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "pinning status {id} should succeed"
        );
    }

    // Pin a 6th — should fail
    let s6 = ctx
        .api
        .post_status(&ctx.alice_token, "pin #5 (over limit)", "public")
        .await;
    let id6 = s6["id"].as_str().unwrap();
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id6}/pin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "pinning a 6th status should return 422",
    );
}

/// Private statuses in a thread context are hidden from non-followers.
#[tokio::test]
async fn test_context_hides_private_status_from_non_follower() {
    let ctx = TestContext::new("ctx-prv-nonfollower").await;

    // Alice posts a public status
    let public_s = ctx
        .api
        .post_status(&ctx.alice_token, "public root", "public")
        .await;
    let public_id = public_s["id"].as_str().unwrap();

    // Alice replies privately
    let private_reply: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "private reply",
                "visibility": "private",
                "in_reply_to_id": public_id,
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let private_id = private_reply["id"].as_str().unwrap();

    // Bob (not a follower) requests context of the public root
    let context: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{public_id}/context"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    let descendant_ids: Vec<&str> = context["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();

    assert!(
        !descendant_ids.contains(&private_id),
        "private reply should not appear in context for non-follower",
    );
}

/// Private statuses in a thread context are visible to followers.
#[tokio::test]
async fn test_context_shows_private_status_to_follower() {
    let ctx = TestContext::new("ctx-prv-follower").await;

    // Bob follows Alice
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.alice_id),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    // Alice posts a public root
    let public_s = ctx
        .api
        .post_status(&ctx.alice_token, "public root", "public")
        .await;
    let public_id = public_s["id"].as_str().unwrap();

    // Alice replies privately
    let private_reply: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "private reply",
                "visibility": "private",
                "in_reply_to_id": public_id,
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let private_id = private_reply["id"].as_str().unwrap();

    // Bob (a follower) requests context
    let context: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{public_id}/context"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    let descendant_ids: Vec<&str> = context["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();

    assert!(
        descendant_ids.contains(&private_id),
        "private reply should appear in context for a follower",
    );
}

/// Private statuses in a thread context are visible to mentioned non-followers.
#[tokio::test]
async fn test_context_shows_private_status_to_mentioned_non_follower() {
    let ctx = TestContext::new("ctx-prv-mentioned").await;

    let public_s = ctx
        .api
        .post_status(&ctx.bob_token, "public root", "public")
        .await;
    let public_id = public_s["id"].as_str().unwrap();

    let private_reply: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "@bob private reply",
                "visibility": "private",
                "in_reply_to_id": public_id,
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let private_id = private_reply["id"].as_str().unwrap();

    let context: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{public_id}/context"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    let descendant_ids: Vec<&str> = context["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();

    assert!(
        descendant_ids.contains(&private_id),
        "private reply should appear in context for mentioned non-follower",
    );
}

/// Unauthenticated context request hides private statuses.
#[tokio::test]
async fn test_context_hides_private_status_unauthenticated() {
    let ctx = TestContext::new("ctx-prv-unauth").await;

    let public_s = ctx
        .api
        .post_status(&ctx.alice_token, "public root", "public")
        .await;
    let public_id = public_s["id"].as_str().unwrap();

    let private_reply: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "private reply",
                "visibility": "private",
                "in_reply_to_id": public_id,
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let private_id = private_reply["id"].as_str().unwrap();

    let context: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{public_id}/context"), None)
        .await
        .json()
        .await
        .unwrap();

    let descendant_ids: Vec<&str> = context["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();

    assert!(
        !descendant_ids.contains(&private_id),
        "private reply should not appear in context for unauthenticated viewer",
    );
}

/// Boosting a status increments the booster's statuses_count; unboosting decrements it.
#[tokio::test]
async fn test_reblog_increments_booster_statuses_count() {
    let ctx = TestContext::new("boost-stat-count").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "to boost for count", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let before: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.bob_id), None)
        .await
        .json()
        .await
        .unwrap();
    let count_before = before["statuses_count"].as_i64().unwrap_or(0);

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    let after_boost: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.bob_id), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        after_boost["statuses_count"].as_i64().unwrap_or(0),
        count_before + 1,
        "statuses_count should increment when boosting",
    );

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/unreblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    let after_unboost: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.bob_id), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        after_unboost["statuses_count"].as_i64().unwrap_or(0),
        count_before,
        "statuses_count should decrement when unboosting",
    );
}

/// A "hide" filter with context=thread marks matching /context descendants for
/// the client to hide.
#[tokio::test]
async fn test_context_thread_filter_marks_matching_descendant() {
    let ctx = TestContext::new("ctx-thread-filter").await;

    // Alice creates a "thread" hide filter for "spamword".
    ctx.api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &json!({
                "title": "Thread spam filter",
                "context": ["thread"],
                "filter_action": "hide",
                "keywords_attributes": [{"keyword": "spamword", "whole_word": false}]
            }),
        )
        .await;

    // Bob posts a public root.
    let root = ctx
        .api
        .post_status(&ctx.bob_token, "root post", "public")
        .await;
    let root_id = root["id"].as_str().unwrap();

    // Bob replies with a clean reply.
    let clean_reply: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "clean reply", "in_reply_to_id": root_id}),
        )
        .await
        .json()
        .await
        .unwrap();
    let clean_id = clean_reply["id"].as_str().unwrap();

    // Bob replies with a spammy reply.
    let spam_reply: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "this contains spamword", "in_reply_to_id": root_id}),
        )
        .await
        .json()
        .await
        .unwrap();
    let spam_id = spam_reply["id"].as_str().unwrap();

    // Alice fetches context — spam reply should be hidden.
    let context: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{root_id}/context"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    let descendants = context["descendants"].as_array().unwrap();
    assert!(
        descendants
            .iter()
            .any(|s| s["id"].as_str() == Some(clean_id)),
        "clean reply should still appear in thread context",
    );
    // The client hides it; the server marks it.
    let spam = descendants
        .iter()
        .find(|s| s["id"].as_str() == Some(spam_id))
        .expect("a filtered reply is still returned");
    assert_eq!(spam["filtered"][0]["filter"]["filter_action"], "hide");
    assert_eq!(spam["filtered"][0]["filter"]["context"], json!(["thread"]));
}

/// Thread context hides statuses from blocked accounts.
#[tokio::test]
async fn test_context_hides_blocked_account_statuses() {
    let ctx = TestContext::new("ctx-blocked-acct").await;

    // Alice posts a public root.
    let root = ctx
        .api
        .post_status(&ctx.alice_token, "public root for block test", "public")
        .await;
    let root_id = root["id"].as_str().unwrap();

    // Bob replies.
    let bob_reply: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "bob's reply", "in_reply_to_id": root_id}),
        )
        .await
        .json()
        .await
        .unwrap();
    let bob_reply_id = bob_reply["id"].as_str().unwrap();

    // Alice blocks Bob.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Alice fetches context — Bob's reply should be hidden.
    let context: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{root_id}/context"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    let desc_ids: Vec<&str> = context["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();

    assert!(
        !desc_ids.contains(&bob_reply_id),
        "blocked account's reply should be hidden in thread context",
    );
}

/// POST /api/v1/statuses with Idempotency-Key is idempotent — same key returns the same status.
#[tokio::test]
async fn test_post_status_idempotency_key() {
    let ctx = TestContext::new("idempotency-key").await;

    let key = "test-idempotency-key-abc123";
    let client = reqwest::Client::new();

    // First request — creates the status.
    let resp1: Value = client
        .post(format!("{}/api/v1/statuses", ctx.api.base_url))
        .header("Host", &ctx.domain)
        .bearer_auth(&ctx.alice_token)
        .header("Idempotency-Key", key)
        .json(&json!({"status": "idempotency test post"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let id1 = resp1["id"].as_str().expect("first response must have id");

    // Second request with the same key — must return the same status, not create a new one.
    let resp2: Value = client
        .post(format!("{}/api/v1/statuses", ctx.api.base_url))
        .header("Host", &ctx.domain)
        .bearer_auth(&ctx.alice_token)
        .header("Idempotency-Key", key)
        .json(&json!({"status": "idempotency test post"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let id2 = resp2["id"].as_str().expect("second response must have id");

    assert_eq!(id1, id2, "idempotent post should return the same status id");
}

/// A "warn" filter in thread context keeps the status but sets the filtered field.
#[tokio::test]
async fn test_context_thread_filter_warn_annotates_status() {
    let ctx = TestContext::new("ctx-thread-warn").await;

    // Alice creates a "thread" warn filter for "warnword".
    ctx.api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &json!({
                "title": "Thread warn filter",
                "context": ["thread"],
                "filter_action": "warn",
                "keywords_attributes": [{"keyword": "warnword", "whole_word": false}]
            }),
        )
        .await;

    let root = ctx
        .api
        .post_status(&ctx.bob_token, "root post for warn", "public")
        .await;
    let root_id = root["id"].as_str().unwrap();

    let warned_reply: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "reply with warnword", "in_reply_to_id": root_id}),
        )
        .await
        .json()
        .await
        .unwrap();
    let warned_id = warned_reply["id"].as_str().unwrap();

    let context: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{root_id}/context"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    let descendants = context["descendants"].as_array().unwrap();
    let warned_status = descendants
        .iter()
        .find(|s| s["id"].as_str() == Some(warned_id));

    assert!(
        warned_status.is_some(),
        "warned reply should still appear in thread context"
    );
    let filtered = warned_status.unwrap()["filtered"].as_array().unwrap();
    assert!(
        !filtered.is_empty(),
        "warned reply should have non-empty filtered array"
    );
    assert_eq!(
        filtered[0]["filter"]["filter_action"].as_str(),
        Some("warn"),
        "filtered entry should reference the warn filter",
    );
}

/// GET /api/v1/statuses/:id/card returns null when no preview card exists.
#[tokio::test]
async fn test_status_card_null_when_no_url() {
    let ctx = TestContext::new("status-card-null").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "no links here at all", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/card"), None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body.is_null(),
        "card should be null when status has no link, got: {body}"
    );
}

/// GET /api/v1/statuses/:id/card returns 404 for a non-existent status.
#[tokio::test]
async fn test_status_card_not_found() {
    let ctx = TestContext::new("status-card-404").await;

    let resp = ctx.api.get("/api/v1/statuses/999999999/card", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET /api/v1/statuses/:id/card returns 404 for a private status viewed without auth.
#[tokio::test]
async fn test_status_card_private_unauthenticated_is_404() {
    let ctx = TestContext::new("status-card-priv").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "private status for card", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/card"), None)
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// POST /api/v1/statuses/:id/translate returns 404 when no translation service
/// is configured (Mastodon rescues NotConfiguredError with not_found).
#[tokio::test]
async fn test_status_translate_returns_404() {
    let ctx = TestContext::new("status-translate").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "hola mundo", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/translate"),
            Some(&ctx.alice_token),
            &json!({"lang": "en"}),
        )
        .await;
    assert_eq!(
        resp.status().as_u16(),
        404,
        "translate should return 404 when not configured"
    );
}

/// POST /api/v1/statuses response includes the application field for the author.
#[tokio::test]
async fn test_post_status_includes_application_field() {
    let ctx = TestContext::new("status-app-field").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "application field test", "public")
        .await;

    // The application field should be present (and have a name).
    let app = &status["application"];
    assert!(
        app.is_object() && app["name"].as_str().is_some(),
        "status application field should be an object with a name, got: {status:?}",
    );
}

/// The application field must never serialize as null — it should be either
/// a proper object (when an application_id is stored) or absent entirely.
#[tokio::test]
async fn test_status_application_field_never_null() {
    let ctx = TestContext::new("status-app-never-null").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "app field null test", "public")
        .await;
    let sid = status["id"].as_str().unwrap();

    // Check from author's perspective
    let s: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{sid}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let app_val = &s["application"];
    assert!(
        app_val.is_object()
            || app_val.is_null() && !s.as_object().unwrap().contains_key("application"),
        "application must be an object or absent, never null; got: {:?}",
        app_val
    );
    // Since test tokens have an application, it should actually be present
    assert!(
        app_val.is_object(),
        "expected application object since token has app_id, got: {:?}",
        app_val
    );
}

/// Deleting the original status cascade-deletes its reblogs.
///
/// Mastodon: when an original post is removed the server also removes all boosts
/// of that post.  After deletion GET /api/v1/statuses/:reblog_id should return 404.
#[tokio::test]
async fn test_delete_original_cascades_to_reblogs() {
    let ctx = TestContext::new("delete-cascade-reblog").await;

    // Alice posts; Bob boosts it.
    let original = ctx
        .api
        .post_status(&ctx.alice_token, "original to cascade-delete", "public")
        .await;
    let original_id = original["id"].as_str().unwrap().to_string();

    let boost_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{original_id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(boost_resp.status(), StatusCode::OK);
    let boost: Value = boost_resp.json().await.unwrap();
    let reblog_id = boost["id"].as_str().unwrap().to_string();

    // Sanity: reblog is visible before deletion.
    let before = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{reblog_id}"),
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(
        before.status(),
        StatusCode::OK,
        "reblog should be visible before original is deleted"
    );

    // Alice deletes the original.
    let del = ctx
        .api
        .delete(&format!("/api/v1/statuses/{original_id}"), &ctx.alice_token)
        .await;
    assert_eq!(
        del.status(),
        StatusCode::OK,
        "delete original should succeed"
    );

    // Bob's reblog should now be gone (410 because it existed then was deleted).
    let after = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{reblog_id}"),
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(
        after.status(),
        StatusCode::GONE,
        "reblog should be 410 after original is deleted (cascade)",
    );
}

/// favourited_by pagination is consistent: ORDER BY matches the cursor column (a.id).
#[tokio::test]
async fn test_favourited_by_ordering_consistent_with_pagination() {
    let ctx = TestContext::new("fav-by-order").await;

    // Alice and Bob both favourite Charlie's status, but they have different account IDs.
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "fav by order test", "public")
        .await;
    let sid = status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{sid}/favourite"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{sid}/favourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    let all: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{sid}/favourited_by"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(all.len() >= 2, "both alice and bob should appear");

    let ids: Vec<i64> = all
        .iter()
        .filter_map(|a| a["id"].as_str().and_then(|s| s.parse::<i64>().ok()))
        .collect();
    let sorted_desc: Vec<i64> = {
        let mut s = ids.clone();
        s.sort_unstable_by(|a, b| b.cmp(a));
        s
    };
    assert_eq!(
        ids, sorted_desc,
        "favourited_by should be ordered by account id DESC"
    );
}

/// reblogged_by pagination is consistent: ORDER BY matches the cursor column (a.id).
#[tokio::test]
async fn test_reblogged_by_ordering_consistent_with_pagination() {
    let ctx = TestContext::new("rb-by-order").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "rb by order test", "public")
        .await;
    let sid = status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{sid}/reblog"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{sid}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    let all: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{sid}/reblogged_by"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(all.len() >= 2, "both alice and bob should appear");

    let ids: Vec<i64> = all
        .iter()
        .filter_map(|a| a["id"].as_str().and_then(|s| s.parse::<i64>().ok()))
        .collect();
    let sorted_desc: Vec<i64> = {
        let mut s = ids.clone();
        s.sort_unstable_by(|a, b| b.cmp(a));
        s
    };
    assert_eq!(
        ids, sorted_desc,
        "reblogged_by should be ordered by account id DESC"
    );
}

/// GET /api/v1/statuses/:id returns `text` (source) only for the author, null for others.
#[tokio::test]
async fn test_get_status_text_field_for_author_and_others() {
    let ctx = TestContext::new("status-text-field").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "source text test", "public")
        .await;
    let sid = status["id"].as_str().unwrap();

    // Per Mastodon API: GET /api/v1/statuses/:id never includes `text`.
    // Source text is available via GET /api/v1/statuses/:id/source.
    let as_author: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{sid}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        as_author["text"].is_null(),
        "GET /statuses/:id should not include text field, got: {:?}",
        as_author["text"]
    );

    let source: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{sid}/source"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        source["text"].as_str(),
        Some("source text test"),
        "source endpoint should return raw text",
    );
}

/// Local status url should be the human-readable /@username/id format, not the AP URI.
#[tokio::test]
async fn test_local_status_url_is_pretty_format() {
    let ctx = TestContext::new("status-url-format").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "url format test", "public")
        .await;
    let sid = status["id"].as_str().unwrap();

    let s: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{sid}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();

    let url = s["url"].as_str().expect("url should be present");
    let uri = s["uri"].as_str().expect("uri should be present");

    assert!(
        url.contains("/@"),
        "url should contain /@username, got: {url}"
    );
    assert!(
        !url.contains("/users/"),
        "url should not be the AP URI /users/ format, got: {url}"
    );
    assert_ne!(url, uri, "url and uri should differ for local statuses");
}

/// Mastodon omits viewer-dependent fields entirely from unauthenticated responses.
/// When authenticated they are present.
#[tokio::test]
async fn test_status_viewer_fields_omitted_when_unauthenticated() {
    let ctx = TestContext::new("status-viewer-fields").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "viewer field test", "public")
        .await;
    let sid = status["id"].as_str().unwrap();

    // Unauthenticated: fields must be absent.
    let s: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{sid}"), None)
        .await
        .json()
        .await
        .unwrap();
    assert!(
        s.get("favourited").is_none(),
        "favourited must be absent when unauthenticated"
    );
    assert!(
        s.get("reblogged").is_none(),
        "reblogged must be absent when unauthenticated"
    );
    assert!(
        s.get("muted").is_none(),
        "muted must be absent when unauthenticated"
    );
    assert!(
        s.get("bookmarked").is_none(),
        "bookmarked must be absent when unauthenticated"
    );
    assert!(
        s.get("filtered").is_none(),
        "filtered must be absent when unauthenticated"
    );

    // Authenticated: fields must be present.
    let s2: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{sid}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(s2["favourited"].as_bool(), Some(false));
    assert_eq!(s2["reblogged"].as_bool(), Some(false));
    assert_eq!(s2["muted"].as_bool(), Some(false));
    assert_eq!(s2["bookmarked"].as_bool(), Some(false));
    assert!(
        s2["filtered"].as_array().is_some(),
        "filtered should be an array when authenticated"
    );
}

/// Status mentions field should be populated when the status text contains @-mentions.
#[tokio::test]
async fn test_status_mentions_populated() {
    let ctx = TestContext::new("status-mentions").await;

    // Alice mentions bob by username (local mention)
    let text = "@bob hello".to_string();
    let status = ctx.api.post_status(&ctx.alice_token, &text, "public").await;
    let sid = status["id"].as_str().unwrap();

    let s: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{sid}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();

    let mentions = s["mentions"]
        .as_array()
        .expect("mentions should be an array");
    assert_eq!(
        mentions.len(),
        1,
        "should have one mention, got: {:?}",
        mentions
    );

    let mention = &mentions[0];
    assert_eq!(
        mention["username"].as_str(),
        Some("bob"),
        "mention username should be bob"
    );
    assert!(
        mention["id"].as_str().is_some(),
        "mention id should be present"
    );
    assert!(
        mention["acct"].as_str().is_some(),
        "mention acct should be present"
    );
    assert!(
        mention["url"].as_str().is_some(),
        "mention url should be present"
    );
}

/// Posting a lone content warning with no body promotes the CW into the body
/// (leaving no CW) but still marks the status sensitive. Mirrors Mastodon's
/// PostStatusService#preprocess_attributes.
#[tokio::test]
async fn test_post_status_spoiler_only_promotes_to_body() {
    let ctx = TestContext::new("status-spoiler-only").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &serde_json::json!({ "status": "", "spoiler_text": "just a warning" }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "CW-only post should be accepted"
    );
    let s: Value = resp.json().await.unwrap();

    assert_eq!(
        s["spoiler_text"].as_str(),
        Some(""),
        "spoiler should be emptied after promotion"
    );
    assert!(
        s["content"]
            .as_str()
            .unwrap_or_default()
            .contains("just a warning"),
        "CW text should become the body: {}",
        s["content"]
    );
    assert_eq!(
        s["sensitive"].as_bool(),
        Some(true),
        "status should remain sensitive"
    );
}

/// A fully-qualified mention to a local account (`@bob@this.instance`) should
/// resolve to the local user, since local accounts are stored with a NULL
/// domain. Mirrors Mastodon's TagManager#local_domain? normalization.
#[tokio::test]
async fn test_status_mention_local_fully_qualified() {
    let ctx = TestContext::new("status-mention-fq-local").await;

    let text = format!("@bob@{} hello", ctx.domain);
    let status = ctx.api.post_status(&ctx.alice_token, &text, "public").await;
    let sid = status["id"].as_str().unwrap();

    let s: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{sid}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();

    let mentions = s["mentions"]
        .as_array()
        .expect("mentions should be an array");
    assert_eq!(
        mentions.len(),
        1,
        "fully-qualified local mention should resolve, got: {:?}",
        mentions
    );
    assert_eq!(mentions[0]["username"].as_str(), Some("bob"));

    // The rendered content should link the mention rather than leave it as text.
    let content = s["content"].as_str().unwrap_or_default();
    assert!(
        content.contains("h-card"),
        "mention should render as a linked h-card: {content}"
    );
}

/// When a user has set a default language, posting without an explicit language
/// should inherit the default rather than store null.
#[tokio::test]
async fn test_status_language_defaults_to_user_preference() {
    let ctx = TestContext::new("status-lang-default").await;

    // Set alice's default language to "ko"
    let form = reqwest::multipart::Form::new().text("source[language]", "ko");
    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200, "update_credentials failed");

    // Post without specifying language
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "언어 기본값 테스트", "public")
        .await;
    assert_eq!(
        status["language"].as_str(),
        Some("ko"),
        "status language should inherit user default 'ko', got: {:?}",
        status["language"]
    );

    // Post with explicit language override
    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &serde_json::json!({"status": "en test", "visibility": "public", "language": "en"}),
        )
        .await;
    let with_lang: Value = resp.json().await.unwrap();
    assert_eq!(
        with_lang["language"].as_str(),
        Some("en"),
        "explicit language 'en' should override the default"
    );
}

/// GET /api/v1/statuses/:id/history on a private status returns 404 for unauthenticated users.
#[tokio::test]
async fn test_status_history_private_requires_auth() {
    let ctx = TestContext::new("history-private-auth").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "private post", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    // Edit it so there's history to show.
    ctx.api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({"status": "private post v2"}),
        )
        .await;

    // Unauthenticated viewer should get 404.
    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/history"), None)
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "private status history should be 404 for unauthenticated"
    );

    // Author can see it.
    let resp = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{id}/history"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let history: Vec<Value> = resp.json().await.unwrap();
    assert!(
        history.len() >= 2,
        "author should see at least 2 history entries"
    );
}

/// GET /api/v1/statuses/:id/context on a private status returns 404 for unauthenticated users.
#[tokio::test]
async fn test_status_context_private_requires_auth() {
    let ctx = TestContext::new("context-private-auth").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "private root", "private")
        .await;
    let id = status["id"].as_str().unwrap();

    // Unauthenticated viewer should get 404.
    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/context"), None)
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "private status context should be 404 for unauthenticated"
    );

    // Author can see it.
    let resp = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{id}/context"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// When the posting account is sensitized, the author sees their own raw `sensitive`
/// value; other viewers see it overridden to true.
#[tokio::test]
async fn test_status_sensitive_not_overridden_for_author() {
    let ctx = TestContext::new("sensitive-author").await;

    // Mark alice's account as sensitized at the DB level.
    sqlx::query!(
        "UPDATE accounts SET sensitized_at = now() WHERE id = $1",
        ctx.alice_id.parse::<i64>().unwrap(),
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    // Alice posts a non-sensitive status.
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "not marked sensitive", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    // Alice (author) sees her own stored value: sensitive = false.
    let s_author: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        s_author["sensitive"].as_bool(),
        Some(false),
        "author should see their own sensitive=false"
    );

    // Bob (another viewer) sees sensitive = true because of sensitized_at.
    let s_viewer: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        s_viewer["sensitive"].as_bool(),
        Some(true),
        "other viewer should see sensitive=true due to sensitized account"
    );
}

/// `pinned` field is present only for the author, absent for other viewers.
#[tokio::test]
async fn test_status_pinned_field_only_for_author() {
    let ctx = TestContext::new("pinned-author-only").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "pin me", "public")
        .await;
    let id = status["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/pin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Author sees `pinned`.
    let s_author: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        s_author.get("pinned").is_some(),
        "author should see pinned field"
    );
    assert_eq!(s_author["pinned"].as_bool(), Some(true));

    // Another authenticated user does not see `pinned`.
    let s_other: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        s_other.get("pinned").is_none(),
        "other viewer should not see pinned field"
    );
}

/// GET /api/v1/statuses/:id/quotes returns an empty array (quotes not yet supported).
#[tokio::test]
async fn test_status_quotes_returns_array() {
    let ctx = TestContext::new("status-quotes").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "quotable", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{id}/quotes"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body.as_array().is_some(),
        "quotes endpoint must return an array"
    );
}

// ── quote posts ──────────────────────────────────────────────────────────────

/// Posting with quote_id embeds the quoted status in the response.
#[tokio::test]
async fn test_create_quote_post_embeds_quoted_status() {
    let ctx = TestContext::new("quote-create-embed").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "original post", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "my quote", "quoted_status_id": original_id, "visibility": "public"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    assert_eq!(body["visibility"].as_str(), Some("public"));
    let quote = &body["quote"];
    assert!(!quote.is_null(), "quote field should be populated");
    let quoted_status = &quote["quoted_status"];
    assert!(
        !quoted_status.is_null(),
        "quoted_status field should be populated"
    );
    assert_eq!(quoted_status["id"].as_str(), Some(original_id));
    assert_eq!(
        quoted_status["content"]
            .as_str()
            .map(|s| s.contains("original post")),
        Some(true)
    );
}

/// A silenced account's public post is downgraded to unlisted, matching
/// Mastodon's PostStatusService#preprocess_attributes.
#[tokio::test]
async fn test_silenced_account_public_downgraded_to_unlisted() {
    let ctx = TestContext::new("silenced-downgrade").await;

    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    sqlx::query("UPDATE accounts SET silenced_at = now() WHERE id = $1")
        .bind(alice_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "hello from a silenced account", "visibility": "public"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["visibility"].as_str(),
        Some("unlisted"),
        "a silenced account's public post must be downgraded to unlisted"
    );
}

/// A direct-message quote of someone else's post must mention the quoted
/// author, else it is rejected (Mastodon safeguard_private_mention_quote!).
#[tokio::test]
async fn test_direct_quote_requires_mentioning_quoted_author() {
    let ctx = TestContext::new("direct-quote-safeguard").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "alice public post", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    // Direct quote without mentioning Alice → 422.
    let resp = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "sneaky private quote", "quoted_status_id": original_id, "visibility": "direct"}),
    ).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "direct quote without mentioning the quoted author must be rejected"
    );

    // Direct quote that mentions Alice → accepted.
    let resp = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "@alice look at this", "quoted_status_id": original_id, "visibility": "direct"}),
    ).await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "direct quote mentioning the quoted author should be accepted"
    );
}

/// Quoting a followers-only status forces the quote down to followers-only,
/// even when the client asks for public (PostStatusService#preprocess_attributes).
/// Only the author may quote it: a local post no one else may see has its quote
/// policy downgraded to none (`downgrade_quote_policy`), so anyone else is
/// refused by `StatusPolicy#quote?`.
#[tokio::test]
async fn test_quote_of_private_forces_private_visibility() {
    let ctx = TestContext::new("quote-private-downgrade").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "followers only", "private")
        .await;
    let original_id = original["id"].as_str().unwrap();

    let resp = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({"status": "quoting myself", "quoted_status_id": original_id, "visibility": "public"}),
    ).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["visibility"].as_str(),
        Some("private"),
        "a public quote of a followers-only post must be downgraded to private"
    );

    let resp = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "quoting alice", "quoted_status_id": original_id, "visibility": "private"}),
    ).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// Quoting increments quotes_count on the original status.
#[tokio::test]
async fn test_quote_post_increments_quotes_count() {
    let ctx = TestContext::new("quote-count").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "countable", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();
    assert_eq!(original["quotes_count"].as_i64(), Some(0));

    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "quoting!", "quoted_status_id": original_id, "visibility": "public"}),
        )
        .await;

    let fetched: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{original_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(fetched["quotes_count"].as_i64(), Some(1));
}

/// GET /api/v1/statuses/:id/quotes lists quote posts of a status.
#[tokio::test]
async fn test_get_status_quotes_lists_quotes() {
    let ctx = TestContext::new("quote-list").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "quotable status", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    // Post two quotes
    let q1 = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "first quote", "quoted_status_id": original_id, "visibility": "public"}),
    ).await.json::<Value>().await.unwrap();
    let _q2 = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({"status": "second quote", "quoted_status_id": original_id, "visibility": "public"}),
    ).await.json::<Value>().await.unwrap();

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{original_id}/quotes"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 2, "should list both quotes");
    // Each entry should have a quote field pointing back to the original
    assert_eq!(
        arr[0]["quote"]["quoted_status"]["id"].as_str(),
        Some(original_id)
    );
    assert_eq!(
        arr[1]["quote"]["quoted_status"]["id"].as_str(),
        Some(original_id)
    );
    // q1 ID should appear in the list
    let ids: Vec<&str> = arr.iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&q1["id"].as_str().unwrap()));
}

/// GET /api/v1/statuses/:id/quotes on nonexistent status returns 404.
#[tokio::test]
async fn test_get_status_quotes_nonexistent_returns_404() {
    let ctx = TestContext::new("quote-list-404").await;
    let resp = ctx
        .api
        .get(
            "/api/v1/statuses/9999999999999/quotes",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Quoting a nonexistent status returns 422.
#[tokio::test]
async fn test_quote_nonexistent_status_returns_422() {
    let ctx = TestContext::new("quote-nonexistent").await;
    let resp = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({"status": "quoting thin air", "quoted_status_id": "9999999999999", "visibility": "public"}),
    ).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Quoting a direct message returns 422.
#[tokio::test]
async fn test_quote_direct_message_returns_422() {
    let ctx = TestContext::new("quote-direct").await;
    let dm = ctx
        .api
        .post_status(&ctx.alice_token, "secret DM", "direct")
        .await;
    let dm_id = dm["id"].as_str().unwrap();
    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "quoting DM", "quoted_status_id": dm_id, "visibility": "public"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Deleting a quote post decrements quotes_count.
#[tokio::test]
async fn test_delete_quote_decrements_count() {
    let ctx = TestContext::new("quote-delete-count").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "to be quoted", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    let quote_resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "quoting!", "quoted_status_id": original_id, "visibility": "public"}),
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    let quote_id = quote_resp["id"].as_str().unwrap();

    // Verify count went up
    let fetched: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{original_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(fetched["quotes_count"].as_i64(), Some(1));

    // Delete the quote
    ctx.api
        .delete(&format!("/api/v1/statuses/{quote_id}"), &ctx.bob_token)
        .await;

    // Verify count went back down
    let fetched2: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{original_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(fetched2["quotes_count"].as_i64(), Some(0));
}

/// The status object serializes quote_approval with automatic/manual/current_user fields.
#[tokio::test]
async fn test_status_has_quote_approval_field() {
    let ctx = TestContext::new("quote-approval-field").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "public post", "public")
        .await;
    // tagged_collections must be present as an empty array (Mastodon API contract).
    assert!(
        status["tagged_collections"].is_array(),
        "tagged_collections must be present as an array, got: {}",
        status["tagged_collections"],
    );
    assert!(
        status["quote_approval"].is_object(),
        "quote_approval must be an object"
    );
    let qa = &status["quote_approval"];
    assert!(qa["automatic"].is_array());
    assert!(qa["manual"].is_array());
    assert!(qa["current_user"].is_string());
    // Public posts allow quoting by everyone
    let auto_arr = qa["automatic"].as_array().unwrap();
    assert!(
        !auto_arr.is_empty(),
        "public posts should have non-empty automaticApproval"
    );
    assert_eq!(qa["current_user"].as_str(), Some("automatic"));
}

/// PUT /api/v1/statuses/:id/interaction_policy sets `quote_approval_policy`
/// (`public`, `followers` or `nobody`) and returns the updated status.
#[tokio::test]
async fn test_update_interaction_policy() {
    let ctx = TestContext::new("interaction-policy").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "policy test post", "public")
        .await;
    let status_id = status["id"].as_str().unwrap();
    assert_eq!(status["quote_approval"]["automatic"], json!(["public"]));

    let updated: Value = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{status_id}/interaction_policy"),
            Some(&ctx.alice_token),
            &json!({"quote_approval_policy": "followers"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(updated["quote_approval"]["automatic"], json!(["followers"]));
    assert_eq!(updated["quote_approval"]["manual"], json!([]));
    // The author may always quote.
    assert_eq!(
        updated["quote_approval"]["current_user"].as_str(),
        Some("automatic")
    );

    let seen_by_bob: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{status_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        seen_by_bob["quote_approval"]["current_user"].as_str(),
        Some("denied")
    );

    let bad = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{status_id}/interaction_policy"),
            Some(&ctx.alice_token),
            &json!({"quote_approval_policy": "everyone"}),
        )
        .await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// PUT /api/v1/statuses/:id/interaction_policy returns 403 when acting on another user's status.
#[tokio::test]
async fn test_update_interaction_policy_forbidden_for_other_user() {
    let ctx = TestContext::new("interaction-policy-forbidden").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "alice's post", "public")
        .await;
    let status_id = status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{status_id}/interaction_policy"),
            Some(&ctx.bob_token),
            &json!({"can_quote": {"always": [], "with_approval": []}}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// POST /api/v1/statuses/:id/unreblog returns 200 even when the status author
/// subsequently blocks the viewer (Mastodon contract: unreblog is always allowed).
#[tokio::test]
async fn test_unreblog_when_blocked_by_author_returns_200() {
    let ctx = TestContext::new("unreblog-blocked-author").await;

    // Alice posts a public status.
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "unreblog-block test", "public")
        .await;
    let status_id = status["id"].as_str().unwrap();

    // Bob reblogs it.
    let rb = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{status_id}/reblog"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(rb.status(), StatusCode::OK);

    // Alice blocks Bob.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;

    // Bob unreblogs — should succeed even though Alice blocked him.
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{status_id}/unreblog"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "unreblog should succeed even when blocked by author"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["reblogged"].as_bool(), Some(false));
}

// ── quote post contract fixes ─────────────────────────────────────────────────

/// Quoting a reblog is not allowed; must return 422.
#[tokio::test]
async fn test_quote_reblog_unwraps_to_original() {
    let ctx = TestContext::new("quote-reblog-unwrap").await;

    // Alice posts the original
    let original = ctx
        .api
        .post_status(&ctx.alice_token, "original post", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    // Bob boosts it
    let reblog: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{original_id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    let reblog_id = reblog["id"].as_str().unwrap();

    // Quoting a reblog is not allowed — must return 422
    let resp = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({"status": "quoting a boost", "quoted_status_id": reblog_id, "visibility": "public"}),
    ).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "quoting a reblog must return 422"
    );
}

/// Quoting a post by a user who blocked you returns 422.
#[tokio::test]
async fn test_quote_blocked_by_quotee_returns_422() {
    let ctx = TestContext::new("quote-blocked-by").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "alice's post", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    // Alice blocks Bob
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let resp = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "quoting despite block", "quoted_status_id": original_id, "visibility": "public"}),
    ).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Quoting a post when you blocked the author returns 422.
#[tokio::test]
async fn test_quote_quoter_blocked_quotee_returns_422() {
    let ctx = TestContext::new("quote-blocked-quotee").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "alice's post", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    // Bob blocks Alice
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.alice_id),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    let resp = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "quoting despite block", "quoted_status_id": original_id, "visibility": "public"}),
    ).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// A quote still waiting for its author's consent (as one of a remote post
/// waits) shows its state, and does not embed the quoted post.
#[tokio::test]
async fn test_pending_quote_is_null_in_response() {
    let ctx = TestContext::new("quote-pending").await;
    let original: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "quote me", "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let original_id = original["id"].as_str().unwrap();
    let quote: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "quoting", "quoted_status_id": original_id, "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let quote_id = quote["id"].as_str().unwrap();
    sqlx::query("UPDATE quotes SET state = 0 WHERE status_id = $1")
        .bind(quote_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();

    let quote: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{quote_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !quote["quote"].is_null(),
        "pending quote must appear in the response"
    );
    assert_eq!(quote["quote"]["state"].as_str(), Some("pending"));
    assert!(
        quote["quote"]["quoted_status"].is_null(),
        "pending quote must not embed quoted_status"
    );
}

/// GET /api/v1/statuses/:id/quotes does not return pending quotes.
#[tokio::test]
async fn test_get_quotes_only_returns_accepted() {
    let ctx = TestContext::new("quote-list-accepted-only").await;
    let original: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "quote me", "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let original_id = original["id"].as_str().unwrap();
    let quote: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "quoting", "quoted_status_id": original_id, "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let quote_id = quote["id"].as_str().unwrap();
    sqlx::query("UPDATE quotes SET state = 0 WHERE status_id = $1")
        .bind(quote_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();

    let quotes: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{original_id}/quotes"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        quotes.as_array().unwrap().len(),
        0,
        "pending quotes must not appear in GET /quotes"
    );
}

/// GET /api/v1/statuses/:id/quotes does not return rejected quotes.
#[tokio::test]
async fn test_get_quotes_excludes_rejected() {
    let ctx = TestContext::new("quote-list-rejected").await;
    let original: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "quote me", "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let original_id = original["id"].as_str().unwrap();
    let quote: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "quoting", "quoted_status_id": original_id, "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let quote_id = quote["id"].as_str().unwrap();
    sqlx::query("UPDATE quotes SET state = 2 WHERE status_id = $1")
        .bind(quote_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();

    let quotes: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{original_id}/quotes"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        quotes.as_array().unwrap().len(),
        0,
        "rejected quotes must not appear in GET /quotes"
    );
}

/// GET /api/v1/statuses/:id/quotes requires authentication.
#[tokio::test]
async fn test_get_quotes_requires_auth() {
    let ctx = TestContext::new("quote-list-auth").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "a post", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{original_id}/quotes"), None)
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "GET /quotes must require authentication"
    );
}

/// POST /api/v1/statuses with quote_approval_policy sets the quote_approval field correctly.
#[tokio::test]
async fn test_post_status_quote_approval_policy() {
    let ctx = TestContext::new("quote-approval-policy").await;

    // Default (no param) → automatic: ["public"]
    let public_post: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "public quote policy", "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        public_post["quote_approval"]["automatic"]
            .as_array()
            .unwrap(),
        &[serde_json::json!("public")],
        "default quote_approval_policy must be automatic:public"
    );
    assert_eq!(
        public_post["quote_approval"]["current_user"].as_str(),
        Some("automatic"),
    );

    // nobody → automatic: [], manual: []
    let nobody_post: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({"status": "nobody quote policy", "visibility": "public", "quote_approval_policy": "nobody"}),
    ).await.json().await.unwrap();
    assert_eq!(
        nobody_post["quote_approval"]["automatic"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "nobody policy must have empty automatic array"
    );
    // `quote_policy_for_account`: the author may always quote their own post.
    assert_eq!(
        nobody_post["quote_approval"]["current_user"].as_str(),
        Some("automatic"),
    );

    // followers → automatic: ["followers"]
    let followers_post: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({"status": "followers quote policy", "visibility": "public", "quote_approval_policy": "followers"}),
    ).await.json().await.unwrap();
    assert_eq!(
        followers_post["quote_approval"]["automatic"]
            .as_array()
            .unwrap(),
        &[serde_json::json!("followers")],
        "followers policy must have automatic:followers"
    );
}

/// GET /api/v1/statuses/:id/quotes: owner of quoted post sees private quotes; others do not.
#[tokio::test]
async fn test_get_quotes_private_visibility_rules() {
    let ctx = TestContext::new("quote-list-private-visibility").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "original", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    // Bob quotes with private visibility (accepted because original is public)
    ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "private quote", "quoted_status_id": original_id, "visibility": "private"}),
    ).await;

    // Alice owns the quoted post so she CAN see Bob's private quote
    let quotes_alice: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{original_id}/quotes"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        quotes_alice.as_array().unwrap().len(),
        1,
        "owner of quoted post should see private quoting posts"
    );

    // Bob (not the quoted-post owner) cannot see his own private quote in this list
    let quotes_bob: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{original_id}/quotes"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        quotes_bob.as_array().unwrap().len(),
        0,
        "non-owner viewer must not see private quoting posts"
    );
}

/// POST /api/v1/statuses/:status_id/quotes/:id/revoke — success case.
#[tokio::test]
async fn test_revoke_quote_success() {
    let ctx = TestContext::new("quote-revoke-success").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "alice's original", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    let quote: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob's quote", "quoted_status_id": original_id, "visibility": "public"}),
    ).await.json().await.unwrap();
    let quote_status_id = quote["id"].as_str().unwrap();

    // Alice (quoted post author) revokes the quote
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{original_id}/quotes/{quote_status_id}/revoke"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"].as_str(), Some(quote_status_id));
    // After revoke the quote field must be present with state "revoked"
    assert!(
        !body["quote"].is_null(),
        "quote must be present after revoke"
    );
    assert_eq!(
        body["quote"]["state"].as_str(),
        Some("revoked"),
        "quote state must be 'revoked' after revocation"
    );
}

/// POST revoke by wrong user (not the quoted post author) returns 403.
#[tokio::test]
async fn test_revoke_quote_wrong_user_returns_403() {
    let ctx = TestContext::new("quote-revoke-403").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "alice's original", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    let quote: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob's quote", "quoted_status_id": original_id, "visibility": "public"}),
    ).await.json().await.unwrap();
    let quote_status_id = quote["id"].as_str().unwrap();

    // Bob tries to revoke his own quote — only Alice (the quoted author) can do this
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{original_id}/quotes/{quote_status_id}/revoke"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// POST revoke without a token returns 401.
#[tokio::test]
async fn test_revoke_quote_unauthenticated_returns_401() {
    let ctx = TestContext::new("quote-revoke-401").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "alice's original", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    let quote: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob's quote", "quoted_status_id": original_id, "visibility": "public"}),
    ).await.json().await.unwrap();
    let quote_status_id = quote["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{original_id}/quotes/{quote_status_id}/revoke"),
            None,
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Deleted quoted status causes quote.state = "deleted" with no quoted_status.
#[tokio::test]
async fn test_quote_state_deleted_when_quoted_post_removed() {
    let ctx = TestContext::new("quote-state-deleted").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "will be deleted", "public")
        .await;
    let original_id = original["id"].as_str().unwrap();

    let quote: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({"status": "bob quotes alice", "quoted_status_id": original_id, "visibility": "public"}),
    ).await.json().await.unwrap();
    let quote_status_id = quote["id"].as_str().unwrap();

    // Confirm quote is present before deletion
    assert!(
        !quote["quote"].is_null(),
        "quote should be present before deletion"
    );

    // Alice deletes the original
    ctx.api
        .delete(&format!("/api/v1/statuses/{original_id}"), &ctx.alice_token)
        .await;

    // Fetch the quoting post — quote.state should now be "deleted"
    let refetched: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{quote_status_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    let q = &refetched["quote"];
    assert!(!q.is_null(), "quote object should still be present");
    assert_eq!(
        q["state"].as_str(),
        Some("deleted"),
        "state should be 'deleted' after quoted post removed"
    );
    assert!(
        q["quoted_status"].is_null(),
        "quoted_status should be null when deleted"
    );
}

/// PATCH /api/v1/accounts/update_credentials with source[quote_policy] persists the setting
/// and subsequent statuses use it as their default.
#[tokio::test]
async fn test_update_credentials_quote_policy() {
    let ctx = TestContext::new("quote-policy-pref").await;

    // Default is "public"
    let creds: Value = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(creds["source"]["quote_policy"].as_str(), Some("public"));

    // Set to "nobody"
    ctx.api
        .patch_multipart(
            "/api/v1/accounts/update_credentials",
            &ctx.alice_token,
            &[("source[quote_policy]", "nobody")],
        )
        .await;

    let creds: Value = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        creds["source"]["quote_policy"].as_str(),
        Some("nobody"),
        "quote_policy should be updated"
    );

    // New post without explicit quote_approval_policy should inherit the user default
    let post: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "nobody may quote me", "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        post["quote_approval"]["automatic"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "nobody policy: automatic must be empty"
    );
    // The author may always quote their own post; nobody else may.
    assert_eq!(
        post["quote_approval"]["current_user"].as_str(),
        Some("automatic"),
        "the author may always quote"
    );
}

/// When a user sets source[quote_policy]=followers, new statuses use "followers" automatic approval.
#[tokio::test]
async fn test_default_quote_policy_followers_applied_to_new_status() {
    let ctx = TestContext::new("quote-policy-followers-default").await;

    ctx.api
        .patch_multipart(
            "/api/v1/accounts/update_credentials",
            &ctx.alice_token,
            &[("source[quote_policy]", "followers")],
        )
        .await;

    let post: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "followers may quote", "visibility": "public"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        post["quote_approval"]["automatic"].as_array().unwrap(),
        &[serde_json::json!("followers")],
        "followers policy: automatic must contain 'followers'"
    );
}
