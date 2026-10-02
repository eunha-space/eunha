//! Mastodon 4.7's email subscriptions: `Api::V1::Accounts::EmailSubscriptionsController`,
//! the confirmation and unsubscribe pages, `EmailDistributionWorker`, and the
//! admin pages' REST counterparts.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{make_admin, set_setting, TestContext};

const MANAGE_EMAIL_SUBSCRIPTIONS: i64 = 1 << 22;

/// Give `account_id` a role carrying `manage_email_subscriptions`.
async fn grant_permission(ctx: &TestContext, account_id: i64) {
    let role_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO user_roles (id, name, position, permissions, highlighted, created_at, updated_at)
         VALUES ($1, 'Newsletters', 10, $2, false, now(), now()) RETURNING id",
    )
    .bind(eunha::snowflake::next_id())
    .bind(MANAGE_EMAIL_SUBSCRIPTIONS)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query("UPDATE users SET role_id = $1 WHERE account_id = $2")
        .bind(role_id)
        .bind(account_id)
        .execute(&ctx.db)
        .await
        .unwrap();
}

/// Turn the user setting `email_subscriptions` on or off.
async fn set_user_setting(ctx: &TestContext, account_id: i64, on: bool) {
    sqlx::query(
        "UPDATE users SET settings = jsonb_set(COALESCE(NULLIF(settings, '')::jsonb, '{}'),
                                               '{email_subscriptions}', to_jsonb($1::boolean))::text
         WHERE account_id = $2",
    )
    .bind(on)
    .bind(account_id)
    .execute(&ctx.db)
    .await
    .unwrap();
}

/// Everything alice needs to be subscribed to.
async fn offer(ctx: &TestContext) -> i64 {
    let alice: i64 = ctx.alice_id.parse().unwrap();
    set_setting(&ctx.db, "email_subscriptions", "true").await;
    grant_permission(ctx, alice).await;
    set_user_setting(ctx, alice, true).await;
    alice
}

async fn subscribe(ctx: &TestContext, account_id: i64, email: &str) -> reqwest::Response {
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{account_id}/email_subscriptions"),
            None,
            &json!({ "email": email }),
        )
        .await
}

async fn token_for(ctx: &TestContext, email: &str) -> String {
    sqlx::query_scalar("SELECT confirmation_token FROM email_subscriptions WHERE email = $1")
        .bind(email)
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

async fn form_post(ctx: &TestContext, path: &str, body: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(ctx.api.url(path))
        .header("host", &ctx.domain)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body.to_string())
        .send()
        .await
        .unwrap()
}

/// `require_feature_enabled!` and `require_account_permissions!` answer an
/// empty 404 until the setting, the role and the user setting all allow it.
#[tokio::test]
async fn test_subscribing_needs_the_feature_the_role_and_the_user() {
    let ctx = TestContext::new("emailsub-gates").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();

    let resp = subscribe(&ctx, alice, "reader@example.com").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "setting off");
    assert_eq!(resp.text().await.unwrap(), "");

    set_setting(&ctx.db, "email_subscriptions", "true").await;
    let resp = subscribe(&ctx, alice, "reader@example.com").await;
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "role lacks permission"
    );

    grant_permission(&ctx, alice).await;
    let resp = subscribe(&ctx, alice, "reader@example.com").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "user setting off");

    set_user_setting(&ctx, alice, true).await;
    let resp = subscribe(&ctx, alice, "reader@example.com").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.json::<Value>().await.unwrap(), json!({}));
    let locale: String = sqlx::query_scalar(
        "SELECT locale FROM email_subscriptions WHERE email = 'reader@example.com'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(locale, "en");

    // `@account.unavailable?`
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(alice)
        .execute(&ctx.db)
        .await
        .unwrap();
    let resp = subscribe(&ctx, alice, "other@example.com").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "suspended account");

    // `Account.local.find`: no such account is a JSON 404.
    let resp = subscribe(&ctx, 1, "other@example.com").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["error"],
        "Record not found"
    );
}

/// `DISABLE_EMAIL_SUBSCRIPTIONS=true`: nothing can turn the feature on.
#[tokio::test]
async fn test_configuration_turns_the_feature_off() {
    let ctx = TestContext::with_config("emailsub-config", |c| {
        c.instance.email_subscriptions = false;
    })
    .await;
    let alice = offer(&ctx).await;
    let resp = subscribe(&ctx, alice, "reader@example.com").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let bob: i64 = ctx.bob_id.parse().unwrap();
    make_admin(&ctx.db, bob).await;
    let overview: Value = ctx
        .api
        .get("/api/v1/admin/email_subscriptions", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(overview["available"], false);
    let resp = ctx
        .api
        .post_json(
            "/api/v1/admin/email_subscriptions/setup",
            Some(&ctx.bob_token),
            &json!({"agreement_email_volume": true, "agreement_privacy_and_terms": true}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let account: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{alice}"), None)
        .await
        .json()
        .await
        .unwrap();
    assert!(account.get("email_subscriptions").is_none());
}

/// The model's validations, rendered by `ValidationErrorFormatter`.
#[tokio::test]
async fn test_validation_errors() {
    let ctx = TestContext::new("emailsub-invalid").await;
    let alice = offer(&ctx).await;

    let resp = subscribe(&ctx, alice, "").await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        json!({
            "error": "Validation failed: Email can't be blank, Email is invalid",
            "details": {"email": [
                {"error": "ERR_BLANK", "description": "can't be blank"},
                {"error": "ERR_INVALID", "description": "is invalid"},
            ]},
        })
    );

    let body: Value = subscribe(&ctx, alice, "not an address")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["details"]["email"][0]["error"], "ERR_INVALID");

    // Normalized before the uniqueness check: squished and downcased.
    assert_eq!(
        subscribe(&ctx, alice, "Reader@Example.com").await.status(),
        StatusCode::OK
    );
    let resp = subscribe(&ctx, alice, "  reader@example.COM ").await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Validation failed: Email has already been taken"
    );
    assert_eq!(body["details"]["email"][0]["error"], "ERR_TAKEN");

    let long = format!("{}@example.com", "a".repeat(320));
    let body: Value = subscribe(&ctx, alice, &long).await.json().await.unwrap();
    assert_eq!(body["details"]["email"][0]["error"], "ERR_TOO_LONG");
}

/// The link in the confirmation email confirms; the one to unsubscribe asks,
/// then destroys the subscription, by form or by one-click POST.
#[tokio::test]
async fn test_confirmation_and_unsubscribing() {
    let ctx = TestContext::new("emailsub-confirm").await;
    let alice = offer(&ctx).await;
    subscribe(&ctx, alice, "reader@example.com").await;
    let token = token_for(&ctx, "reader@example.com").await;
    assert_eq!(token.len(), 20);

    let resp = ctx
        .api
        .get(
            &format!("/email_subscriptions/confirmation?confirmation_token={token}"),
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let page = resp.text().await.unwrap();
    assert!(page.contains("You&#x27;re signed up") || page.contains("You're signed up"));
    assert!(page.contains(&format!("/unsubscribe?token={token}")));
    let confirmed: bool = sqlx::query_scalar(
        "SELECT confirmed_at IS NOT NULL FROM email_subscriptions WHERE email = 'reader@example.com'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(confirmed);

    let resp = ctx
        .api
        .get(
            "/email_subscriptions/confirmation?confirmation_token=nope",
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = ctx
        .api
        .get(&format!("/unsubscribe?token={token}"), None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp
        .text()
        .await
        .unwrap()
        .contains("Unsubscribe from alice?"));

    let resp = form_post(&ctx, "/unsubscribe", &format!("token={token}")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.text().await.unwrap().contains("You are unsubscribed"));
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM email_subscriptions")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(left, 0);
    let resp = form_post(&ctx, "/unsubscribe", &format!("token={token}")).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // `List-Unsubscribe-Post: List-Unsubscribe=One-Click`
    subscribe(&ctx, alice, "other@example.com").await;
    let token = token_for(&ctx, "other@example.com").await;
    let resp = form_post(
        &ctx,
        &format!("/unsubscribe?token={token}"),
        "List-Unsubscribe=One-Click",
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM email_subscriptions")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// With the Mastodon's `secret_key_base`, links carry Mastodon's signed
/// GlobalID of the subscription, a GlobalID Mastodon signed unsubscribes, and
/// a link eunha mailed before the secret was configured still works.
#[tokio::test]
async fn test_unsubscribing_with_a_signed_global_id() {
    let ctx = TestContext::with_instance_config("emailsub-sgid", |instance| {
        instance.secret_key_base = Some(eunha::secret_key_base::SecretKeyBase::new(
            "0123456789abcdef".repeat(8),
        ));
    })
    .await;
    let secret = ctx.state.instance.secret_key_base.clone().unwrap();
    let alice = offer(&ctx).await;
    subscribe(&ctx, alice, "reader@example.com").await;
    let token = token_for(&ctx, "reader@example.com").await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM email_subscriptions WHERE email = $1")
        .bind("reader@example.com")
        .fetch_one(&ctx.db)
        .await
        .unwrap();

    let page = ctx
        .api
        .get(
            &format!("/email_subscriptions/confirmation?confirmation_token={token}"),
            None,
        )
        .await
        .text()
        .await
        .unwrap();
    let (_, rest) = page.split_once("/unsubscribe?token=").unwrap();
    let linked = urlencoding::decode(&rest[..rest.find('"').unwrap()])
        .unwrap()
        .into_owned();
    assert_eq!(
        secret.locate_signed(&linked, "unsubscribe", chrono::Utc::now()),
        Some(("EmailSubscription".to_owned(), id))
    );

    // `@subscription.to_sgid(for: 'unsubscribe')`, as Mastodon mails it.
    let sgid = secret.signed_global_id("EmailSubscription", id, "unsubscribe", chrono::Utc::now());
    let query = format!("token={}", urlencoding::encode(&sgid));
    let resp = ctx.api.get(&format!("/unsubscribe?{query}"), None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp
        .text()
        .await
        .unwrap()
        .contains("Unsubscribe from alice?"));
    // One made for another purpose, or that has expired, names nothing.
    let other = secret.signed_global_id("EmailSubscription", id, "default", chrono::Utc::now());
    let stale = secret.signed_global_id(
        "EmailSubscription",
        id,
        "unsubscribe",
        chrono::Utc::now() - chrono::Duration::days(40),
    );
    for bad in [other, stale] {
        let resp = ctx
            .api
            .get(
                &format!("/unsubscribe?token={}", urlencoding::encode(&bad)),
                None,
            )
            .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
    let resp = form_post(&ctx, "/unsubscribe", &query).await;
    assert!(resp.text().await.unwrap().contains("You are unsubscribed"));
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM email_subscriptions")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(left, 0);

    // The confirmation token, which eunha's links carried before.
    subscribe(&ctx, alice, "other@example.com").await;
    let token = token_for(&ctx, "other@example.com").await;
    let resp = form_post(&ctx, "/unsubscribe", &format!("token={token}")).await;
    assert!(resp.text().await.unwrap().contains("You are unsubscribed"));
}

/// Posting batches public posts that are not replies to others, and the
/// worker mails the batch to confirmed subscribers only.
#[tokio::test]
async fn test_distribution_batches_public_posts_for_confirmed_subscribers() {
    let ctx = TestContext::new("emailsub-distribute").await;
    let alice = offer(&ctx).await;
    subscribe(&ctx, alice, "confirmed@example.com").await;
    subscribe(&ctx, alice, "pending@example.com").await;
    sqlx::query(
        "UPDATE email_subscriptions SET confirmed_at = now() WHERE email = 'confirmed@example.com'",
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let public = ctx
        .api
        .post_status(&ctx.alice_token, "First post", "public")
        .await;
    let public_id: i64 = public["id"].as_str().unwrap().parse().unwrap();
    ctx.api
        .post_status(&ctx.alice_token, "Unlisted post", "unlisted")
        .await;
    ctx.api
        .post_status(&ctx.alice_token, "Followers post", "private")
        .await;
    let bobs = ctx
        .api
        .post_status(&ctx.bob_token, "Bob's post", "public")
        .await;
    let reply_to_bob: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "@bob hi", "visibility": "public", "in_reply_to_id": bobs["id"]}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(reply_to_bob["id"].is_string());
    let self_reply: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "Thread", "visibility": "public", "in_reply_to_id": public["id"]}),
        )
        .await
        .json()
        .await
        .unwrap();
    let self_reply_id: i64 = self_reply["id"].as_str().unwrap().parse().unwrap();

    let state = &ctx.state;
    assert_eq!(
        eunha::email_subscriptions::pending_batch(state, alice).await,
        vec![public_id, self_reply_id]
    );
    // Bob offers nothing, so nothing of his is batched.
    let bob: i64 = ctx.bob_id.parse().unwrap();
    assert!(eunha::email_subscriptions::pending_batch(state, bob)
        .await
        .is_empty());

    // The worker filters again: a boost or a non-public post that found its
    // way into the set is not mailed.
    let unlisted: i64 =
        sqlx::query_scalar("SELECT id FROM statuses WHERE account_id = $1 AND visibility = 1")
            .bind(alice)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    let mut redis = state.redis_coordination.clone();
    let _: () = redis::cmd("SADD")
        .arg(
            state
                .redis_keys
                .key(format!("email_subscriptions:{alice}:next_batch")),
        )
        .arg(unlisted)
        .query_async(&mut redis)
        .await
        .unwrap();

    let sent = eunha::email_subscriptions::distribute(state, alice)
        .await
        .unwrap();
    assert_eq!(sent.statuses, vec![self_reply_id, public_id]);
    assert_eq!(sent.recipients, vec!["confirmed@example.com".to_string()]);
    assert!(eunha::email_subscriptions::pending_batch(state, alice)
        .await
        .is_empty());
    // Nothing left: the next run sends nothing.
    assert_eq!(
        eunha::email_subscriptions::distribute(state, alice)
            .await
            .unwrap(),
        Default::default()
    );

    // Turned off by the user, posts are no longer batched.
    set_user_setting(&ctx, alice, false).await;
    ctx.api
        .post_status(&ctx.alice_token, "Quiet", "public")
        .await;
    assert!(eunha::email_subscriptions::pending_batch(state, alice)
        .await
        .is_empty());
}

/// `REST::AccountSerializer#email_subscriptions`, present only while the
/// feature is enabled.
#[tokio::test]
async fn test_account_serializer_says_who_can_be_subscribed_to() {
    let ctx = TestContext::new("emailsub-serializer").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let get = |id: String| {
        let ctx = &ctx;
        async move {
            ctx.api
                .get(&format!("/api/v1/accounts/{id}"), None)
                .await
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    assert!(get(ctx.alice_id.clone())
        .await
        .get("email_subscriptions")
        .is_none());
    offer(&ctx).await;
    assert_eq!(get(ctx.alice_id.clone()).await["email_subscriptions"], true);
    assert_eq!(get(ctx.bob_id.clone()).await["email_subscriptions"], false);
    let me: Value = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["email_subscriptions"], true);
    let batch: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts?id[]={alice}&id[]={}", ctx.bob_id),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    let flags: Vec<&Value> = batch.iter().map(|a| &a["email_subscriptions"]).collect();
    assert!(flags.contains(&&json!(true)) && flags.contains(&&json!(false)));
}

/// The account's own switch, which Mastodon keeps on its privacy settings
/// page.
#[tokio::test]
async fn test_own_switch() {
    let ctx = TestContext::new("emailsub-own").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let own: Value = ctx
        .api
        .get("/api/eunha/v1/email_subscriptions", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        own,
        json!({"available": false, "enabled": false, "subscribers": 0})
    );
    let resp = ctx
        .api
        .put_json(
            "/api/eunha/v1/email_subscriptions",
            Some(&ctx.alice_token),
            &json!({"enabled": true}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    set_setting(&ctx.db, "email_subscriptions", "true").await;
    grant_permission(&ctx, alice).await;
    let own: Value = ctx
        .api
        .put_json(
            "/api/eunha/v1/email_subscriptions",
            Some(&ctx.alice_token),
            &json!({"enabled": true}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        own,
        json!({"available": true, "enabled": true, "subscribers": 0})
    );
    assert_eq!(
        subscribe(&ctx, alice, "reader@example.com").await.status(),
        StatusCode::OK
    );
}

/// The admin pages' REST counterparts, all behind `manage_settings`.
#[tokio::test]
async fn test_admin_pages() {
    let ctx = TestContext::new("emailsub-admin").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    make_admin(&ctx.db, bob).await;
    let admin = Some(ctx.bob_token.as_str());

    let resp = ctx
        .api
        .get("/api/v1/admin/email_subscriptions", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let overview: Value = ctx
        .api
        .get("/api/v1/admin/email_subscriptions", admin)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(overview["available"], true);
    assert_eq!(overview["enabled"], false);
    // `manage_email_subscriptions | administrator`: the seeded roles that
    // carry `administrator`, and bob's.
    assert!(overview["roles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == "Admin" && r["accounts"] == 1));

    // `Form::EmailSubscriptionsConfirmation`: both agreements.
    let resp = ctx
        .api
        .post_json(
            "/api/v1/admin/email_subscriptions/setup",
            admin,
            &json!({"agreement_email_volume": true}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Validation failed: Agreement privacy and terms must be accepted"
    );
    let overview: Value = ctx
        .api
        .post_json(
            "/api/v1/admin/email_subscriptions/setup",
            admin,
            &json!({"agreement_email_volume": "1", "agreement_privacy_and_terms": "1"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(overview["enabled"], true);

    // The user setting, from the account page.
    grant_permission(&ctx, alice).await;
    let entry: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/email_subscriptions/accounts/{alice}/enable"),
            admin,
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(entry["status"], "active");
    subscribe(&ctx, alice, "one@example.com").await;
    subscribe(&ctx, alice, "two@example.com").await;

    let overview: Value = ctx
        .api
        .get("/api/v1/admin/email_subscriptions", admin)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(overview["accounts"].as_array().unwrap().len(), 1);
    assert_eq!(overview["accounts"][0]["account"]["id"], ctx.alice_id);
    assert_eq!(overview["accounts"][0]["subscribers"], 2);

    let subscribers: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/admin/email_subscriptions/accounts/{alice}/subscriptions"),
            admin,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(subscribers.len(), 2);
    let resp = ctx
        .api
        .delete(
            &format!(
                "/api/v1/admin/email_subscriptions/{}",
                subscribers[0]["id"].as_str().unwrap()
            ),
            &ctx.bob_token,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let entry: Value = ctx
        .api
        .get(
            &format!("/api/v1/admin/email_subscriptions/accounts/{alice}"),
            admin,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(entry["subscribers"], 1);

    let entry: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/email_subscriptions/accounts/{alice}/disable"),
            admin,
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(entry["status"], "disabled");

    let overview: Value = ctx
        .api
        .put_json(
            "/api/v1/admin/email_subscriptions/additional_footer_text",
            admin,
            &json!({"email_footer_text": "Sent by Example Ltd."}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(overview["email_footer_text"], "Sent by Example Ltd.");

    let overview: Value = ctx
        .api
        .post_json("/api/v1/admin/email_subscriptions/purge", admin, &json!({}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(overview["accounts"], json!([]));

    let overview: Value = ctx
        .api
        .post_json(
            "/api/v1/admin/email_subscriptions/disable",
            admin,
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(overview["enabled"], false);
}

/// `clean_unconfirmed_email_subscriptions!`: a week unconfirmed and it goes.
#[tokio::test]
async fn test_unconfirmed_subscriptions_are_cleaned_up() {
    let ctx = TestContext::new("emailsub-cleanup").await;
    let alice = offer(&ctx).await;
    for email in [
        "old@example.com",
        "confirmed@example.com",
        "new@example.com",
    ] {
        subscribe(&ctx, alice, email).await;
    }
    sqlx::query(
        "UPDATE email_subscriptions SET created_at = now() - interval '8 days'
         WHERE email IN ('old@example.com', 'confirmed@example.com')",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE email_subscriptions SET confirmed_at = now() WHERE email = 'confirmed@example.com'",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let removed = eunha::email_subscriptions::clean_unconfirmed(&ctx.state)
        .await
        .unwrap();
    assert_eq!(removed, 1);
    let left: Vec<String> =
        sqlx::query_scalar("SELECT email FROM email_subscriptions ORDER BY email")
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    assert_eq!(left, vec!["confirmed@example.com", "new@example.com"]);
}

/// Accounts embedded in statuses and notifications carry
/// `email_subscriptions` too, as Mastodon's serializer gives every account.
#[tokio::test]
async fn test_embedded_accounts_say_whether_they_offer_subscriptions() {
    let ctx = TestContext::new("email-subs-embedded").await;
    offer(&ctx).await;
    let post = ctx
        .api
        .post_status(&ctx.alice_token, "for subscribers", "public")
        .await;
    assert_eq!(post["account"]["email_subscriptions"], true);
    let bob_post = ctx
        .api
        .post_status(&ctx.bob_token, "not offered", "public")
        .await;
    assert_eq!(bob_post["account"]["email_subscriptions"], false);

    // Bob favourites alice's post; alice's notification names bob.
    ctx.api
        .post_json(
            &format!(
                "/api/v1/statuses/{}/favourite",
                post["id"].as_str().unwrap()
            ),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    let notifications: Vec<serde_json::Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let favourite = notifications
        .iter()
        .find(|n| n["type"] == "favourite")
        .expect("a favourite notification");
    assert_eq!(favourite["account"]["email_subscriptions"], false);
    assert_eq!(favourite["status"]["account"]["email_subscriptions"], true);
}
