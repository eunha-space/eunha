//! Signing up as Mastodon does: `AppSignUpService` and
//! `Auth::RegistrationsController` save the account and an unconfirmed user at
//! once, the link mailed to it confirms it, and until then the user's tokens
//! and session reach only what Mastodon lets an unconfirmed user reach.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

fn carol() -> Value {
    json!({
        "username": "carol",
        "email": "carol@example.com",
        "password": "a-long-enough-password",
        "agreement": true,
        "locale": "en",
    })
}

fn details(body: &Value, attribute: &str) -> Vec<String> {
    body["details"][attribute]
        .as_array()
        .map(|errors| {
            errors
                .iter()
                .map(|e| e["error"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// What `User.create!` and `Doorkeeper::AccessToken.create!` write, before
/// anything is confirmed: the account with its key, the user unconfirmed with
/// its token and app, and a token that authenticates as that user.
#[tokio::test]
async fn test_a_sign_up_is_an_unconfirmed_user_at_once() {
    let ctx = TestContext::new("signup-writes-users").await;
    let (app_id, app_token) = ctx.app_token("read write").await;
    let response = ctx
        .api
        .post_json("/api/v1/accounts", Some(&app_token), &carol())
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let token: Value = response.json().await.unwrap();
    assert_eq!(token["token_type"], "Bearer");
    assert_eq!(token["scope"], "read write");
    let access_token = token["access_token"].as_str().unwrap().to_string();

    let (confirmed, confirmation, sent, approved, app, key): (
        bool,
        Option<String>,
        bool,
        bool,
        Option<i64>,
        bool,
    ) = sqlx::query_as(
        "SELECT u.confirmed_at IS NOT NULL, u.confirmation_token,
                u.confirmation_sent_at IS NOT NULL, u.approved, u.created_by_application_id,
                (a.public_key <> '' OR EXISTS (SELECT 1 FROM keypairs k WHERE k.account_id = a.id))
         FROM users u JOIN accounts a ON a.id = u.account_id WHERE a.username = 'carol'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(!confirmed);
    let confirmation = confirmation.expect("a confirmation token");
    assert!(sent);
    assert!(approved, "an open instance approves a sign-up at once");
    assert_eq!(app, Some(app_id));
    assert!(key, "the account has its signing key from the start");
    let owner: Option<i64> = sqlx::query_scalar(
        "SELECT t.resource_owner_id FROM oauth_access_tokens t WHERE t.token = $1",
    )
    .bind(&access_token)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(owner.is_some());

    let mail = ctx
        .mail_to("carol@example.com", "Confirm your email address")
        .await
        .expect("the confirmation mail");
    assert!(mail
        .html
        .contains(&format!("/auth/confirm?token={confirmation}")));

    // `require_user!` for an unconfirmed user.
    let refused = ctx
        .api
        .get("/api/v1/timelines/home", Some(&access_token))
        .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let body: Value = refused.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Your login is missing a confirmed e-mail address"
    );
    let checked: Value = ctx
        .api
        .get("/api/v1/emails/check_confirmation", Some(&access_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(checked, json!(false));

    // The username and the address are taken while the link waits.
    let again = ctx
        .api
        .post_json("/api/v1/accounts", Some(&app_token), &carol())
        .await;
    assert_eq!(again.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = again.json().await.unwrap();
    assert_eq!(details(&body, "username"), ["ERR_TAKEN"]);
    assert_eq!(details(&body, "email"), ["ERR_TAKEN"]);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("E-mail address has already been taken"));

    let confirmed = ctx
        .api
        .get(&format!("/auth/confirm?token={confirmation}"), None)
        .await;
    assert_eq!(confirmed.status(), StatusCode::SEE_OTHER);
    let home = ctx
        .api
        .get("/api/v1/timelines/home", Some(&access_token))
        .await;
    assert_eq!(home.status(), StatusCode::OK);
    let checked: Value = ctx
        .api
        .get("/api/v1/emails/check_confirmation", Some(&access_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(checked, json!(true));
}

/// `doorkeeper_authorize! :write, :'write:accounts'` and
/// `require_client_credentials!`.
#[tokio::test]
async fn test_signing_up_takes_an_apps_own_token() {
    let ctx = TestContext::new("signup-app-token").await;
    let none = ctx.api.post_json("/api/v1/accounts", None, &carol()).await;
    assert_eq!(none.status(), StatusCode::UNAUTHORIZED);

    let as_user = ctx
        .api
        .post_json("/api/v1/accounts", Some(&ctx.alice_token), &carol())
        .await;
    assert_eq!(as_user.status(), StatusCode::FORBIDDEN);
    let body: Value = as_user.json().await.unwrap();
    assert_eq!(
        body["error"],
        "This method requires an client credentials authentication"
    );

    let (_, read_only) = ctx.app_token("read").await;
    let unscoped = ctx
        .api
        .post_json("/api/v1/accounts", Some(&read_only), &carol())
        .await;
    assert_eq!(unscoped.status(), StatusCode::FORBIDDEN);
}

/// An invite's use is counted when the user is saved (`counter_cache`), not
/// when it is confirmed.
#[tokio::test]
async fn test_an_invite_is_used_at_sign_up() {
    let ctx = TestContext::new("signup-invite-uses").await;
    let invite: Value = ctx
        .api
        .post_json("/api/v1/invites", Some(&ctx.bob_token), &json!({}))
        .await
        .json()
        .await
        .unwrap();
    let mut body = carol();
    body["invite_code"] = invite["code"].clone();
    assert_eq!(ctx.sign_up(&body).await.status(), StatusCode::OK);
    let uses: i32 = sqlx::query_scalar("SELECT uses FROM invites WHERE code = $1")
        .bind(invite["code"].as_str().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(uses, 1);
}

/// Devise's `confirm_within`: a link is good for two days.
#[tokio::test]
async fn test_a_link_lasts_two_days() {
    let ctx = TestContext::new("signup-confirm-within").await;
    assert_eq!(ctx.sign_up(&carol()).await.status(), StatusCode::OK);
    let token = ctx.confirmation_token("carol").await;
    sqlx::query(
        "UPDATE users SET confirmation_sent_at = now() - interval '49 hours'
         WHERE email = 'carol@example.com'",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let stale = ctx
        .api
        .get(&format!("/auth/confirm?token={token}"), None)
        .await;
    assert_eq!(
        stale.headers().get("location").unwrap(),
        "/account/login?confirmed=invalid"
    );
    let confirmed: bool = sqlx::query_scalar(
        "SELECT confirmed_at IS NOT NULL FROM users WHERE email = 'carol@example.com'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(!confirmed);
}

/// The link mailed to a user who signed up through an app asks to go back to
/// it (`redirect_to_app`), and following it sends the browser to the app's
/// first redirect URI as it stands, as `after_confirmation_path_for` does.
#[tokio::test]
async fn test_confirming_returns_to_the_app() {
    let ctx = TestContext::new("signup-confirm-redirect").await;
    let app: Value = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({
                "client_name": "Phone",
                "redirect_uris": "https://client.example/cb\nhttps://client.example/other",
                "scopes": "read write",
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let token: Value = ctx
        .api
        .post_json(
            "/oauth/token",
            None,
            &json!({
                "grant_type": "client_credentials",
                "client_id": app["client_id"],
                "client_secret": app["client_secret"],
                "scope": "read write",
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let signed_up = ctx
        .api
        .post_json("/api/v1/accounts", token["access_token"].as_str(), &carol())
        .await;
    assert_eq!(signed_up.status(), StatusCode::OK);
    let confirmation = ctx.confirmation_token("carol").await;
    let mail = ctx
        .mail_to("carol@example.com", "Confirm your email address")
        .await
        .unwrap();
    let link = format!("/auth/confirm?token={confirmation}&redirect_to_app=true");
    assert!(mail.html.contains(&link), "{}", mail.html);

    let confirmed = ctx.api.get(&link, None).await;
    assert_eq!(confirmed.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        confirmed.headers().get("location").unwrap(),
        "https://client.example/cb"
    );
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_access_grants")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(grants, 0, "nothing is granted on confirming");
}

/// The username is kept as it was entered, and is taken whatever the case.
#[tokio::test]
async fn test_the_username_keeps_its_case() {
    let ctx = TestContext::new("signup-username-case").await;
    let mut body = carol();
    body["username"] = json!("Carol_Day");
    assert_eq!(ctx.sign_up(&body).await.status(), StatusCode::OK);
    let username: String = sqlx::query_scalar(
        "SELECT a.username FROM accounts a JOIN users u ON u.account_id = a.id
         WHERE u.email = 'carol@example.com'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(username, "Carol_Day");

    let mut again = carol();
    again["username"] = json!("carol_day");
    again["email"] = json!("other@example.com");
    let refused: Value = ctx.sign_up(&again).await.json().await.unwrap();
    assert_eq!(details(&refused, "username"), ["ERR_TAKEN"]);

    let found = ctx
        .api
        .get(
            &format!(
                "/.well-known/webfinger?resource=acct:carol_day@{}",
                ctx.domain
            ),
            None,
        )
        .await;
    assert_eq!(found.status(), StatusCode::OK);
}

/// Every validation runs, the moderation ones with the rest, as the model's
/// do, so one refusal names all that is wrong.
#[tokio::test]
async fn test_a_refusal_names_everything_wrong() {
    let ctx = TestContext::new("signup-all-errors").await;
    sqlx::query(
        "INSERT INTO email_domain_blocks (domain, allow_with_approval, created_at, updated_at)
         VALUES ('blocked.example', false, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO username_blocks (username, normalized_username, exact, allow_with_approval, created_at, updated_at)
         VALUES ('admin', 'admin', false, false, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let refused = ctx
        .sign_up(&json!({
            "username": "admin",
            "email": "someone@blocked.example",
            "password": "short",
        }))
        .await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = refused.json().await.unwrap();
    assert_eq!(details(&body, "password"), ["ERR_TOO_SHORT"]);
    assert_eq!(details(&body, "username"), ["ERR_RESERVED"]);
    assert_eq!(details(&body, "email"), ["ERR_BLOCKED"]);
    assert_eq!(details(&body, "agreement"), ["ERR_ACCEPTED"]);
}

/// The authorization page an app sends a user to signs an unconfirmed user in
/// and sends them on to `auth/setup`, as `require_functional!` does.
#[tokio::test]
async fn test_the_authorization_page_sends_an_unconfirmed_user_to_setup() {
    let ctx = TestContext::new("signup-authorize-setup").await;
    assert_eq!(ctx.sign_up(&carol()).await.status(), StatusCode::OK);
    let app: Value = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({
                "client_name": "Phone",
                "redirect_uris": "https://client.example/cb",
                "scopes": "read",
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let response = ctx
        .api
        .post_form(
            "/oauth/authorize",
            None,
            &[
                ("client_id", app["client_id"].as_str().unwrap()),
                ("redirect_uri", "https://client.example/cb"),
                ("scope", "read"),
                ("email", "carol@example.com"),
                ("password", "a-long-enough-password"),
            ],
        )
        .await;
    assert_eq!(response.headers().get("location").unwrap(), "/auth/setup");
    assert!(session_cookie(&response).is_some(), "signed in");
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_access_grants")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(grants, 0);
}

/// `Api::V1::Emails::ConfirmationsController#create`: for the app the user
/// signed up through, while the address awaits confirmation.
#[tokio::test]
async fn test_the_app_can_resend_the_link_and_correct_the_address() {
    let ctx = TestContext::new("signup-resend").await;
    let (_, app_token) = ctx.app_token("read write").await;
    let token: Value = ctx
        .api
        .post_json("/api/v1/accounts", Some(&app_token), &carol())
        .await
        .json()
        .await
        .unwrap();
    let access_token = token["access_token"].as_str().unwrap().to_string();

    let resent = ctx
        .api
        .post_json(
            "/api/v1/emails/confirmations",
            Some(&access_token),
            &json!({}),
        )
        .await;
    assert_eq!(resent.status(), StatusCode::OK);
    assert_eq!(resent.json::<Value>().await.unwrap(), json!({}));
    // Mail goes through the job queue; give the second a moment.
    let mut sent = 0;
    for _ in 0..50 {
        sent = ctx
            .sent_to("carol@example.com")
            .into_iter()
            .filter(|m| m.subject.contains("Confirm your email"))
            .count();
        if sent == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(sent, 2);

    // A new address waits in `unconfirmed_email`, and the link goes to it.
    let corrected = ctx
        .api
        .post_json(
            "/api/v1/emails/confirmations",
            Some(&access_token),
            &json!({"email": "carol@elsewhere.example"}),
        )
        .await;
    assert_eq!(corrected.status(), StatusCode::OK);
    let unconfirmed: Option<String> =
        sqlx::query_scalar("SELECT unconfirmed_email FROM users WHERE email = 'carol@example.com'")
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(unconfirmed.as_deref(), Some("carol@elsewhere.example"));
    // Devise sends the reconfirmation for the address held back, then the
    // resend: two mails, the same link.
    let mut to_new = Vec::new();
    for _ in 0..50 {
        to_new = ctx.sent_to("carol@elsewhere.example");
        if to_new.len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(to_new.len(), 2);
    assert_eq!(to_new[0].html, to_new[1].html);
    let confirmation = ctx.confirmation_token("carol").await;
    ctx.api
        .get(&format!("/auth/confirm?token={confirmation}"), None)
        .await;
    let email: String = sqlx::query_scalar(
        "SELECT u.email FROM users u JOIN accounts a ON a.id = u.account_id
         WHERE a.username = 'carol'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(email, "carol@elsewhere.example");

    // Confirmed now, with nothing awaiting confirmation.
    let refused = ctx
        .api
        .post_json(
            "/api/v1/emails/confirmations",
            Some(&access_token),
            &json!({}),
        )
        .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let body: Value = refused.json().await.unwrap();
    assert_eq!(
        body["error"],
        "This method is only available while the e-mail is awaiting confirmation"
    );

    // Not the app the user signed up through.
    let other = ctx
        .api
        .post_json(
            "/api/v1/emails/confirmations",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(other.status(), StatusCode::FORBIDDEN);
    let body: Value = other.json().await.unwrap();
    assert_eq!(
        body["error"],
        "This method is only available to the application the user originally signed-up with"
    );
}

fn session_cookie(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("account_session=") && !v.starts_with("account_session=;"))
        .map(|v| v.split(';').next().unwrap().to_string())
}

async fn get_with_cookie(ctx: &TestContext, path: &str, cookie: &str) -> reqwest::Response {
    ctx.api
        .http
        .get(ctx.api.url(path))
        .header("host", &ctx.api.host)
        .header("cookie", cookie)
        .send()
        .await
        .unwrap()
}

/// The web sign-up signs the new user in and sends them to `auth/setup`,
/// where they can put their address right and have the link sent again; an
/// unconfirmed user who signs in later is sent there too.
#[tokio::test]
async fn test_the_web_sign_up_waits_on_the_setup_page() {
    let ctx = TestContext::new("signup-web-setup").await;
    let registered = ctx
        .api
        .post_form(
            "/auth",
            None,
            &[
                ("username", "carol"),
                ("email", "carol@example.com"),
                ("password", "a-long-enough-password"),
                ("agreement", "1"),
            ],
        )
        .await;
    assert_eq!(registered.status(), StatusCode::SEE_OTHER);
    assert_eq!(registered.headers().get("location").unwrap(), "/auth/setup");
    let cookie = session_cookie(&registered).expect("signed in");

    let setup = get_with_cookie(&ctx, "/auth/setup", &cookie).await;
    assert_eq!(setup.status(), StatusCode::OK);
    let page = setup.text().await.unwrap();
    assert!(page.contains("Check your inbox"));
    assert!(page.contains("carol@example.com"));
    let home = get_with_cookie(&ctx, "/account", &cookie).await;
    assert_eq!(home.headers().get("location").unwrap(), "/auth/setup");

    let corrected = ctx
        .api
        .http
        .post(ctx.api.url("/auth/setup"))
        .header("host", &ctx.api.host)
        .header("cookie", &cookie)
        .form(&[("email", "carol@elsewhere.example")])
        .send()
        .await
        .unwrap();
    assert_eq!(
        corrected.headers().get("location").unwrap(),
        "/auth/setup?sent=1"
    );
    assert!(ctx
        .mail_to("carol@elsewhere.example", "Confirm email")
        .await
        .is_some());

    // Signing in again, still unconfirmed.
    let signed_in = ctx
        .api
        .post_form(
            "/account/login",
            None,
            &[
                ("email", "carol@example.com"),
                ("password", "a-long-enough-password"),
            ],
        )
        .await;
    assert_eq!(signed_in.headers().get("location").unwrap(), "/auth/setup");

    let confirmation = ctx.confirmation_token("carol").await;
    ctx.api
        .get(&format!("/auth/confirm?token={confirmation}"), None)
        .await;
    let done = get_with_cookie(&ctx, "/auth/setup", &cookie).await;
    assert_eq!(done.headers().get("location").unwrap(), "/");
}

/// Sign-ups waiting in the table eunha used to keep them in become
/// unconfirmed users when `eunha migrate` runs, and the link already mailed
/// still confirms them.
#[tokio::test]
async fn test_pending_sign_ups_are_converted() {
    let ctx = TestContext::new("signup-convert-pending").await;
    sqlx::query(
        "CREATE TABLE eunha.pending_signups (
             id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
             username TEXT NOT NULL, email TEXT NOT NULL,
             email_normalized TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL,
             invite_id BIGINT, reason TEXT, locale TEXT NOT NULL DEFAULT 'en',
             app_id BIGINT, confirmation_token TEXT NOT NULL UNIQUE,
             expires_at TIMESTAMPTZ NOT NULL DEFAULT now() + interval '24 hours',
             created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
             sign_up_ip inet, time_zone varchar)",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let hash = eunha::crypto::hash_password("a-long-enough-password")
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO eunha.pending_signups
           (username, email, email_normalized, password_hash, confirmation_token, reason, expires_at)
         VALUES ('carol', 'carol@example.com', 'carol@example.com', $1, 'pending-link', 'hi',
                 now() + interval '1 hour'),
                ('late', 'late@example.com', 'late@example.com', $1, 'expired-link', NULL,
                 now() - interval '1 hour'),
                ('alice', 'other@example.com', 'other@example.com', $1, 'taken-link', NULL,
                 now() + interval '1 hour')",
    )
    .bind(&hash)
    .execute(&ctx.db)
    .await
    .unwrap();

    let report = eunha::accounts::convert_pending_signups(
        &ctx.db,
        ctx.state.encryptor.as_ref(),
        &ctx.state.instance,
    )
    .await
    .unwrap();
    assert_eq!(report.converted, 1);
    assert_eq!(report.dropped, 2);
    // `account.created`, queued for the server to deliver.
    let webhooks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM eunha.jobs WHERE kind = 'TriggerWebhookWorker'
           AND args->>'event' = 'account.created'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(webhooks, 1);

    let reason: String = sqlx::query_scalar(
        "SELECT r.text FROM user_invite_requests r JOIN users u ON u.id = r.user_id
         WHERE u.email = 'carol@example.com' AND u.confirmed_at IS NULL",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(reason, "hi");
    let confirmed = ctx.api.get("/auth/confirm?token=pending-link", None).await;
    assert_eq!(
        confirmed.headers().get("location").unwrap(),
        "/account/login?confirmed=1"
    );
    let signed_in = ctx
        .api
        .post_form(
            "/account/login",
            None,
            &[
                ("email", "carol@example.com"),
                ("password", "a-long-enough-password"),
            ],
        )
        .await;
    assert_eq!(signed_in.headers().get("location").unwrap(), "/account");
}
