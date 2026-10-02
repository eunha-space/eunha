//! `Api::BaseController`'s `require_not_suspended!` and `require_user!`: a
//! token stays valid whatever becomes of its user, and what the user may do
//! with it is decided per endpoint.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn error(response: reqwest::Response) -> (StatusCode, String) {
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    (
        status,
        body["error"].as_str().unwrap_or_default().to_owned(),
    )
}

async fn post_status(ctx: &TestContext, token: &str) -> (StatusCode, String) {
    let response = ctx
        .api
        .post_json("/api/v1/statuses", Some(token), &json!({"status": "hello"}))
        .await;
    if response.status() == StatusCode::OK {
        return (StatusCode::OK, String::new());
    }
    error(response).await
}

async fn set_user(ctx: &TestContext, sql: &str) {
    sqlx::query(&format!("UPDATE users SET {sql} WHERE account_id = $1"))
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
}

/// Each way a user can fall short of `functional?` gets Mastodon's refusal
/// where `require_user!` runs, and the token still reads what needs none.
#[tokio::test]
async fn test_require_user_refuses_users_short_of_functional() {
    let ctx = TestContext::new("gate-standing").await;
    let token = ctx.bob_token.clone();

    set_user(&ctx, "confirmed_at = NULL").await;
    assert_eq!(
        post_status(&ctx, &token).await,
        (
            StatusCode::FORBIDDEN,
            "Your login is missing a confirmed e-mail address".into()
        )
    );
    set_user(&ctx, "confirmed_at = now(), approved = false").await;
    assert_eq!(
        post_status(&ctx, &token).await,
        (
            StatusCode::FORBIDDEN,
            "Your login is currently pending approval".into()
        )
    );
    set_user(&ctx, "approved = true, disabled = true").await;
    assert_eq!(
        post_status(&ctx, &token).await,
        (
            StatusCode::FORBIDDEN,
            "Your login is currently disabled".into()
        )
    );
    let verify = ctx
        .api
        .get("/api/v1/accounts/verify_credentials", Some(&token))
        .await;
    assert_eq!(verify.status(), StatusCode::FORBIDDEN);
    // `GET /api/v1/statuses/:id` runs no `require_user!`.
    let alice_post = ctx
        .api
        .post_status(&ctx.alice_token, "visible", "public")
        .await;
    let shown = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{}", alice_post["id"].as_str().unwrap()),
            Some(&token),
        )
        .await;
    assert_eq!(shown.status(), StatusCode::OK);

    // A moved account is not functional either, but may still undo its move.
    set_user(&ctx, "disabled = false").await;
    sqlx::query("UPDATE accounts SET moved_to_account_id = $1 WHERE id = $2")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    assert_eq!(
        post_status(&ctx, &token).await,
        (
            StatusCode::FORBIDDEN,
            "Your login is currently disabled".into()
        )
    );
    let undo = ctx.api.delete("/api/v1/accounts/redirect", &token).await;
    assert_ne!(undo.status(), StatusCode::FORBIDDEN);
}

/// A suspended account's token is refused on every API endpoint, including
/// ones that need no token at all.
#[tokio::test]
async fn test_suspended_accounts_tokens_are_refused_everywhere() {
    let ctx = TestContext::new("gate-suspended").await;
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    for path in ["/api/v1/instance", "/api/v1/timelines/public"] {
        let (status, message) = error(ctx.api.get(path, Some(&ctx.bob_token)).await).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(message, "Your login is currently disabled");
    }
    // Without the token the same endpoints answer.
    let anonymous = ctx.api.get("/api/v1/instance", None).await;
    assert_eq!(anonymous.status(), StatusCode::OK);
}

/// A client-credentials token has no user: `require_user!` answers 422.
#[tokio::test]
async fn test_app_tokens_cannot_act_as_a_user() {
    let ctx = TestContext::new("gate-app-token").await;
    let app_id: i64 = sqlx::query_scalar(
        "INSERT INTO oauth_applications (name, uid, secret, redirect_uri, scopes)
         VALUES ('app', gen_random_uuid()::text, gen_random_uuid()::text,
                 'urn:ietf:wg:oauth:2.0:oob', 'read write') RETURNING id",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO oauth_access_tokens (application_id, token, scopes, created_at)
         VALUES ($1, 'apptoken-gates', 'read write', now())",
    )
    .bind(app_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    let (status, message) = error(
        ctx.api
            .get("/api/v1/timelines/home", Some("apptoken-gates"))
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(message, "This method requires an authenticated user");
}
