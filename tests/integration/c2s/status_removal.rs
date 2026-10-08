//! Removing a status as `RemoveStatusService` does: destroyed with what Rails
//! destroys with it, kept for moderators when cited, purged a month later,
//! its media left for a redraft, and its featured tags counted out.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{tiny_png, TestContext};

async fn statuses_count(ctx: &TestContext, account_id: &str) -> i64 {
    let account: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{account_id}"), None)
        .await
        .json()
        .await
        .unwrap();
    account["statuses_count"].as_i64().unwrap()
}

async fn rows(ctx: &TestContext, sql: &str, id: i64) -> i64 {
    sqlx::query_scalar::<_, i64>(sql)
        .bind(id)
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

async fn upload(ctx: &TestContext) -> String {
    let media: Value = ctx
        .api
        .post_multipart_file(
            "/api/v1/media",
            &ctx.alice_token,
            "t.png",
            "image/png",
            tiny_png(),
            &[],
        )
        .await
        .json()
        .await
        .unwrap();
    media["id"].as_str().unwrap().to_owned()
}

async fn post_with_media(ctx: &TestContext, media_id: &str) -> reqwest::Response {
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "look", "visibility": "public", "media_ids": [media_id]}),
        )
        .await
}

/// A deleted status is destroyed: the row, its favourites and their
/// notifications go, and only then does it stop counting.
#[tokio::test]
async fn test_delete_destroys_status_and_dependents() {
    let ctx = TestContext::new("rm-destroy").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "going away", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    let id: i64 = sid.parse().unwrap();
    for path in ["favourite", "bookmark"] {
        let resp = ctx
            .api
            .post_json(
                &format!("/api/v1/statuses/{sid}/{path}"),
                Some(&ctx.bob_token),
                &json!({}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    assert_eq!(statuses_count(&ctx, &ctx.alice_id).await, 1);

    let resp = ctx
        .api
        .delete(&format!("/api/v1/statuses/{sid}"), &ctx.alice_token)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    assert_eq!(
        rows(&ctx, "SELECT count(*) FROM statuses WHERE id = $1", id).await,
        0
    );
    for sql in [
        "SELECT count(*) FROM favourites WHERE status_id = $1",
        "SELECT count(*) FROM bookmarks WHERE status_id = $1",
    ] {
        assert_eq!(rows(&ctx, sql, id).await, 0, "{sql}");
    }
    let favourite_notifications: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE activity_type = 'Favourite' AND type = 'favourite'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(favourite_notifications, 0);
    assert_eq!(statuses_count(&ctx, &ctx.alice_id).await, 0);
    // `Status.find` finds nothing, as for any status that is not there.
    let get = ctx.api.get(&format!("/api/v1/statuses/{sid}"), None).await;
    assert_eq!(get.status(), StatusCode::NOT_FOUND);
}

/// `delete_media` decides whether the media goes: left unattached for a
/// redraft, or destroyed with the status.
#[tokio::test]
async fn test_delete_keeps_media_for_redraft_unless_asked() {
    let ctx = TestContext::new("rm-redraft").await;
    let media_id = upload(&ctx).await;
    let first: Value = post_with_media(&ctx, &media_id).await.json().await.unwrap();
    let resp = ctx
        .api
        .delete(
            &format!("/api/v1/statuses/{}", first["id"].as_str().unwrap()),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mid: i64 = media_id.parse().unwrap();
    assert_eq!(
        rows(
            &ctx,
            "SELECT count(*) FROM media_attachments WHERE id = $1 AND status_id IS NULL",
            mid
        )
        .await,
        1,
        "the attachment waits for the redraft"
    );

    let redraft = post_with_media(&ctx, &media_id).await;
    assert_eq!(
        redraft.status(),
        StatusCode::OK,
        "the redraft takes the media"
    );
    let redraft: Value = redraft.json().await.unwrap();
    let resp = ctx
        .api
        .delete(
            &format!(
                "/api/v1/statuses/{}?delete_media=true",
                redraft["id"].as_str().unwrap()
            ),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        rows(
            &ctx,
            "SELECT count(*) FROM media_attachments WHERE id = $1",
            mid
        )
        .await,
        0
    );
}

/// A status cited by an unresolved report is kept, discarded and still
/// counted, until the daily cleanup purges it a month later.
#[tokio::test]
async fn test_reported_status_kept_until_cleanup() {
    let ctx = TestContext::new("rm-reported").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "reported", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    let id: i64 = sid.parse().unwrap();
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    sqlx::query(
        "INSERT INTO reports (account_id, target_account_id, status_ids, comment, created_at, updated_at)
         VALUES ($1, $2, ARRAY[$3]::bigint[], '', now(), now())",
    )
    .bind(bob)
    .bind(alice)
    .bind(id)
    .execute(&ctx.db)
    .await
    .unwrap();

    let resp = ctx
        .api
        .delete(&format!("/api/v1/statuses/{sid}"), &ctx.alice_token)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        rows(
            &ctx,
            "SELECT count(*) FROM statuses WHERE id = $1 AND deleted_at IS NOT NULL",
            id
        )
        .await,
        1,
        "kept for moderators"
    );
    assert_eq!(statuses_count(&ctx, &ctx.alice_id).await, 1);
    // Kept, but `Status.find` and `@account.statuses.find` do not find it:
    // 404 everywhere, never 410.
    for path in [
        format!("/api/v1/statuses/{sid}"),
        format!("/api/v1/statuses/{sid}/context"),
        format!("/api/v1/statuses/{sid}/favourited_by"),
    ] {
        let resp = ctx.api.get(&path, Some(&ctx.alice_token)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
    }
    for path in [
        format!("/users/alice/statuses/{sid}"),
        format!("/users/alice/statuses/{sid}/activity"),
    ] {
        let resp = ctx.api.ap_get(&path, None).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
    }

    // Not yet a month: nothing to purge.
    assert_eq!(
        eunha::remove_status::clean_discarded_statuses(&ctx.state)
            .await
            .unwrap(),
        0
    );
    sqlx::query("UPDATE statuses SET deleted_at = now() - interval '31 days' WHERE id = $1")
        .bind(id)
        .execute(&ctx.db)
        .await
        .unwrap();
    assert_eq!(
        eunha::remove_status::clean_discarded_statuses(&ctx.state)
            .await
            .unwrap(),
        1
    );
    ctx.state.jobs.settle().await;
    eunha::jobs::drain(&ctx.state).await.unwrap();
    ctx.state.jobs.settle().await;
    assert_eq!(
        rows(&ctx, "SELECT count(*) FROM statuses WHERE id = $1", id).await,
        0
    );
    assert_eq!(statuses_count(&ctx, &ctx.alice_id).await, 0);
}

/// Unboosting destroys the boost and gives the count back.
#[tokio::test]
async fn test_unreblog_destroys_boost() {
    let ctx = TestContext::new("rm-unreblog").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "boost me", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    let boost: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{sid}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    let boost_id: i64 = boost["id"].as_str().unwrap().parse().unwrap();
    let undone: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{sid}/unreblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(undone["reblogged"], json!(false));
    assert_eq!(undone["reblogs_count"], json!(0));
    assert_eq!(
        rows(
            &ctx,
            "SELECT count(*) FROM statuses WHERE id = $1",
            boost_id
        )
        .await,
        0
    );
    assert_eq!(statuses_count(&ctx, &ctx.bob_id).await, 0);
}

async fn featured_count(ctx: &TestContext, name: &str) -> String {
    let list: Vec<Value> = ctx
        .api
        .get("/api/v1/featured_tags", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    list.iter()
        .find(|t| t["name"].as_str() == Some(name))
        .map(|t| t["statuses_count"].as_str().unwrap().to_owned())
        .unwrap()
}

/// A featured tag counts the posts everyone may see: from the posts already
/// there when it is created (`reset_data`), up with a new public one, not with
/// a private one, and down when one is deleted.
#[tokio::test]
async fn test_featured_tag_counts_distributable_posts() {
    let ctx = TestContext::new("rm-featured").await;
    ctx.api
        .post_status(&ctx.alice_token, "one #apple", "public")
        .await;
    ctx.api
        .post_status(&ctx.alice_token, "secret #apple", "private")
        .await;
    let resp = ctx
        .api
        .post_json(
            "/api/v1/featured_tags",
            Some(&ctx.alice_token),
            &json!({"name": "apple"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let created: Value = resp.json().await.unwrap();
    assert_eq!(created["statuses_count"], json!("1"));

    let second = ctx
        .api
        .post_status(&ctx.alice_token, "two #apple", "unlisted")
        .await;
    ctx.api
        .post_status(&ctx.alice_token, "hidden #apple", "private")
        .await;
    assert_eq!(featured_count(&ctx, "apple").await, "2");

    ctx.api
        .delete(
            &format!("/api/v1/statuses/{}", second["id"].as_str().unwrap()),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(featured_count(&ctx, "apple").await, "1");
}

/// A local status saved through the model sends `status.updated`: here, a
/// change to its quote policy.
#[tokio::test]
async fn test_status_updated_webhook_on_quote_policy_change() {
    use std::sync::{Arc, Mutex};
    let ctx = TestContext::new("rm-webhook").await;
    let received: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = received.clone();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |body: String| {
            let sink = sink.clone();
            async move {
                sink.lock().unwrap().push(body);
                "ok"
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    sqlx::query(
        "INSERT INTO webhooks (url, events, secret, enabled, created_at, updated_at)
         VALUES ($1, '{status.updated}', 'a-long-enough-secret', true, now(), now())",
    )
    .bind(format!("http://{addr}/hook"))
    .execute(&ctx.db)
    .await
    .unwrap();

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "policy", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{sid}/interaction_policy"),
            Some(&ctx.alice_token),
            &json!({"quote_approval_policy": "followers"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let mut delivered = None;
    for _ in 0..50 {
        if let Some(body) = received.lock().unwrap().first().cloned() {
            delivered = Some(body);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let event: Value = serde_json::from_str(&delivered.expect("status.updated delivered")).unwrap();
    assert_eq!(event["event"], "status.updated");
    assert_eq!(event["object"]["id"], json!(sid));
}
