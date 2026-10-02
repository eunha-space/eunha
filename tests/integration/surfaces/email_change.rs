//! A member changing their own email address, as Devise's reconfirmable does
//! it behind Mastodon's account settings form.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{seed_user, TestContext};

const PASSWORD: &str = "testpassword123";

async fn change(ctx: &TestContext, body: Value) -> reqwest::Response {
    ctx.api
        .put_json("/api/eunha/v1/email", Some(&ctx.alice_token), &body)
        .await
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
async fn test_a_new_address_waits_for_its_link() {
    let ctx = TestContext::new("email-change").await;
    let resp = change(
        &ctx,
        json!({ "email": " Alice.New@Example.com ", "current_password": PASSWORD }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    // Devise strips and downcases it, and keeps the old one until confirmed.
    assert_eq!(body["email"], "alice@test.invalid");
    assert_eq!(body["unconfirmed_email"], "alice.new@example.com");

    let reconfirmation = ctx
        .mail_to("alice.new@example.com", "Mastodon: Confirm email for")
        .await
        .expect("the reconfirmation mail");
    assert!(reconfirmation
        .html
        .contains("Confirm the new address to change your email."));
    let notice = ctx
        .mail_to("alice@test.invalid", "Mastodon: Email changed")
        .await
        .expect("the email changed notice");
    assert!(notice.html.contains("alice.new@example.com"));

    // The link makes the new address the address.
    let token: String =
        sqlx::query_scalar("SELECT confirmation_token FROM users WHERE account_id = $1")
            .bind(ctx.alice_id.parse::<i64>().unwrap())
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    let href = format!("/auth/confirm?token={token}");
    assert!(reconfirmation.html.contains(&href));
    ctx.api.get(&href, None).await;
    let now: Value = ctx
        .api
        .get("/api/eunha/v1/email", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(now["email"], "alice.new@example.com");
    assert!(now["unconfirmed_email"].is_null());
}

#[tokio::test]
async fn test_the_password_and_the_address_are_checked() {
    let ctx = TestContext::new("email-change-refused").await;
    let resp = change(&ctx, json!({ "email": "bob@test.invalid" })).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Validation failed: Email has already been taken, Current password can't be blank"
    );
    assert_eq!(details(&body, "current_password"), ["ERR_BLANK"]);

    let resp = change(
        &ctx,
        json!({ "email": "not an address", "current_password": "wrong-password" }),
    )
    .await;
    let body: Value = resp.json().await.unwrap();
    assert_eq!(details(&body, "email"), ["ERR_INVALID"]);
    assert_eq!(details(&body, "current_password"), ["ERR_INVALID"]);

    // The same address is no change, and mails nothing.
    let resp = change(
        &ctx,
        json!({ "email": "alice@test.invalid", "current_password": PASSWORD }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(body["unconfirmed_email"].is_null());
    assert!(ctx.sent_to("alice@test.invalid").is_empty());
}

#[tokio::test]
async fn test_blocks_apply_to_an_unconfirmed_user() {
    let ctx = TestContext::new("email-change-blocks").await;
    sqlx::query(
        "INSERT INTO email_domain_blocks (domain, allow_with_approval, created_at, updated_at)
         VALUES ('blocked.example', false, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    use sha2::{Digest, Sha256};
    let hash = hex::encode(Sha256::digest(b"carol@canonical.example"));
    sqlx::query(
        "INSERT INTO canonical_email_blocks (canonical_email_hash, created_at, updated_at)
         VALUES ($1, now(), now())",
    )
    .bind(hash)
    .execute(&ctx.db)
    .await
    .unwrap();

    // `UserEmailValidator` passes over a confirmed user's change.
    let resp = change(
        &ctx,
        json!({ "email": "alice@blocked.example", "current_password": PASSWORD }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let (_, carol_token) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    sqlx::query("UPDATE users SET confirmed_at = NULL WHERE email = 'carol@test.invalid'")
        .execute(&ctx.db)
        .await
        .unwrap();
    let blocked = ctx
        .api
        .put_json(
            "/api/eunha/v1/email",
            Some(&carol_token),
            &json!({ "email": "carol@sub.blocked.example", "current_password": PASSWORD }),
        )
        .await;
    assert_eq!(blocked.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = blocked.json().await.unwrap();
    assert_eq!(details(&body, "email"), ["ERR_BLOCKED"]);
    let canonical = ctx
        .api
        .put_json(
            "/api/eunha/v1/email",
            Some(&carol_token),
            &json!({ "email": "c.a.r.o.l+x@canonical.example", "current_password": PASSWORD }),
        )
        .await;
    let body: Value = canonical.json().await.unwrap();
    assert_eq!(details(&body, "email"), ["ERR_TAKEN"]);
}
