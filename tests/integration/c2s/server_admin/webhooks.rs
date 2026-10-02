//! `Admin::WebhooksController`.

use super::*;

const MANAGE_WEBHOOKS: i64 = 1 << 15;
const MANAGE_REPORTS: i64 = 1 << 4;

/// Webhooks: `manage_webhooks`, validated as `Webhook` is, events limited to
/// what the moderator may see, secrets made and rotated, and none of it
/// logged.
#[tokio::test]
async fn test_webhooks() {
    let ctx = TestContext::new("srv-webhooks").await;
    give_role(&ctx, &ctx.bob_id, 10, MANAGE_WEBHOOKS | MANAGE_REPORTS).await;
    let post = |path: String, body: Value| {
        let api = &ctx.api;
        let token = ctx.bob_token.clone();
        async move { api.post_json(&path, Some(&token), &body).await }
    };
    let cases = [
        (
            json!({"url": "", "events": []}),
            "Validation failed: Url can't be blank, Url is invalid, Events can't be blank, Events is invalid",
        ),
        (
            json!({"url": "https://hooks.example/a", "events": ["report.created", "status.exploded"]}),
            "Validation failed: Events is invalid",
        ),
        (
            json!({"url": "https://hooks.example/a", "events": ["account.created"]}),
            "Validation failed: Events cannot include events you don't have the rights to",
        ),
        (
            json!({"url": "https://hooks.example/a", "events": ["report.created"], "template": "{{ object.id }}"}),
            "Validation failed: Template is invalid",
        ),
    ];
    for (body, message) in cases {
        assert_eq!(
            error_of(
                post("/api/v1/admin/webhooks".into(), body.clone()).await,
                StatusCode::UNPROCESSABLE_ENTITY
            )
            .await,
            message,
            "for {body}"
        );
    }

    let hook = json_ok(
        post(
            "/api/v1/admin/webhooks".into(),
            json!({
                "url": "https://hooks.example/a",
                "events": [" report.created ", ""],
                "template": "{\"text\": \"Report {{object.id}}\"}",
            }),
        )
        .await,
    )
    .await;
    assert_eq!(hook["events"], json!(["report.created"]));
    assert_eq!(hook["enabled"], true);
    let secret = hook["secret"].as_str().unwrap().to_owned();
    assert_eq!(secret.len(), 40);
    assert_eq!(hook["can_update"], true);
    let id = hook["id"].as_str().unwrap();

    let rotated = json_ok(
        post(
            format!("/api/v1/admin/webhooks/{id}/secret/rotate"),
            json!({}),
        )
        .await,
    )
    .await;
    assert_ne!(rotated["secret"], secret.as_str());
    let disabled =
        json_ok(post(format!("/api/v1/admin/webhooks/{id}/disable"), json!({})).await).await;
    assert_eq!(disabled["enabled"], false);
    let edited = json_ok(
        ctx.api
            .patch_json(
                &format!("/api/v1/admin/webhooks/{id}"),
                Some(&ctx.bob_token),
                &json!({"events": ["report.created", "report.updated"]}),
            )
            .await,
    )
    .await;
    assert_eq!(
        edited["events"],
        json!(["report.created", "report.updated"])
    );

    // A hook on account events is out of reach for a role without
    // `manage_users`.
    let account_hook: i64 = sqlx::query_scalar(
        "INSERT INTO webhooks (url, events, secret, created_at, updated_at)
         VALUES ('https://hooks.example/b', '{account.created}', 'abcdefghijklmnop', now(), now())
         RETURNING id",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let list = json_ok(
        ctx.api
            .get("/api/v1/admin/webhooks", Some(&ctx.bob_token))
            .await,
    )
    .await;
    assert_eq!(list[1]["can_update"], false);
    assert_eq!(
        ctx.api
            .delete(
                &format!("/api/v1/admin/webhooks/{account_hook}"),
                &ctx.bob_token
            )
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    json_ok(
        ctx.api
            .delete(&format!("/api/v1/admin/webhooks/{id}"), &ctx.bob_token)
            .await,
    )
    .await;
    assert!(logs(&ctx).await.is_empty());
}
