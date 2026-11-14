//! The sign-up form at `/auth/signup`, the only one: what it says about an
//! invite, what it asks for, the username check it makes as one is typed,
//! and where signing in goes once the address is confirmed.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn bobs_invite(ctx: &TestContext, body: Value) -> String {
    let invite: Value = ctx
        .api
        .post_json("/api/v1/invites", Some(&ctx.bob_token), &body)
        .await
        .json()
        .await
        .unwrap();
    invite["code"].as_str().expect("bob may invite").to_owned()
}

async fn page(ctx: &TestContext, path: &str) -> String {
    let response = ctx.api.get(path, None).await;
    assert_eq!(response.status(), StatusCode::OK, "{path}");
    response.text().await.unwrap()
}

/// The form names the inviter and says what the invite will do; a pasted
/// link is read for its code; an invite no longer good says why and is not
/// sent along.
#[tokio::test]
async fn test_the_form_describes_the_invite() {
    let ctx = TestContext::new("signup-form-invite").await;
    let code = bobs_invite(&ctx, json!({ "autofollow": true })).await;

    let body = page(&ctx, &format!("/auth/signup?invite={code}")).await;
    assert!(body.contains("Invited by @bob."), "{body}");
    assert!(body.contains("You will follow them once you join."));
    assert!(body.contains(&format!(r#"name="invite_code" value="{code}""#)));
    // Open registrations: nothing to review, so no reason asked.
    assert!(!body.contains(r#"name="reason""#));

    let pasted = urlencoding::encode(&format!("https://example.com/invite/{code}")).into_owned();
    let body = page(&ctx, &format!("/auth/signup?invite={pasted}")).await;
    assert!(body.contains("Invited by @bob."), "{body}");

    sqlx::query("UPDATE invites SET expires_at = now() - interval '1 day' WHERE code = $1")
        .bind(&code)
        .execute(&ctx.db)
        .await
        .unwrap();
    let body = page(&ctx, &format!("/auth/signup?invite={code}")).await;
    assert!(body.contains(r#"class="error""#), "{body}");
    assert!(!body.contains("Invited by"));
    assert!(!body.contains(r#"name="invite_code""#));
    // Registrations are open, so the form is still there.
    assert!(body.contains(r#"id="signup-form""#));
}

/// On an instance that reviews sign-ups, the reason is asked for unless the
/// invite skips review, whatever invite is used.
#[tokio::test]
async fn test_the_reason_is_asked_for_unless_the_invite_skips_review() {
    let ctx = TestContext::with_approval_required("signup-form-reason").await;
    crate::helpers::set_setting(&ctx.db, "require_invite_text", "true").await;
    let code = bobs_invite(&ctx, json!({})).await;

    let body = page(&ctx, "/auth/signup").await;
    assert!(body.contains(r#"name="reason""#));
    let body = page(&ctx, &format!("/auth/signup?invite={code}")).await;
    assert!(body.contains(r#"name="reason""#), "{body}");
    assert!(body.contains("An admin will review your application."));
    assert!(body.contains("Apply for an account"));

    // `invite_bypass_approval` on the everyone role.
    sqlx::query("UPDATE user_roles SET permissions = permissions | (1 << 21) WHERE id = -99")
        .execute(&ctx.db)
        .await
        .unwrap();
    let body = page(&ctx, &format!("/auth/signup?invite={code}")).await;
    assert!(!body.contains(r#"name="reason""#), "{body}");
    assert!(!body.contains("An admin will review"));
}

/// The form's fields tell password managers the email is the login, and it
/// loads no script from elsewhere.
#[tokio::test]
async fn test_the_form_itself() {
    let ctx = TestContext::new("signup-form-fields").await;
    let body = page(&ctx, "/auth/signup").await;
    assert!(body.contains(r#"name="email" required autocomplete="username""#));
    assert!(body.contains("/privacy-policy"));
    assert!(!body.contains("unpkg.com"));
    assert!(body.contains(r#"action="/auth""#));
    assert!(body.contains(r#"id="username-handle""#));

    let body = page(&ctx, "/auth/signup?lang=ko").await;
    assert!(body.contains(r#"action="/auth?lang=ko""#));
    assert!(body.contains(r#"<span class="lang-active">KO</span>"#));
}

/// `GET /auth/signup/username` says what signing up would say about the
/// username alone.
#[tokio::test]
async fn test_the_username_is_checked_as_it_is_typed() {
    let ctx = TestContext::new("signup-form-username").await;
    let check = |name: &'static str| {
        let ctx = &ctx;
        async move {
            let response = ctx
                .api
                .get(&format!("/auth/signup/username?username={name}"), None)
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            let body: Value = response.json().await.unwrap();
            body["details"]["username"]
                .as_array()
                .map(|errors| {
                    errors
                        .iter()
                        .map(|e| e["error"].as_str().unwrap().to_owned())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        }
    };
    assert!(check("newcomer").await.is_empty());
    assert_eq!(check("BOB").await, ["ERR_TAKEN"]);
    assert_eq!(check("not-ok").await, ["ERR_INVALID"]);
}

/// Mastodon's invite link, opened in a browser, is the sign-up form with the
/// code.
#[tokio::test]
async fn test_an_invite_link_opens_the_form() {
    let ctx = TestContext::new("signup-form-invite-link").await;
    let code = bobs_invite(&ctx, json!({})).await;
    let response = ctx.api.get(&format!("/invite/{code}"), None).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        response.headers().get("location").unwrap(),
        &format!("/auth/signup?invite={code}"),
    );
}

/// Confirming leaves the browser set to go on into the web app (`/start`)
/// once the person signs in, unless it was already on its way somewhere.
#[tokio::test]
async fn test_signing_in_after_confirming_goes_into_the_app() {
    let ctx = TestContext::new("signup-form-start").await;
    let signup = ctx
        .sign_up(&json!({
            "username": "carol",
            "email": "carol@example.com",
            "password": "a-long-enough-password",
            "agreement": true,
        }))
        .await;
    assert_eq!(signup.status(), StatusCode::OK);
    let token = ctx.confirmation_token("carol").await;

    let confirmed = ctx
        .api
        .get(&format!("/auth/confirm?token={token}"), None)
        .await;
    assert_eq!(confirmed.status(), StatusCode::SEE_OTHER);
    let return_to = confirmed
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("account_return_to="))
        .expect("a place to go on to")
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert_eq!(return_to, "account_return_to=%2Fstart");

    let signed_in = ctx
        .api
        .http
        .post(ctx.api.url("/account/login"))
        .header("host", &ctx.api.host)
        .header("cookie", &return_to)
        .form(&[
            ("email", "carol@example.com"),
            ("password", "a-long-enough-password"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(signed_in.headers().get("location").unwrap(), "/start");

    // An app's authorization page waiting is left to wait.
    let signup = ctx
        .sign_up(&json!({
            "username": "dave",
            "email": "dave@example.com",
            "password": "a-long-enough-password",
            "agreement": true,
        }))
        .await;
    assert_eq!(signup.status(), StatusCode::OK);
    let token = ctx.confirmation_token("dave").await;
    let confirmed = ctx
        .api
        .http
        .get(ctx.api.url(&format!("/auth/confirm?token={token}")))
        .header("host", &ctx.api.host)
        .header(
            "cookie",
            "account_return_to=%2Foauth%2Fauthorize%3Fclient_id%3Dx",
        )
        .send()
        .await
        .unwrap();
    assert!(!confirmed
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| v.starts_with("account_return_to=")));
}
