//! `statuses.ordered_media_attachment_ids`: the order a post's attachments
//! were asked for, which `Status#ordered_media_attachments` shows them in.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// An upload of alice's, not yet on a status, with the given description.
async fn upload(ctx: &TestContext, description: &str) -> i64 {
    let id = eunha::snowflake::next_id();
    sqlx::query(
        "INSERT INTO media_attachments (id, account_id, remote_url, type, description, created_at, updated_at)
         VALUES ($1, $2, $3, 0, $4, now(), now())",
    )
    .bind(id)
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .bind(format!("https://files.example/{id}.png"))
    .bind(description)
    .execute(&ctx.db)
    .await
    .unwrap();
    id
}

fn ids(status: &Value) -> Vec<String> {
    status["media_attachments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect()
}

async fn ordered(ctx: &TestContext, status_id: &str) -> Option<Vec<i64>> {
    sqlx::query_scalar("SELECT ordered_media_attachment_ids FROM statuses WHERE id = $1")
        .bind(status_id.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

/// `PostStatusService` records the order `media_ids` gave, and the status,
/// its ActivityPub object and its single view all show the attachments in
/// it, not in the order they were uploaded.
#[tokio::test]
async fn test_a_post_keeps_the_order_its_media_were_asked_for() {
    let ctx = TestContext::new("media-order-post").await;
    let first = upload(&ctx, "first").await;
    let second = upload(&ctx, "second").await;
    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "two pictures",
                "media_ids": [second.to_string(), first.to_string(), second.to_string()],
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let status: Value = resp.json().await.unwrap();
    let id = status["id"].as_str().unwrap().to_owned();
    let expected = vec![second.to_string(), first.to_string()];
    assert_eq!(ids(&status), expected);
    assert_eq!(ordered(&ctx, &id).await, Some(vec![second, first]));

    let shown: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(ids(&shown), expected);

    let note: Value = ctx
        .api
        .ap_get(&format!("/users/alice/statuses/{id}"), None)
        .await
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = note["attachment"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["name"].as_str())
        .collect();
    assert_eq!(names, ["second", "first"]);

    // A post with no media records an empty order, as `[] & []` is.
    let plain = ctx
        .api
        .post_status(&ctx.alice_token, "no pictures", "public")
        .await;
    assert_eq!(
        ordered(&ctx, plain["id"].as_str().unwrap()).await,
        Some(vec![])
    );
}

/// `UpdateStatusService` takes a new order as an edit, and an attachment the
/// edit leaves out stays attached for the history, which still shows it,
/// while the status no longer does.
#[tokio::test]
async fn test_an_edit_reorders_and_drops_media_as_mastodon_does() {
    let ctx = TestContext::new("media-order-edit").await;
    let first = upload(&ctx, "first").await;
    let second = upload(&ctx, "second").await;
    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "pictures", "media_ids": [first.to_string(), second.to_string()]}),
        )
        .await
        .json()
        .await
        .unwrap();
    let id = status["id"].as_str().unwrap().to_owned();

    // Only the order changes, and that is an edit.
    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({"status": "pictures", "media_ids": [second.to_string(), first.to_string()]}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let edited: Value = resp.json().await.unwrap();
    assert_eq!(ids(&edited), [second.to_string(), first.to_string()]);
    assert!(edited["edited_at"].is_string());

    // Dropping one: it is no longer shown, but it is still the status's.
    let edited: Value = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({"status": "pictures", "media_ids": [first.to_string()]}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(ids(&edited), [first.to_string()]);
    let still: Option<i64> =
        sqlx::query_scalar("SELECT status_id FROM media_attachments WHERE id = $1")
            .bind(second)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(still, Some(id.parse::<i64>().unwrap()));

    let history: Vec<Value> = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/history"), None)
        .await
        .json()
        .await
        .unwrap();
    let versions: Vec<Vec<String>> = history.iter().map(ids).collect();
    assert_eq!(
        versions,
        [
            vec![first.to_string(), second.to_string()],
            vec![second.to_string(), first.to_string()],
            vec![first.to_string()],
        ]
    );

    // An upload attached to another post cannot be taken.
    let other = upload(&ctx, "other").await;
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "elsewhere", "media_ids": [other.to_string()]}),
        )
        .await;
    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({"status": "pictures", "media_ids": [other.to_string()]}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        format!("Media {other} not found or already attached to another post")
    );
}

/// A status written before the order was recorded shows every attachment
/// by id, in its history as in itself.
#[tokio::test]
async fn test_a_status_without_an_order_shows_its_media_by_id() {
    let ctx = TestContext::new("media-order-legacy").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "old post", "public")
        .await;
    let id = status["id"].as_str().unwrap().to_owned();
    let first = upload(&ctx, "first").await;
    let second = upload(&ctx, "second").await;
    sqlx::query("UPDATE media_attachments SET status_id = $1 WHERE id = ANY($2)")
        .bind(id.parse::<i64>().unwrap())
        .bind(vec![first, second])
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query("UPDATE statuses SET ordered_media_attachment_ids = NULL WHERE id = $1")
        .bind(id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let shown: Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(ids(&shown), [first.to_string(), second.to_string()]);
    let history: Vec<Value> = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/history"), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(ids(&history[0]), [first.to_string(), second.to_string()]);
}
