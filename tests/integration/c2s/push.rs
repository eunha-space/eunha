use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

fn fake_sub_payload(endpoint: &str) -> serde_json::Value {
    let (_, p256dh) = eunha::push::generate_vapid_keypair().unwrap();
    json!({
        "subscription": {
            "endpoint": endpoint,
            "keys": {
                "p256dh": p256dh,
                "auth": "tBHItJI5svbpez7KI4CCXg=="
            }
        },
        "data": {
            "alerts": {
                "follow": true,
                "favourite": false,
                "reblog": true,
                "mention": true,
                "poll": false,
                "status": false
            },
            "policy": "all"
        }
    })
}

/// Full push subscription lifecycle: create → get → update → delete.
#[tokio::test]
async fn test_push_subscription_lifecycle() {
    let ctx = TestContext::new("push-lifecycle").await;

    // Create subscription.
    let create_resp = ctx
        .api
        .post_json(
            "/api/v1/push/subscription",
            Some(&ctx.alice_token),
            &fake_sub_payload("https://push.example.com/test-endpoint"),
        )
        .await;
    assert_eq!(create_resp.status(), StatusCode::OK);
    let sub: Value = create_resp.json().await.unwrap();
    assert!(sub["id"].as_str().is_some(), "id missing");
    assert_eq!(
        sub["endpoint"].as_str(),
        Some("https://push.example.com/test-endpoint")
    );
    assert_eq!(sub["alerts"]["follow"].as_bool(), Some(true));
    assert_eq!(sub["alerts"]["favourite"].as_bool(), Some(false));
    assert!(sub["server_key"].as_str().is_some(), "server_key missing");

    // GET returns the same subscription.
    let get_resp = ctx
        .api
        .get("/api/v1/push/subscription", Some(&ctx.alice_token))
        .await;
    assert_eq!(get_resp.status(), StatusCode::OK);
    let got: Value = get_resp.json().await.unwrap();
    assert_eq!(got["id"].as_str(), sub["id"].as_str());
    assert_eq!(
        got["endpoint"].as_str(),
        Some("https://push.example.com/test-endpoint")
    );

    // PUT updates alert settings.
    let update_resp = ctx
        .api
        .put_json(
            "/api/v1/push/subscription",
            Some(&ctx.alice_token),
            &json!({
                "data": {
                    "alerts": {"follow": false, "favourite": true},
                    "policy": "followed"
                }
            }),
        )
        .await;
    assert_eq!(update_resp.status(), StatusCode::OK);
    let updated: Value = update_resp.json().await.unwrap();
    // `update!(data: data_params)`: what was given replaces what was stored.
    assert_eq!(
        updated["alerts"],
        json!({"follow": false, "favourite": true})
    );
    assert_eq!(updated["policy"].as_str(), Some("followed"));

    // DELETE removes the subscription.
    let del_resp = ctx
        .api
        .delete("/api/v1/push/subscription", &ctx.alice_token)
        .await;
    assert_eq!(del_resp.status(), StatusCode::OK);

    // GET now returns 404.
    let after_del = ctx
        .api
        .get("/api/v1/push/subscription", Some(&ctx.alice_token))
        .await;
    assert_eq!(after_del.status(), StatusCode::NOT_FOUND);
}

/// `Api::V1::Push::SubscriptionsController`: the data is stored as given,
/// an alert not given is off, and blank data is `{}`, whose policy reads
/// `all`.
#[tokio::test]
async fn push_subscription_data_is_stored_as_given() {
    let ctx = TestContext::new("push-data").await;

    let mut body = fake_sub_payload("https://push.example.com/data");
    body["data"] = json!({"alerts": {"mention": true, "admin.report": true, "bogus": true}});
    let resp = ctx
        .api
        .post_json("/api/v1/push/subscription", Some(&ctx.alice_token), &body)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let sub: Value = resp.json().await.unwrap();
    assert_eq!(
        sub["alerts"],
        json!({"mention": true, "admin.report": true})
    );
    assert_eq!(sub["policy"], "all");
    assert_eq!(sub["standard"], false);
    let stored: Value = sqlx::query_scalar("SELECT data::jsonb FROM web_push_subscriptions")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(
        stored,
        json!({"alerts": {"mention": true, "admin.report": true}})
    );

    // A blank update stores `{}`.
    let resp = ctx
        .api
        .put_json(
            "/api/v1/push/subscription",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    let sub: Value = resp.json().await.unwrap();
    assert_eq!(sub["alerts"], json!({}));
    assert_eq!(sub["policy"], "all");

    // Data with nothing permitted is a missing parameter.
    let resp = ctx
        .api
        .put_json(
            "/api/v1/push/subscription",
            Some(&ctx.alice_token),
            &json!({"data": {"bogus": 1}}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // So is a subscription without one.
    let resp = ctx
        .api
        .post_json(
            "/api/v1/push/subscription",
            Some(&ctx.alice_token),
            &json!({"data": {"policy": "all"}}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// POST /api/v1/push/subscription is idempotent: second POST for the same token replaces the first.
#[tokio::test]
async fn test_push_subscription_idempotent() {
    let ctx = TestContext::new("push-idem").await;

    ctx.api
        .post_json(
            "/api/v1/push/subscription",
            Some(&ctx.alice_token),
            &fake_sub_payload("https://push.example.com/first"),
        )
        .await;

    let second = ctx
        .api
        .post_json(
            "/api/v1/push/subscription",
            Some(&ctx.alice_token),
            &fake_sub_payload("https://push.example.com/second"),
        )
        .await;
    assert_eq!(second.status(), StatusCode::OK);

    // GET should return the second endpoint.
    let got: Value = ctx
        .api
        .get("/api/v1/push/subscription", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        got["endpoint"].as_str(),
        Some("https://push.example.com/second")
    );
}

/// `SubscriptionsController#create` destroys the token's subscription and
/// makes a new one, under `with_redis_lock("push_subscription:<user>")`:
/// held by another, the request is a 503; released, it goes through.
#[tokio::test]
async fn test_push_subscription_create_takes_the_users_lock() {
    let ctx = TestContext::new("push-lock").await;
    let first: Value = ctx
        .api
        .post_json(
            "/api/v1/push/subscription",
            Some(&ctx.alice_token),
            &fake_sub_payload("https://push.example.com/first"),
        )
        .await
        .json()
        .await
        .unwrap();
    let user_id = crate::helpers::user_id_for(&ctx.db, ctx.alice_id.parse::<i64>().unwrap()).await;
    let held = eunha::redis_lock::try_acquire(
        &ctx.state,
        &format!("lock:push_subscription:{user_id}"),
        60_000,
    )
    .await
    .unwrap();
    let busy = ctx
        .api
        .post_json(
            "/api/v1/push/subscription",
            Some(&ctx.alice_token),
            &fake_sub_payload("https://push.example.com/busy"),
        )
        .await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    held.release().await;
    let second: Value = ctx
        .api
        .post_json(
            "/api/v1/push/subscription",
            Some(&ctx.alice_token),
            &fake_sub_payload("https://push.example.com/second"),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_ne!(second["id"], first["id"], "a new subscription");
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM web_push_subscriptions WHERE user_id = $1")
            .bind(user_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

/// GET /api/v1/push/subscription returns 404 when no subscription exists.
#[tokio::test]
async fn test_push_subscription_get_when_none() {
    let ctx = TestContext::new("push-get-none").await;

    let resp = ctx
        .api
        .get("/api/v1/push/subscription", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A push endpoint on the loopback that answers `status` and keeps the
/// headers of what it was sent.
async fn push_endpoint(
    status: std::sync::Arc<std::sync::atomic::AtomicU16>,
) -> (
    String,
    std::sync::Arc<std::sync::Mutex<Vec<axum::http::HeaderMap>>>,
) {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let kept = seen.clone();
    let app = axum::Router::new().route(
        "/push",
        axum::routing::post(move |headers: axum::http::HeaderMap| {
            let kept = kept.clone();
            let status = status.clone();
            async move {
                kept.lock().unwrap().push(headers);
                axum::http::StatusCode::from_u16(status.load(std::sync::atomic::Ordering::SeqCst))
                    .unwrap()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/push"), seen)
}

/// Subscribe alice's token to `endpoint`, with real keys; the subscription id.
async fn subscribe(ctx: &TestContext, endpoint: &str, standard: bool) -> i64 {
    let (_, p256dh) = eunha::push::generate_vapid_keypair().unwrap();
    let resp: Value = ctx
        .api
        .post_json(
            "/api/v1/push/subscription",
            Some(&ctx.alice_token),
            &json!({
                "subscription": {
                    "endpoint": endpoint,
                    "standard": standard,
                    "keys": { "p256dh": p256dh, "auth": "tBHItJI5svbpez7KI4CCXg" },
                },
                "data": { "alerts": { "admin.sign_up": true }, "policy": "all" },
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    resp["id"].as_str().unwrap().parse().unwrap()
}

/// An `admin.sign_up` notification of bob's sign-up, for alice.
async fn sign_up_notification(ctx: &TestContext) -> i64 {
    sqlx::query_scalar(
        r#"INSERT INTO notifications
             (account_id, from_account_id, "type", activity_type, activity_id, created_at, updated_at)
           VALUES ($1, $2, 'admin.sign_up', 'Account', $2, now(), now())
           RETURNING id"#,
    )
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .bind(ctx.bob_id.parse::<i64>().unwrap())
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

async fn push(ctx: &TestContext, id: i64) -> anyhow::Result<()> {
    let notification_id = sign_up_notification(ctx).await;
    push_notification(ctx, id, notification_id).await
}

async fn push_notification(ctx: &TestContext, id: i64, notification_id: i64) -> anyhow::Result<()> {
    use eunha::jobs::Job as _;
    eunha::push::PushNotificationWorker {
        web_push_subscription_id: id,
        notification_id: Some(notification_id),
    }
    .perform(&ctx.state)
    .await
}

async fn subscriptions(ctx: &TestContext) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM web_push_subscriptions")
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

/// `Web::PushNotificationWorker`: the headers Mastodon sends, the encoding the
/// subscription asked for, and the `Unsubscribe-URL` that works.
#[tokio::test]
async fn pushes_carry_mastodons_headers_and_an_unsubscribe_url() {
    let ctx = TestContext::reaching_loopback("push-headers").await;
    let status = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(201));
    let (endpoint, seen) = push_endpoint(status.clone()).await;

    let id = subscribe(&ctx, &endpoint, true).await;
    push(&ctx, id).await.unwrap();
    let headers = seen.lock().unwrap().pop().expect("a push");
    assert_eq!(headers["ttl"], "172800");
    assert_eq!(headers["urgency"], "normal");
    assert_eq!(headers["content-encoding"], "aes128gcm");
    assert!(headers["authorization"]
        .to_str()
        .unwrap()
        .starts_with("vapid t="));
    let unsubscribe = headers["unsubscribe-url"].to_str().unwrap().to_owned();
    let prefix = format!("https://{}/api/web/push_subscriptions/", ctx.domain);
    let path = unsubscribe
        .strip_prefix(&format!("https://{}", ctx.domain))
        .unwrap();
    assert!(unsubscribe.starts_with(&prefix), "{unsubscribe}");

    // `Api::Web::PushSubscriptionsController#destroy`, with no user.
    let resp = ctx
        .api
        .http
        .delete(ctx.api.url(path))
        .header("host", &ctx.api.host)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(subscriptions(&ctx).await, 0);
    // A token for nothing still gets a 200.
    let resp = ctx
        .api
        .http
        .delete(ctx.api.url(path))
        .header("host", &ctx.api.host)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // A legacy subscription is pushed `aesgcm`.
    let id = subscribe(&ctx, &endpoint, false).await;
    push(&ctx, id).await.unwrap();
    let headers = seen.lock().unwrap().pop().expect("a push");
    assert_eq!(headers["content-encoding"], "aesgcm");
    assert_eq!(headers["urgency"], "normal");
    assert!(headers.contains_key("unsubscribe-url"));

    // A server error is retried; a 4xx other than 408 and 429 ends the
    // subscription.
    status.store(500, std::sync::atomic::Ordering::SeqCst);
    assert!(push(&ctx, id).await.is_err());
    status.store(429, std::sync::atomic::Ordering::SeqCst);
    assert!(push(&ctx, id).await.is_err());
    assert_eq!(subscriptions(&ctx).await, 1);
    status.store(410, std::sync::atomic::Ordering::SeqCst);
    push(&ctx, id).await.unwrap();
    assert_eq!(subscriptions(&ctx).await, 0);
}

/// Subscribe alice's token to `endpoint` with `data`.
async fn subscribe_with(ctx: &TestContext, endpoint: &str, data: Value) -> i64 {
    let (_, p256dh) = eunha::push::generate_vapid_keypair().unwrap();
    let resp: Value = ctx
        .api
        .post_json(
            "/api/v1/push/subscription",
            Some(&ctx.alice_token),
            &json!({
                "subscription": {
                    "endpoint": endpoint,
                    "standard": true,
                    "keys": { "p256dh": p256dh, "auth": "tBHItJI5svbpez7KI4CCXg" },
                },
                "data": data,
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    resp["id"].as_str().unwrap().parse().unwrap()
}

/// Bob's sign-up, told to alice as `admin.sign_up`; how many pushes reached
/// the endpoint for it.
async fn notify_sign_up(
    ctx: &TestContext,
    seen: &std::sync::Mutex<Vec<axum::http::HeaderMap>>,
) -> usize {
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    sqlx::query("DELETE FROM notifications WHERE account_id = $1")
        .bind(alice)
        .execute(&ctx.db)
        .await
        .unwrap();
    seen.lock().unwrap().clear();
    eunha::push::notify_local(&ctx.state, alice, "admin.sign_up", "Account", bob, bob).await;
    ctx.state.jobs.settle().await;
    seen.lock().unwrap().len()
}

/// `Web::PushSubscription#pushable?`: any type is pushed when its alert is
/// on, the staff types among them, and only as the policy allows.
#[tokio::test]
async fn pushes_follow_the_alerts_and_the_policy() {
    let ctx = TestContext::reaching_loopback("push-pushable").await;
    let status = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(201));
    let (endpoint, seen) = push_endpoint(status).await;

    // No alert for the type: nothing.
    subscribe_with(&ctx, &endpoint, json!({"alerts": {"mention": true}})).await;
    assert_eq!(notify_sign_up(&ctx, &seen).await, 0);

    // On, as a form would give it.
    subscribe_with(&ctx, &endpoint, json!({"alerts": {"admin.sign_up": "1"}})).await;
    assert_eq!(notify_sign_up(&ctx, &seen).await, 1);

    // `followed`: only from someone alice follows.
    let followed = json!({"alerts": {"admin.sign_up": true}, "policy": "followed"});
    subscribe_with(&ctx, &endpoint, followed).await;
    assert_eq!(notify_sign_up(&ctx, &seen).await, 0);
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    assert_eq!(notify_sign_up(&ctx, &seen).await, 1);

    // `follower`: only from someone following alice.
    let follower = json!({"alerts": {"admin.sign_up": true}, "policy": "follower"});
    subscribe_with(&ctx, &endpoint, follower).await;
    assert_eq!(notify_sign_up(&ctx, &seen).await, 0);

    // `none`: nothing at all.
    let none = json!({"alerts": {"admin.sign_up": true}, "policy": "none"});
    subscribe_with(&ctx, &endpoint, none).await;
    assert_eq!(notify_sign_up(&ctx, &seen).await, 0);
}

/// `Web::NotificationSerializer`: the subscription's token, the subscriber's
/// locale, the notification, the sender's avatar, the type's subject naming
/// the sender in that locale, and the post's text or else the sender's bio,
/// without tags and cut to 140 characters.
#[tokio::test]
async fn pushes_carry_mastodons_payload() {
    let ctx = TestContext::new("push-payload").await;
    let id = subscribe_with(&ctx, "https://push.example.com/p", json!({})).await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    sqlx::query("UPDATE accounts SET display_name = '', note = $2 WHERE id = $1")
        .bind(bob)
        .bind(format!("<p>Hi &amp; {}</p>", "b".repeat(200)))
        .execute(&ctx.db)
        .await
        .unwrap();

    let notification = sign_up_notification(&ctx).await;
    let payload = eunha::push::payload(&ctx.state, id, notification)
        .await
        .unwrap()
        .unwrap();
    let token: String = sqlx::query_scalar(
        "SELECT t.token FROM oauth_access_tokens t JOIN web_push_subscriptions w ON w.access_token_id = t.id WHERE w.id = $1",
    )
    .bind(id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(payload.access_token, token);
    assert_eq!(payload.notification_id, notification);
    assert_eq!(payload.notification_type, "admin.sign_up");
    assert_eq!(payload.preferred_locale, "en");
    assert_eq!(payload.title, "bob signed up");
    assert!(payload.body.starts_with("Hi & bbb"), "{}", payload.body);
    assert_eq!(payload.body.chars().count(), 140);
    assert!(payload.body.ends_with("..."));
    assert!(!payload.icon.is_empty());

    // In the subscriber's locale.
    sqlx::query("UPDATE users SET locale = 'ko' WHERE account_id = $1")
        .bind(alice)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET display_name = 'Bob' WHERE id = $1")
        .bind(bob)
        .execute(&ctx.db)
        .await
        .unwrap();
    let payload = eunha::push::payload(&ctx.state, id, notification)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(payload.preferred_locale, "ko");
    assert_eq!(payload.title, "Bob 님이 가입했습니다");

    // A post's content warning, else its text.
    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "@alice <b>hello</b> there"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let mention: i64 = sqlx::query_scalar(
        r#"SELECT id FROM notifications WHERE account_id = $1 AND "type" = 'mention'"#,
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let payload = eunha::push::payload(&ctx.state, id, mention)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(payload.title, "Bob 님의 멘션");
    assert_eq!(payload.body, "@alice hello there");
    sqlx::query("UPDATE statuses SET spoiler_text = 'cw' WHERE id = $1")
        .bind(status["id"].as_str().unwrap().parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let payload = eunha::push::payload(&ctx.state, id, mention)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(payload.body, "cw");

    // A type without a subject reads as a missing translation.
    sqlx::query(r#"UPDATE notifications SET "type" = 'annual_report' WHERE id = $1"#)
        .bind(notification)
        .execute(&ctx.db)
        .await
        .unwrap();
    let payload = eunha::push::payload(&ctx.state, id, notification)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        payload.title,
        "Translation missing: ko.notification_mailer.annual_report.subject"
    );
}

/// A favourite's and a boost's push read the post they are about, through
/// the `Favourite` and the boost their notifications point at.
#[tokio::test]
async fn pushes_read_the_post_a_favourite_or_boost_is_about() {
    let ctx = TestContext::new("push-payload-target").await;
    let id = subscribe_with(&ctx, "https://push.example.com/t", json!({})).await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "a post worth keeping", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    for verb in ["favourite", "reblog"] {
        let resp = ctx
            .api
            .post_json(
                &format!("/api/v1/statuses/{sid}/{verb}"),
                Some(&ctx.bob_token),
                &json!({}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let notification: i64 = sqlx::query_scalar(
            r#"SELECT id FROM notifications WHERE account_id = $1 AND "type" = $2"#,
        )
        .bind(alice)
        .bind(verb)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
        let payload = eunha::push::payload(&ctx.state, id, notification)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(payload.body, "a post worth keeping", "{verb}");
    }
}

/// `Web::PushSubscription`'s validations: an endpoint that is no URL, or
/// keys that cannot encrypt, are refused.
#[tokio::test]
async fn push_subscriptions_are_validated() {
    let ctx = TestContext::new("push-valid").await;
    let mut body = fake_sub_payload("ftp://push.example.com/x");
    let resp = ctx
        .api
        .post_json("/api/v1/push/subscription", Some(&ctx.alice_token), &body)
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let error: Value = resp.json().await.unwrap();
    assert_eq!(error["error"], "Validation failed: Endpoint is invalid");

    body["subscription"]["endpoint"] = json!("https://push.example.com/x");
    body["subscription"]["keys"]["p256dh"] = json!("BNotAKey");
    let resp = ctx
        .api
        .post_json("/api/v1/push/subscription", Some(&ctx.alice_token), &body)
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(subscriptions(&ctx).await, 0);
}

/// `Web::PushNotificationWorker#perform`: nothing is sent for a
/// notification older than the TTL, one whose activity is gone, or one the
/// subscription no longer wants; a subscription that is not valid is
/// destroyed.
#[tokio::test]
async fn pushes_are_checked_again_when_sent() {
    let ctx = TestContext::reaching_loopback("push-send-checks").await;
    let status = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(201));
    let (endpoint, seen) = push_endpoint(status).await;
    let id = subscribe(&ctx, &endpoint, true).await;
    let sent = || seen.lock().unwrap().len();

    // Pushed while fresh.
    let notification = sign_up_notification(&ctx).await;
    push_notification(&ctx, id, notification).await.unwrap();
    assert_eq!(sent(), 1);

    // Older than 48 hours: dropped.
    sqlx::query("UPDATE notifications SET updated_at = now() - interval '49 hours' WHERE id = $1")
        .bind(notification)
        .execute(&ctx.db)
        .await
        .unwrap();
    push_notification(&ctx, id, notification).await.unwrap();
    assert_eq!(sent(), 1);

    // About an activity that is gone: dropped.
    let notification = sign_up_notification(&ctx).await;
    sqlx::query("UPDATE notifications SET activity_id = -1 WHERE id = $1")
        .bind(notification)
        .execute(&ctx.db)
        .await
        .unwrap();
    push_notification(&ctx, id, notification).await.unwrap();
    assert_eq!(sent(), 1);

    // The alert turned off since it was queued: dropped.
    let notification = sign_up_notification(&ctx).await;
    ctx.api
        .put_json(
            "/api/v1/push/subscription",
            Some(&ctx.alice_token),
            &json!({"data": {"alerts": {"mention": true}}}),
        )
        .await;
    push_notification(&ctx, id, notification).await.unwrap();
    assert_eq!(sent(), 1);

    // A subscription with keys that cannot encrypt is destroyed.
    sqlx::query("UPDATE web_push_subscriptions SET key_p256dh = 'BNotAKey' WHERE id = $1")
        .bind(id)
        .execute(&ctx.db)
        .await
        .unwrap();
    push_notification(&ctx, id, notification).await.unwrap();
    assert_eq!(sent(), 1);
    assert_eq!(subscriptions(&ctx).await, 0);
}

/// The subscription endpoints take `subscription[...]` and `data[...]` as a
/// form, as Rails reads them; the values are stored as the form gave them
/// and read back cast.
#[tokio::test]
async fn push_subscriptions_take_forms() {
    let ctx = TestContext::new("push-form").await;
    let (_, p256dh) = eunha::push::generate_vapid_keypair().unwrap();
    let resp = ctx
        .api
        .post_form(
            "/api/v1/push/subscription",
            Some(&ctx.alice_token),
            &[
                ("subscription[endpoint]", "https://push.example.com/form"),
                ("subscription[keys][p256dh]", &p256dh),
                ("subscription[keys][auth]", "tBHItJI5svbpez7KI4CCXg"),
                ("subscription[standard]", "true"),
                ("data[alerts][mention]", "true"),
                ("data[alerts][follow]", "0"),
                ("data[policy]", "follower"),
            ],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let sub: Value = resp.json().await.unwrap();
    assert_eq!(sub["standard"], true);
    assert_eq!(sub["alerts"], json!({"mention": true, "follow": false}));
    assert_eq!(sub["policy"], "follower");
    let stored: Value = sqlx::query_scalar("SELECT data::jsonb FROM web_push_subscriptions")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(
        stored,
        json!({"policy": "follower", "alerts": {"mention": "true", "follow": "0"}})
    );

    let resp = ctx
        .api
        .http
        .put(ctx.api.url("/api/v1/push/subscription"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .form(&[("data[alerts][poll]", "true")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sub: Value = resp.json().await.unwrap();
    assert_eq!(sub["alerts"], json!({"poll": true}));
    assert_eq!(sub["policy"], "all");
}
