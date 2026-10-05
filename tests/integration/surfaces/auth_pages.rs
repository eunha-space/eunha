use reqwest::StatusCode;

use crate::helpers::TestContext;

/// The server-rendered account-deletion page (eunha's `/settings/delete`).
#[tokio::test]
async fn test_account_delete_page_and_challenge() {
    let ctx = TestContext::new("acct-delete-page").await;
    let cookie =
        crate::helpers::account_session_cookie(&ctx.api, "alice@test.invalid", "testpassword123")
            .await;
    let alice_account_id: i64 = ctx.alice_id.parse().unwrap();

    // Signed out, the page sends you to the login form.
    let anon = ctx.api.get("/account/delete", None).await;
    assert_eq!(anon.status(), StatusCode::SEE_OTHER);

    let page = ctx
        .api
        .http
        .get(ctx.api.url("/account/delete"))
        .header("host", &ctx.api.host)
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let body = page.text().await.unwrap();
    assert!(
        body.contains("/auth.css"),
        "should link the shared stylesheet"
    );
    assert!(
        body.contains("name=\"password\""),
        "should ask for the password challenge",
    );

    // A failed challenge leaves the account alone.
    let wrong = ctx
        .api
        .http
        .post(ctx.api.url("/account/delete"))
        .header("host", &ctx.api.host)
        .header("cookie", &cookie)
        .header("HX-Request", "true")
        .form(&[("password", "notmypassword")])
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::OK);
    assert!(wrong.text().await.unwrap().contains("error"));
    let still_live: bool =
        sqlx::query_scalar("SELECT suspended_at IS NULL FROM accounts WHERE id = $1")
            .bind(alice_account_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(still_live, "a failed challenge must not delete anything");

    let ok = ctx
        .api
        .http
        .post(ctx.api.url("/account/delete"))
        .header("host", &ctx.api.host)
        .header("cookie", &cookie)
        .header("HX-Request", "true")
        .form(&[("password", "testpassword123")])
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
    assert_eq!(
        ok.headers()
            .get("hx-redirect")
            .and_then(|v| v.to_str().ok()),
        Some("/account/login?deleted=1"),
        "a deleted account is signed out",
    );
    // `Account#mark_deleted!`, as 4.7.0 records a deletion asked for.
    let deleted: bool =
        sqlx::query_scalar("SELECT requested_deletion_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(alice_account_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(deleted, "account should be marked deleted");
    // `sign_out` deactivates the session.
    let sessions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM session_activations s JOIN users u ON u.id = s.user_id
         WHERE u.account_id = $1",
    )
    .bind(alice_account_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(sessions, 0);
}

/// `Settings::DeletesController#show` warns a user who is not yet confirmed
/// and approved differently: nothing they have is lost yet.
#[tokio::test]
async fn test_account_delete_page_for_a_pending_user() {
    let ctx = TestContext::new("acct-delete-pending").await;
    sqlx::query("UPDATE users SET approved = false WHERE email = 'alice@test.invalid'")
        .execute(&ctx.db)
        .await
        .unwrap();
    let cookie =
        crate::helpers::account_session_cookie(&ctx.api, "alice@test.invalid", "testpassword123")
            .await;
    let body = ctx
        .api
        .http
        .get(ctx.api.url("/account/delete"))
        .header("host", &ctx.api.host)
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("Your username will become available again"));
    assert!(!body.contains("Your username will remain unavailable"));
    assert!(body.contains("/privacy-policy"));
}

/// The server-rendered auth pages link the shared SPA-matching stylesheet.
#[tokio::test]
async fn test_auth_pages_use_shared_stylesheet() {
    let ctx = TestContext::new("auth-pages-css").await;

    for path in ["/auth/signup", "/account/login"] {
        let resp = ctx.api.get(path, None).await;
        assert_eq!(resp.status(), StatusCode::OK, "{path} should render");
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("/auth.css"),
            "{path} should link the shared auth stylesheet",
        );
        assert!(
            !body.contains("background:#0f0f0f"),
            "{path} should no longer carry the old hard-coded dark styles",
        );
    }
}

/// Confirming an email lands on the sign-in form rather than a dead-end page,
/// and so does a link that has already been used.
#[tokio::test]
async fn test_email_confirmation_redirects_to_sign_in() {
    let ctx = TestContext::new("auth-confirm-redirect").await;
    // The account switcher may have left another account signed in.
    let cookie =
        crate::helpers::account_session_cookie(&ctx.api, "alice@test.invalid", "testpassword123")
            .await;

    let signup = ctx
        .sign_up(&serde_json::json!({
            "username": "confirmee",
            "email": "confirmee@example.com",
            "password": "a-long-enough-password",
            "agreement": true,
        }))
        .await;
    assert_eq!(signup.status(), StatusCode::OK);
    let token = ctx.confirmation_token("confirmee").await;

    let ok = ctx
        .api
        .http
        .get(ctx.api.url(&format!("/auth/confirm?token={token}")))
        .header("host", &ctx.api.host)
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        ok.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/account/login?confirmed=1"),
    );

    let confirmed: bool = sqlx::query_scalar(
        "SELECT confirmed_at IS NOT NULL FROM users WHERE email = 'confirmee@example.com'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(
        confirmed,
        "the account should have been created and confirmed"
    );

    let page = ctx
        .api
        .http
        .get(ctx.api.url("/account/login?confirmed=1"))
        .header("host", &ctx.api.host)
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let body = page.text().await.unwrap();
    assert!(
        body.contains("Your email is confirmed"),
        "the sign-in page should say the confirmation worked",
    );

    // The same link a second time: used up, but still not a dead end.
    let again = ctx
        .api
        .get(&format!("/auth/confirm?token={token}"), None)
        .await;
    assert_eq!(again.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        again
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok()),
        Some("/account/login?confirmed=invalid"),
    );

    let stale = ctx.api.get("/account/login?confirmed=invalid", None).await;
    let body = stale.text().await.unwrap();
    assert!(
        body.contains("no longer valid"),
        "the sign-in page should explain the stale link",
    );
}
