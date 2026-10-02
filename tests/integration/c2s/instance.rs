use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// GET /api/v1/instance returns valid instance data.
#[tokio::test]
async fn test_instance_v1() {
    let ctx = TestContext::new("instance-v1").await;

    let resp = ctx.api.get("/api/v1/instance", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    assert!(body["uri"].as_str().is_some(), "uri field missing");
    assert!(body["title"].as_str().is_some(), "title field missing");
    assert!(body["version"].as_str().is_some(), "version field missing");
}

/// GET /api/v2/instance returns valid instance data including usage.
#[tokio::test]
async fn test_instance_v2() {
    let ctx = TestContext::new("instance-v2").await;

    let resp = ctx.api.get("/api/v2/instance", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    assert!(body["domain"].as_str().is_some(), "domain field missing");
    assert!(body["version"].as_str().is_some(), "version field missing");
}

/// GET /api/v1/instance/extended_description returns 200.
#[tokio::test]
async fn test_instance_extended_description() {
    let ctx = TestContext::new("inst-ext").await;

    let resp = ctx
        .api
        .get("/api/v1/instance/extended_description", None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// GET /api/v1/instance/peers returns an array.
#[tokio::test]
async fn test_instance_peers() {
    let ctx = TestContext::new("inst-peers").await;

    let resp = ctx.api.get("/api/v1/instance/peers", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<String> = resp.json().await.unwrap();
}

/// GET /api/v1/instance/privacy_policy returns 200.
#[tokio::test]
async fn test_instance_privacy_policy() {
    let ctx = TestContext::new("inst-priv").await;

    let resp = ctx.api.get("/api/v1/instance/privacy_policy", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// GET /api/v1/instance/translation_languages returns 200.
#[tokio::test]
async fn test_instance_translation_languages() {
    let ctx = TestContext::new("inst-trans").await;

    let resp = ctx
        .api
        .get("/api/v1/instance/translation_languages", None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// POST /api/v1/apps registers an application and returns credentials.
#[tokio::test]
async fn test_register_app() {
    let ctx = TestContext::new("oauth-app").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({
                "client_name": "Test App",
                "redirect_uris": "urn:ietf:wg:oauth:2.0:oob",
                "scopes": "read write"
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    assert!(body["client_id"].as_str().is_some(), "client_id missing");
    assert!(
        body["client_secret"].as_str().is_some(),
        "client_secret missing"
    );
    assert_eq!(body["name"].as_str(), Some("Test App"));
}

/// GET /api/v1/announcements returns an array (empty when none published).
#[tokio::test]
async fn test_get_announcements() {
    let ctx = TestContext::new("announce").await;

    let resp = ctx
        .api
        .get("/api/v1/announcements", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<Value> = resp.json().await.unwrap();
}

/// GET /api/v1/custom_emojis returns a JSON array.
#[tokio::test]
async fn test_get_custom_emojis() {
    let ctx = TestContext::new("emojis").await;

    let resp = ctx.api.get("/api/v1/custom_emojis", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<Value> = resp.json().await.unwrap();
}

/// GET /api/v1/trends/tags returns a JSON array.
#[tokio::test]
async fn test_trending_tags() {
    let ctx = TestContext::new("trends-tags").await;

    let resp = ctx.api.get("/api/v1/trends/tags", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<Value> = resp.json().await.unwrap();
}

/// GET /api/v1/trends/statuses returns a JSON array.
#[tokio::test]
async fn test_trending_statuses() {
    let ctx = TestContext::new("trends-stat").await;

    let resp = ctx.api.get("/api/v1/trends/statuses", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<Value> = resp.json().await.unwrap();
}

/// GET /api/v1/trends/links returns a JSON array.
#[tokio::test]
async fn test_trending_links() {
    let ctx = TestContext::new("trends-links").await;

    let resp = ctx.api.get("/api/v1/trends/links", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<Value> = resp.json().await.unwrap();
}

/// POST /api/v1/emails/confirmations returns 200 (no-op stub).
#[tokio::test]
async fn test_email_confirmations_endpoint() {
    let ctx = TestContext::new("email-confirm").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/emails/confirmations",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// GET /api/v1/announcements returns read=false before dismissal, read=true after.
#[tokio::test]
async fn test_announcement_dismiss() {
    let ctx = TestContext::new("ann-dismiss").await;

    // Insert a published announcement via direct DB write.
    let pool = ctx.db.clone();

    let ann_id: i64 = sqlx::query_scalar!(
        r#"INSERT INTO announcements (text, published, all_day, published_at, created_at, updated_at)
           VALUES ('test announcement', true, false, now(), now(), now())
           RETURNING id"#,
    )
    .fetch_one(&pool)
    .await
    .unwrap();

    // Get announcements — should appear as unread.
    let before: Vec<Value> = ctx
        .api
        .get("/api/v1/announcements", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let ann = before
        .iter()
        .find(|a| a["id"].as_str().and_then(|s| s.parse::<i64>().ok()) == Some(ann_id));
    assert!(ann.is_some(), "announcement should appear in list");
    assert_eq!(
        ann.unwrap()["read"].as_bool(),
        Some(false),
        "should be unread initially"
    );

    // Dismiss it.
    let dismiss_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/announcements/{ann_id}/dismiss"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(dismiss_resp.status(), StatusCode::OK);

    // After dismissal it should appear as read.
    let after: Vec<Value> = ctx
        .api
        .get("/api/v1/announcements", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let ann2 = after
        .iter()
        .find(|a| a["id"].as_str().and_then(|s| s.parse::<i64>().ok()) == Some(ann_id));
    assert!(
        ann2.is_some(),
        "announcement should still appear after dismiss"
    );
    assert_eq!(
        ann2.unwrap()["read"].as_bool(),
        Some(true),
        "should be read after dismiss"
    );
}

/// GET /api/v1/instance/activity returns an array of weekly stats.
#[tokio::test]
async fn test_instance_activity_returns_array() {
    let ctx = TestContext::new("inst-activity").await;

    // Post a status so there's activity to count
    ctx.api
        .post_status(&ctx.alice_token, "some activity", "public")
        .await;

    let resp = ctx.api.get("/api/v1/instance/activity", None).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.unwrap();
    assert!(body.is_array(), "instance/activity should return an array");

    // Each entry should have the required fields as strings
    if let Some(entry) = body.as_array().and_then(|a| a.first()) {
        assert!(
            entry["week"].is_string(),
            "week should be a string timestamp"
        );
        assert!(entry["statuses"].is_string(), "statuses should be a string");
        assert!(entry["logins"].is_string(), "logins should be a string");
        assert!(
            entry["registrations"].is_string(),
            "registrations should be a string"
        );
    }
}

/// GET /api/v1/instance/rules returns an array (may be empty).
#[tokio::test]
async fn test_instance_rules_returns_array() {
    let ctx = TestContext::new("inst-rules").await;

    let resp = ctx.api.get("/api/v1/instance/rules", None).await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.is_array(),
        "instance/rules should return an array, got: {body:?}"
    );
}

/// GET /api/v1/peers/search?q= returns matching peer domains.
#[tokio::test]
async fn test_peers_search_returns_array() {
    let ctx = TestContext::new("peers-search").await;

    let resp = ctx.api.get("/api/v1/peers/search?q=test", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    let _ = body; // just verify it returns valid JSON array
}
