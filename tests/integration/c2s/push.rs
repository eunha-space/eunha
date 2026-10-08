use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

fn fake_sub_payload(endpoint: &str) -> serde_json::Value {
    json!({
        "subscription": {
            "endpoint": endpoint,
            "keys": {
                "p256dh": "BNcRdreALRFXTkOOUHK1EtK2wtZ5MRe5dvXNkbmkjfGAaLfMIRyWTa8dFbGFnO2hFmPbq3bWI4_4lCLi0bJkLY=",
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
    assert_eq!(updated["alerts"]["follow"].as_bool(), Some(false));
    assert_eq!(updated["alerts"]["favourite"].as_bool(), Some(true));
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
                "data": { "alerts": { "mention": true }, "policy": "all" },
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    resp["id"].as_str().unwrap().parse().unwrap()
}

async fn push(ctx: &TestContext, id: i64) -> anyhow::Result<()> {
    use eunha::jobs::Job as _;
    eunha::push::PushNotificationWorker {
        web_push_subscription_id: id,
        payload: r#"{"title":"hi"}"#.into(),
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
