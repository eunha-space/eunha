//! What Mastodon 5.0 changed on the client side that has no other home here:
//! `prompt=login` and `prompt=create` on the authorization page, postponed
//! confirmation mails, and smaller API additions.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{
    account_session_cookie, grant_admin_scopes, make_admin, session_cookie_of, set_setting,
    TestContext,
};

const PASSWORD: &str = "testpassword123";
const CALLBACK: &str = "https://client.example/cb";

async fn register_app(ctx: &TestContext) -> (String, String) {
    let app: Value = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({
                "client_name": "prompting",
                "redirect_uris": CALLBACK,
                "scopes": "read write",
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    (
        app["client_id"].as_str().unwrap().to_owned(),
        app["client_secret"].as_str().unwrap().to_owned(),
    )
}

fn authorize_path(client_id: &str, prompt: &str) -> String {
    format!(
        "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}&scope=read&state=s1&prompt={prompt}",
        urlencoding::encode(CALLBACK),
    )
}

async fn get(ctx: &TestContext, path: &str, cookies: &str) -> reqwest::Response {
    ctx.api
        .http
        .get(ctx.api.url(path))
        .header("host", &ctx.api.host)
        .header("cookie", cookies)
        .send()
        .await
        .unwrap()
}

/// The cookies a response sets, as a `Cookie` header's pairs, without those it
/// clears.
fn cookies_of(response: &reqwest::Response) -> Vec<String> {
    response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter(|value| !value.contains("Max-Age=0"))
        .filter_map(|value| value.split(';').next().map(str::to_owned))
        .collect()
}

#[tokio::test]
async fn metadata_says_which_prompts_there_are() {
    let ctx = TestContext::new("oauth-prompts").await;
    let body: Value = ctx
        .api
        .get("/.well-known/oauth-authorization-server", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["prompt_values_supported"], json!(["login", "create"]));
}

/// `require_reauth!`: signed in already, the page asks to sign out first, and
/// a session begun since answers it.
#[tokio::test]
async fn prompt_login_asks_for_a_fresh_sign_in() {
    let ctx = TestContext::new("oauth-prompt-login").await;
    let (client_id, _) = register_app(&ctx).await;
    let path = authorize_path(&client_id, "login");
    let session = account_session_cookie(&ctx.api, "alice@test.invalid", PASSWORD).await;

    let asked = get(&ctx, &path, &session).await;
    assert_eq!(asked.status(), StatusCode::OK);
    let noted = cookies_of(&asked);
    assert!(
        noted[0].starts_with("oauth_require_reauth_for="),
        "{noted:?}"
    );
    let html = asked.text().await.unwrap();
    assert!(html.contains("Re-authentication required"), "{html}");
    assert!(html.contains("name=\"continue\""), "{html}");

    // Coming back with the same session changes nothing.
    let again = get(&ctx, &path, &format!("{session}; {}", noted[0])).await;
    assert!(again
        .text()
        .await
        .unwrap()
        .contains("Re-authentication required"));

    // Another session, signed in since, goes on to the consent page, and the
    // answer is good only once.
    let fresh = account_session_cookie(&ctx.api, "alice@test.invalid", PASSWORD).await;
    let answered = get(&ctx, &path, &format!("{fresh}; {}", noted[0])).await;
    assert!(answered.headers().get_all("set-cookie").iter().any(|c| c
        .to_str()
        .unwrap()
        .starts_with("oauth_require_reauth_for=;")));
    let html = answered.text().await.unwrap();
    assert!(!html.contains("Re-authentication required"), "{html}");
    let once_more = get(&ctx, &path, &fresh).await;
    assert!(once_more
        .text()
        .await
        .unwrap()
        .contains("Re-authentication required"));

    // Another authorization's note answers nothing.
    let other = authorize_path(&client_id, "login").replace("state=s1", "state=s2");
    let refused = get(&ctx, &other, &format!("{fresh}; {}", noted[0])).await;
    assert!(refused
        .text()
        .await
        .unwrap()
        .contains("Re-authentication required"));
}

/// `authenticate_resource_owner!`, `after_sign_up_path_for`,
/// `skip_authorization?` and `update_user_created_by_application_id`: the app
/// sends someone to sign up, they come back to it without a consent page, and
/// redeeming the code records the app.
#[tokio::test]
async fn prompt_create_signs_up_and_goes_straight_back() {
    let ctx = TestContext::new("oauth-prompt-create").await;
    let (client_id, client_secret) = register_app(&ctx).await;
    let path = authorize_path(&client_id, "create");

    let to_sign_up = ctx.api.get(&path, None).await;
    assert_eq!(to_sign_up.status(), StatusCode::FOUND);
    let target = to_sign_up.headers()["location"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(target.starts_with("/auth/signup"), "{target}");
    let mut cookies = cookies_of(&to_sign_up);

    let registered = ctx
        .api
        .http
        .post(ctx.api.url("/auth"))
        .header("host", &ctx.api.host)
        .header("cookie", cookies.join("; "))
        .json(&json!({
            "username": "newbie",
            "email": "newbie@test.invalid",
            "password": "a-long-enough-password",
            "agreement": true,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(registered.status(), StatusCode::SEE_OTHER);
    let back = registered.headers()["location"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(back.starts_with("/oauth/authorize?"), "{back}");
    assert!(back.contains("prompt=create"), "{back}");
    cookies.extend(cookies_of(&registered));
    cookies.retain(|c| {
        !c.starts_with("oauth_registration_app") && !c.starts_with("account_return_to")
    });
    assert!(cookies
        .iter()
        .any(|c| c.starts_with("oauth_created_by_app=")));

    // Unconfirmed as it is, the account gets its code.
    let answered = get(&ctx, &back, &cookies.join("; ")).await;
    assert_eq!(answered.status(), StatusCode::FOUND);
    let location = url::Url::parse(answered.headers()["location"].to_str().unwrap()).unwrap();
    assert!(location.as_str().starts_with(CALLBACK), "{location}");
    let code = location
        .query_pairs()
        .find(|(key, _)| key == "code")
        .unwrap()
        .1
        .into_owned();

    let token = ctx
        .api
        .post_json(
            "/oauth/token",
            None,
            &json!({
                "grant_type": "authorization_code",
                "client_id": client_id,
                "client_secret": client_secret,
                "redirect_uri": CALLBACK,
                "code": code,
            }),
        )
        .await;
    assert_eq!(token.status(), StatusCode::OK);
    let recorded: Option<i64> = sqlx::query_scalar(
        r#"SELECT u.created_by_application_id FROM users u
           JOIN accounts a ON a.id = u.account_id WHERE a.username = 'newbie'"#,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let app_id: i64 = sqlx::query_scalar("SELECT id FROM oauth_applications WHERE uid = $1")
        .bind(&client_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(recorded, Some(app_id));
}

/// `User#set_approved` with `postpone_confirmation_emails`, `mails_disabled?`,
/// `Scheduler::PendingAccountsNotificationScheduler` and `User#approve!`.
#[tokio::test]
async fn confirmation_mail_waits_for_approval_when_postponed() {
    let ctx = TestContext::with_approval_required("postponed-mail").await;
    set_setting(&ctx.db, "postpone_confirmation_emails", "--- true\n").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    make_admin(&ctx.db, alice).await;
    grant_admin_scopes(&ctx.db, alice).await;
    let email = "waiting@test.invalid";

    let registered = ctx
        .api
        .http
        .post(ctx.api.url("/auth"))
        .header("host", &ctx.api.host)
        .json(&json!({
            "username": "waiting",
            "email": email,
            "password": "a-long-enough-password",
            "agreement": true,
            "reason": "please let me in",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(registered.status(), StatusCode::SEE_OTHER);
    let session = session_cookie_of(&registered).unwrap();

    let row: (i64, i64, bool, bool, bool) = sqlx::query_as(
        r#"SELECT u.account_id, u.id, u.disabled, u.approved, u.confirmation_sent_at IS NULL
           FROM users u WHERE u.email = $1"#,
    )
    .bind(email)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let (account_id, _, disabled, approved, never_sent) = row;
    assert!(disabled && !approved && never_sent);
    assert!(ctx.sent_to(email).is_empty(), "no mail until approved");

    // The setup page waits rather than asking to check for a link.
    let setup = get(&ctx, "/auth/setup", &session).await;
    assert_eq!(setup.status(), StatusCode::OK);
    assert!(setup
        .text()
        .await
        .unwrap()
        .contains("Keep an eye on your inbox"));

    // Staff hear about it from the scheduler, and only once.
    eunha::accounts::notify_staff_about_postponed_accounts(&ctx.state)
        .await
        .unwrap();
    let subject = format!("New accounts up for review on {}", ctx.domain);
    assert!(ctx.mail_to("alice@test.invalid", &subject).await.is_some());
    let told = |ctx: &TestContext| {
        ctx.sent_to("alice@test.invalid")
            .into_iter()
            .filter(|mail| mail.subject == subject)
            .count()
    };
    assert_eq!(told(&ctx), 1);
    eunha::accounts::notify_staff_about_postponed_accounts(&ctx.state)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(told(&ctx), 1, "the scheduler resumes after whom it told");

    // Approval enables the account and sends the link at last.
    eunha::accounts::approve(&ctx.state, account_id)
        .await
        .unwrap();
    let (disabled, sent): (bool, bool) = sqlx::query_as(
        "SELECT disabled, confirmation_sent_at IS NOT NULL FROM users WHERE email = $1",
    )
    .bind(email)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(!disabled && sent);
    assert!(ctx.mail_to(email, "").await.is_some());
}

#[tokio::test]
async fn warning_presets_have_an_action_category() {
    let ctx = TestContext::new("preset-category").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    make_admin(&ctx.db, alice).await;
    grant_admin_scopes(&ctx.db, alice).await;
    let token = ctx.alice_token.clone();

    let created: Value = ctx
        .api
        .post_json(
            "/api/v1/admin/warning_presets",
            Some(&token),
            &json!({"text": "stop that"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(created["action_category"], "all_actions");

    let id = created["id"].as_str().unwrap();
    let path = format!("/api/v1/admin/warning_presets/{id}");
    let updated: Value = ctx
        .api
        .patch_json(&path, Some(&token), &json!({"action_category": "suspend"}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(updated["action_category"], "suspend");
    let refused = ctx
        .api
        .patch_json(&path, Some(&token), &json!({"action_category": "explode"}))
        .await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// `state` and `verify_credentials?allow_nonfunctional=true`.
#[tokio::test]
async fn verify_credentials_tells_a_nonfunctional_account_why() {
    let ctx = TestContext::new("nonfunctional").await;
    let token = ctx.alice_token.clone();
    let path = "/api/v1/accounts/verify_credentials";

    let functional: Value = ctx.api.get(path, Some(&token)).await.json().await.unwrap();
    assert_eq!(functional["state"]["functional"], true);

    sqlx::query("UPDATE users SET confirmed_at = NULL WHERE account_id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    assert_eq!(
        ctx.api.get(path, Some(&token)).await.status(),
        StatusCode::FORBIDDEN
    );
    let told: Value = ctx
        .api
        .get(&format!("{path}?allow_nonfunctional=true"), Some(&token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(told["state"]["functional"], false);
    assert_eq!(told["state"]["confirmed"], false);
    assert_eq!(told["state"]["error_code"], "pending_confirmation");
}

#[tokio::test]
async fn the_home_timeline_takes_exclusions() {
    let ctx = TestContext::new("home-exclude").await;
    let token = ctx.alice_token.clone();
    let post: Value = ctx
        .api
        .post_status(&token, "an original post", "public")
        .await;
    let id = post["id"].as_str().unwrap();
    let reply: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&token),
            &json!({"status": "a reply", "in_reply_to_id": id}),
        )
        .await
        .json()
        .await
        .unwrap();
    let home = |query: &'static str| {
        let (api, token) = (&ctx.api, token.clone());
        async move {
            let timeline: Vec<Value> = api
                .get(&format!("/api/v1/timelines/home{query}"), Some(&token))
                .await
                .json()
                .await
                .unwrap();
            timeline
                .iter()
                .map(|status| status["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    let everything = home("").await;
    assert!(everything.iter().any(|s| s == id));
    assert!(everything
        .iter()
        .any(|s| s == reply["id"].as_str().unwrap()));
    let without_replies = home("?exclude_replies=true").await;
    assert!(without_replies.iter().any(|s| s == id));
    assert!(!without_replies
        .iter()
        .any(|s| s == reply["id"].as_str().unwrap()));
}

#[tokio::test]
async fn the_sign_up_api_says_it_is_deprecated() {
    let ctx = TestContext::new("signup-deprecated").await;
    let response = ctx
        .sign_up(&json!({
            "username": "apiuser",
            "email": "apiuser@test.invalid",
            "password": "a-long-enough-password",
            "agreement": true,
            "locale": "en",
        }))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["deprecation"], "@1790812800");
}

#[tokio::test]
async fn a_remote_profile_path_is_a_404_for_the_web_app() {
    let ctx = TestContext::new("remote-path").await;
    let response = ctx.api.get("/@nobody@remote.example/whatever", None).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
