//! Sessions, authorized applications and sign-in history.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{account_session_cookie, user_id_for, TestContext};

const PASSWORD: &str = "testpassword123";

async fn sign_in_as(ctx: &TestContext, user_agent: &str) -> String {
    let resp = ctx
        .api
        .http
        .post(ctx.api.url("/account/login"))
        .header("host", &ctx.api.host)
        .header("user-agent", user_agent)
        .form(&[("email", "alice@test.invalid"), ("password", PASSWORD)])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    resp.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

async fn account_page(ctx: &TestContext, cookie: &str) -> StatusCode {
    ctx.api
        .http
        .get(ctx.api.url("/account"))
        .header("host", &ctx.api.host)
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn test_sessions_are_listed_and_revoked() {
    let ctx = TestContext::new("sessions").await;
    let alice_user = user_id_for(&ctx.db, ctx.alice_id.parse().unwrap()).await;

    let firefox = sign_in_as(
        &ctx,
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.5; rv:128.0) Gecko/20100101 Firefox/128.0",
    )
    .await;
    let chrome = sign_in_as(
        &ctx,
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36",
    )
    .await;
    assert_eq!(account_page(&ctx, &firefox).await, StatusCode::OK);

    let sessions: Vec<Value> = ctx
        .api
        .get("/api/eunha/v1/sessions", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(sessions.len(), 2);
    let descriptions: Vec<&str> = sessions
        .iter()
        .map(|s| s["description"].as_str().unwrap())
        .collect();
    assert!(descriptions.contains(&"Firefox on macOS"));
    assert!(descriptions.contains(&"Chrome on Windows"));
    assert!(sessions.iter().all(|s| s["current"] == false));

    // A session's own token is the current session.
    let firefox_id = sessions.iter().find(|s| s["browser"] == "firefox").unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let session_token: String = sqlx::query_scalar(
        "SELECT t.token FROM session_activations s JOIN oauth_access_tokens t ON t.id = s.access_token_id
         WHERE s.id = $1",
    )
    .bind(firefox_id.parse::<i64>().unwrap())
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let mine: Vec<Value> = ctx
        .api
        .get("/api/eunha/v1/sessions", Some(&session_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(mine
        .iter()
        .any(|s| s["id"] == firefox_id.as_str() && s["current"] == true));

    // Revoking it ends the cookie and deletes its token.
    let revoked = ctx
        .api
        .delete(
            &format!("/api/eunha/v1/sessions/{firefox_id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(revoked.status(), StatusCode::OK);
    assert_eq!(account_page(&ctx, &firefox).await, StatusCode::SEE_OTHER);
    let gone = ctx
        .api
        .get("/api/v1/accounts/verify_credentials", Some(&session_token))
        .await;
    assert_eq!(gone.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(account_page(&ctx, &chrome).await, StatusCode::OK);

    // Someone else's session is not found.
    let bob_cookie = account_session_cookie(&ctx.api, "bob@test.invalid", PASSWORD).await;
    let bob_session: i64 = sqlx::query_scalar(
        "SELECT id FROM session_activations WHERE user_id <> $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(alice_user)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let foreign = ctx
        .api
        .delete(
            &format!("/api/eunha/v1/sessions/{bob_session}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(foreign.status(), StatusCode::NOT_FOUND);
    assert_eq!(account_page(&ctx, &bob_cookie).await, StatusCode::OK);

    // Signing out deactivates the session.
    ctx.api
        .http
        .post(ctx.api.url("/account/logout"))
        .header("host", &ctx.api.host)
        .header("cookie", &chrome)
        .send()
        .await
        .unwrap();
    assert_eq!(account_page(&ctx, &chrome).await, StatusCode::SEE_OTHER);
    let left: i64 =
        sqlx::query_scalar("SELECT count(*) FROM session_activations WHERE user_id = $1")
            .bind(alice_user)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn test_sessions_are_capped_and_a_password_change_ends_the_others() {
    let ctx = TestContext::new("sessions-cap").await;
    let alice_user = user_id_for(&ctx.db, ctx.alice_id.parse().unwrap()).await;

    let mut cookies = Vec::new();
    for _ in 0..11 {
        cookies.push(account_session_cookie(&ctx.api, "alice@test.invalid", PASSWORD).await);
    }
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM session_activations WHERE user_id = $1")
            .bind(alice_user)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(count, 10, "`max_session_activations`");
    assert_eq!(account_page(&ctx, &cookies[0]).await, StatusCode::SEE_OTHER);

    let keep = cookies.last().unwrap().clone();
    let changed = ctx
        .api
        .http
        .post(ctx.api.url("/account/password"))
        .header("host", &ctx.api.host)
        .header("cookie", &keep)
        .form(&[
            ("current_password", PASSWORD),
            ("new_password", "a-new-password"),
            ("new_password_confirm", "a-new-password"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(changed.status(), StatusCode::SEE_OTHER);
    assert_eq!(account_page(&ctx, &keep).await, StatusCode::OK);
    assert_eq!(account_page(&ctx, &cookies[5]).await, StatusCode::SEE_OTHER);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM session_activations WHERE user_id = $1")
            .bind(alice_user)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn test_authorized_applications_are_listed_and_revoked() {
    let ctx = TestContext::new("authorized-apps").await;
    let alice_user = user_id_for(&ctx.db, ctx.alice_id.parse().unwrap()).await;

    // Using a token records when, once a day.
    ctx.api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await;
    let apps: Vec<Value> = ctx
        .api
        .get(
            "/api/eunha/v1/authorized_applications",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0]["name"], "test");
    assert_eq!(apps[0]["scopes"], json!(["read", "write", "follow"]));
    assert!(apps[0]["last_used_at"].is_string());
    let app_id = apps[0]["id"].as_str().unwrap().to_string();

    // A web push subscription goes with the app's tokens.
    let token_id: i64 = sqlx::query_scalar("SELECT id FROM oauth_access_tokens WHERE token = $1")
        .bind(&ctx.alice_token)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO web_push_subscriptions (endpoint, key_p256dh, key_auth, access_token_id, user_id, created_at, updated_at)
         VALUES ('https://push.example/1', 'p', 'a', $1, $2, now(), now())",
    )
    .bind(token_id)
    .bind(alice_user)
    .execute(&ctx.db)
    .await
    .unwrap();

    let revoked = ctx
        .api
        .delete(
            &format!("/api/eunha/v1/authorized_applications/{app_id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(revoked.status(), StatusCode::OK);
    let refused = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    let subscriptions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM web_push_subscriptions WHERE user_id = $1")
            .bind(alice_user)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(subscriptions, 0);
    // Bob's token, from his own app, still works.
    let bob = ctx
        .api
        .get("/api/v1/accounts/verify_credentials", Some(&ctx.bob_token))
        .await;
    assert_eq!(bob.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_login_activities_are_listed() {
    let ctx = TestContext::new("login-activities").await;
    account_session_cookie(&ctx.api, "alice@test.invalid", PASSWORD).await;
    let wrong = ctx
        .api
        .post_form(
            "/account/login",
            None,
            &[("email", "alice@test.invalid"), ("password", "wrong")],
        )
        .await;
    assert_eq!(wrong.status(), StatusCode::OK);

    let activities: Vec<Value> = ctx
        .api
        .get("/api/eunha/v1/login_activities", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(activities.len(), 2);
    assert_eq!(activities[0]["success"], false, "newest first");
    assert_eq!(activities[1]["success"], true);
    assert_eq!(activities[1]["authentication_method"], "password");

    let older: Vec<Value> = ctx
        .api
        .get(
            &format!(
                "/api/eunha/v1/login_activities?max_id={}",
                activities[0]["id"].as_str().unwrap()
            ),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(older.len(), 1);
}

#[tokio::test]
async fn test_oauth_application_timestamp_repair_preserves_known_dates() {
    let ctx = TestContext::new("app-timestamps").await;
    let app_id: i64 =
        sqlx::query_scalar("SELECT application_id FROM oauth_access_tokens WHERE token=$1")
            .bind(&ctx.alice_token)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    sqlx::query("UPDATE oauth_applications SET created_at=NULL,updated_at=NULL WHERE id=$1")
        .bind(app_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../../../migrations/027_oauth_application_timestamps.sql"
    ))
    .execute(&ctx.db)
    .await
    .unwrap();
    let repaired: bool = sqlx::query_scalar("SELECT a.created_at=(SELECT min(t.created_at) FROM oauth_access_tokens t WHERE t.application_id=a.id) AND a.updated_at IS NOT NULL FROM oauth_applications a WHERE a.id=$1")
        .bind(app_id).fetch_one(&ctx.db).await.unwrap();
    assert!(repaired);
    sqlx::query(
        "UPDATE oauth_applications SET created_at='2020-01-01',updated_at='2020-02-01' WHERE id=$1",
    )
    .bind(app_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../../../migrations/027_oauth_application_timestamps.sql"
    ))
    .execute(&ctx.db)
    .await
    .unwrap();
    let preserved: bool = sqlx::query_scalar("SELECT created_at='2020-01-01'::timestamp AND updated_at='2020-02-01'::timestamp FROM oauth_applications WHERE id=$1")
        .bind(app_id).fetch_one(&ctx.db).await.unwrap();
    assert!(preserved);
}
