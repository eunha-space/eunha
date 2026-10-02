//! Forgotten passwords, as Devise's recoverable module and Mastodon's
//! `Auth::PasswordsController` handle them.

use reqwest::StatusCode;
use sha2::{Digest as _, Sha256};

use crate::helpers::{account_session_cookie, user_id_for, TestContext};

/// Give a user a reset token as if it had been mailed, returning the token.
async fn mail_a_token(ctx: &TestContext, user_id: i64, hours_ago: i32) -> String {
    let token = format!("reset-token-{user_id}-{hours_ago}");
    sqlx::query(
        "UPDATE users SET reset_password_token = $1,
                reset_password_sent_at = now() - make_interval(hours => $2) WHERE id = $3",
    )
    .bind(hex::encode(Sha256::digest(token.as_bytes())))
    .bind(hours_ago)
    .bind(user_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    token
}

async fn html_post(ctx: &TestContext, path: &str, form: &[(&str, &str)]) -> reqwest::Response {
    ctx.api
        .http
        .post(ctx.api.url(path))
        .header("host", &ctx.api.host)
        .header("accept", "text/html")
        .form(form)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn test_asking_for_a_reset_never_says_who_has_an_account() {
    let ctx = TestContext::new("password-reset-ask").await;
    let alice_user = user_id_for(&ctx.db, ctx.alice_id.parse().unwrap()).await;

    let page = ctx.api.get("/auth/password/new", None).await;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.text().await.unwrap().contains("name=\"email\""));
    let login = ctx
        .api
        .get("/account/login", None)
        .await
        .text()
        .await
        .unwrap();
    assert!(login.contains("/auth/password/new"));

    for email in ["alice@test.invalid", "nobody@test.invalid"] {
        let answer = html_post(&ctx, "/auth/password", &[("email", email)]).await;
        assert_eq!(answer.status(), StatusCode::OK);
        assert!(answer
            .text()
            .await
            .unwrap()
            .contains("If your email address exists in our database"));
    }
    // The column holds a digest, never the token that was mailed.
    let stored: String = sqlx::query_scalar("SELECT reset_password_token FROM users WHERE id = $1")
        .bind(alice_user)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(stored.len(), 64);

    // An API client gets a bare 200 either way.
    let api = ctx
        .api
        .post_form("/auth/password", None, &[("email", "nobody@test.invalid")])
        .await;
    assert_eq!(api.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_a_reset_sets_the_password_and_ends_every_session() {
    let ctx = TestContext::new("password-reset").await;
    let alice_user = user_id_for(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    let cookie = account_session_cookie(&ctx.api, "alice@test.invalid", "testpassword123").await;

    // A stale or unknown token goes back to asking.
    let stale = mail_a_token(&ctx, alice_user, 7).await;
    let expired = ctx
        .api
        .get(
            &format!("/auth/password/edit?reset_password_token={stale}"),
            None,
        )
        .await;
    assert_eq!(expired.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        expired.headers()["location"],
        "/auth/password/new?invalid=1"
    );
    let unknown = ctx
        .api
        .get("/auth/password/edit?reset_password_token=nope", None)
        .await;
    assert_eq!(unknown.status(), StatusCode::SEE_OTHER);

    let token = mail_a_token(&ctx, alice_user, 1).await;
    let form = ctx
        .api
        .get(
            &format!("/auth/password/edit?reset_password_token={token}"),
            None,
        )
        .await;
    assert_eq!(form.status(), StatusCode::OK);
    assert!(form.text().await.unwrap().contains(&token));

    let mismatch = html_post(
        &ctx,
        "/auth/password/edit",
        &[
            ("reset_password_token", &token),
            ("password", "brand-new-password"),
            ("password_confirmation", "something-else"),
        ],
    )
    .await;
    assert!(mismatch
        .text()
        .await
        .unwrap()
        .contains("Password confirmation doesn&#x27;t match Password"));
    let short = html_post(
        &ctx,
        "/auth/password/edit",
        &[
            ("reset_password_token", &token),
            ("password", "short"),
            ("password_confirmation", "short"),
        ],
    )
    .await;
    assert!(short.text().await.unwrap().contains("too short"));

    let done = html_post(
        &ctx,
        "/auth/password/edit",
        &[
            ("reset_password_token", &token),
            ("password", "brand-new-password"),
            ("password_confirmation", "brand-new-password"),
        ],
    )
    .await;
    assert!(done
        .text()
        .await
        .unwrap()
        .contains("Your password has been changed successfully."));

    // The new password works, the old one does not.
    account_session_cookie(&ctx.api, "alice@test.invalid", "brand-new-password").await;
    let old = ctx
        .api
        .post_form(
            "/account/login",
            None,
            &[
                ("email", "alice@test.invalid"),
                ("password", "testpassword123"),
            ],
        )
        .await;
    assert_eq!(old.status(), StatusCode::OK);

    // Every session and token from before is gone, and the token is spent.
    let page = ctx
        .api
        .http
        .get(ctx.api.url("/account"))
        .header("host", &ctx.api.host)
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::SEE_OTHER);
    let api = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(api.status(), StatusCode::UNAUTHORIZED);
    let again = html_post(
        &ctx,
        "/auth/password/edit",
        &[
            ("reset_password_token", &token),
            ("password", "another-password"),
            ("password_confirmation", "another-password"),
        ],
    )
    .await;
    assert!(again
        .text()
        .await
        .unwrap()
        .contains("Reset password token is invalid"));

    // Mastodon's own `PUT /auth/password`, as an API client sends it.
    let token = mail_a_token(&ctx, alice_user, 0).await;
    let put = ctx
        .api
        .put_json(
            "/auth/password",
            None,
            &serde_json::json!({
                "reset_password_token": token,
                "password": "a-third-password",
                "password_confirmation": "a-third-password",
            }),
        )
        .await;
    assert_eq!(put.status(), StatusCode::OK);
}
