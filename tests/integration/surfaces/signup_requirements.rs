//! What a sign-up must agree to and show: Mastodon 4.7's `agreement`, the
//! minimum age, the invite request text, and Devise's password length.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{set_setting, TestContext};

async fn sign_up(ctx: &TestContext, body: Value) -> reqwest::Response {
    ctx.sign_up(&body).await
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

#[tokio::test]
async fn test_sign_up_requires_the_agreement_and_a_sound_password() {
    let ctx = TestContext::new("signup-agreement").await;

    let refused = sign_up(
        &ctx,
        json!({ "username": "carol", "email": "carol@example.com", "password": "short" }),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = refused.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Validation failed: Password is too short (minimum is 8 characters), Service agreement must be accepted"
    );
    assert_eq!(details(&body, "agreement"), ["ERR_ACCEPTED"]);
    assert_eq!(details(&body, "password"), ["ERR_TOO_SHORT"]);

    // Devise's upper bound, and an agreement given as a form would give it.
    let long = "x".repeat(73);
    let (_, app) = ctx.app_token("read write").await;
    let refused = ctx
        .api
        .post_form(
            "/api/v1/accounts",
            Some(&app),
            &[
                ("username", "carol"),
                ("email", "carol@example.com"),
                ("password", &long),
                ("agreement", "1"),
            ],
        )
        .await;
    let body: Value = refused.json().await.unwrap();
    assert_eq!(details(&body, "password"), ["ERR_TOO_LONG"]);
    assert!(details(&body, "agreement").is_empty());

    // "yes" is not one of the values `acceptance` takes.
    let refused = sign_up(
        &ctx,
        json!({ "username": "carol", "email": "carol@example.com",
                "password": "a-long-enough-password", "agreement": "yes" }),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let accepted = sign_up(
        &ctx,
        json!({ "username": "carol", "email": "carol@example.com",
                "password": "a-long-enough-password", "agreement": "true" }),
    )
    .await;
    assert_eq!(accepted.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_a_minimum_age_asks_for_a_date_of_birth() {
    let ctx = TestContext::new("signup-min-age").await;
    let instance: Value = ctx
        .api
        .get("/api/v2/instance", None)
        .await
        .json()
        .await
        .unwrap();
    assert!(instance["registrations"]["min_age"].is_null());

    set_setting(&ctx.db, "min_age", "16").await;
    let instance: Value = ctx
        .api
        .get("/api/v2/instance", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(instance["registrations"]["min_age"], 16);

    let page = ctx
        .api
        .get("/auth/signup", None)
        .await
        .text()
        .await
        .unwrap();
    assert!(page.contains("name=\"date_of_birth\""));
    assert!(page.contains("name=\"agreement\""));

    let base = json!({ "username": "dana", "email": "dana@example.com",
                       "password": "a-long-enough-password", "agreement": true });
    let missing: Value = sign_up(&ctx, base.clone()).await.json().await.unwrap();
    assert_eq!(details(&missing, "date_of_birth"), ["ERR_BLANK"]);

    let today = chrono::Utc::now().date_naive();
    let too_young = today
        .checked_sub_months(chrono::Months::new(15 * 12))
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    let mut body = base.clone();
    body["date_of_birth"] = json!(too_young);
    let young: Value = sign_up(&ctx, body).await.json().await.unwrap();
    assert_eq!(details(&young, "date_of_birth"), ["ERR_BELOW_LIMIT"]);
    assert_eq!(
        young["error"],
        "Validation failed: Date of birth is below the age limit"
    );

    // Sixteen years ago today is old enough.
    let exactly = today
        .checked_sub_months(chrono::Months::new(16 * 12))
        .unwrap()
        .format("%Y-%m-%d")
        .to_string();
    let mut body = base.clone();
    body["date_of_birth"] = json!(exactly);
    assert_eq!(sign_up(&ctx, body).await.status(), StatusCode::OK);

    // `User#set_age_verified_at`.
    let token = ctx.confirmation_token("dana").await;
    ctx.api
        .get(&format!("/auth/confirm?token={token}"), None)
        .await;
    let verified: bool = sqlx::query_scalar(
        "SELECT u.age_verified_at IS NOT NULL FROM users u JOIN accounts a ON a.id = u.account_id
         WHERE a.username = 'dana'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(verified);
}

#[tokio::test]
async fn test_the_invite_request_text_can_be_required() {
    let ctx = TestContext::with_approval_required("signup-reason").await;
    let instance: Value = ctx
        .api
        .get("/api/v2/instance", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(instance["registrations"]["reason_required"], false);

    set_setting(&ctx.db, "require_invite_text", "true").await;
    let instance: Value = ctx
        .api
        .get("/api/v2/instance", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(instance["registrations"]["reason_required"], true);

    let base = json!({ "username": "erin", "email": "erin@example.com",
                       "password": "a-long-enough-password", "agreement": true });
    let missing: Value = sign_up(&ctx, base.clone()).await.json().await.unwrap();
    assert_eq!(details(&missing, "reason"), ["ERR_BLANK"]);
    assert_eq!(missing["error"], "Validation failed: Reason can't be blank");

    let mut body = base.clone();
    body["reason"] = json!("x".repeat(421));
    let long: Value = sign_up(&ctx, body).await.json().await.unwrap();
    assert_eq!(details(&long, "reason"), ["ERR_TOO_LONG"]);

    let mut body = base;
    body["reason"] = json!("I would like to join.");
    assert_eq!(sign_up(&ctx, body).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_sign_up_keeps_a_time_zone_rails_knows() {
    let ctx = TestContext::new("signup-time-zone").await;
    for (username, time_zone) in [("carol", "Seoul"), ("dave", "Mars/Olympus_Mons")] {
        let accepted = sign_up(
            &ctx,
            json!({ "username": username, "email": format!("{username}@example.com"),
                    "password": "a-long-enough-password", "agreement": true,
                    "time_zone": time_zone }),
        )
        .await;
        assert_eq!(accepted.status(), StatusCode::OK);
        let token = ctx.confirmation_token(username).await;
        ctx.api
            .get(&format!("/auth/confirm?token={token}"), None)
            .await;
    }
    let zones: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT a.username, u.time_zone FROM users u JOIN accounts a ON a.id = u.account_id
         WHERE a.username IN ('carol', 'dave') ORDER BY a.username",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    // `normalizes :time_zone`: a name `ActiveSupport::TimeZone` cannot find
    // is no time zone at all, not a refusal.
    assert_eq!(
        zones,
        [
            ("carol".into(), Some("Seoul".into())),
            ("dave".into(), None)
        ]
    );
}
