//! Mastodon's rate limits: the `Rack::Attack` throttles and the
//! `RateLimiter` families, with the headers and the `429` they answer with.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn limited(label: &str) -> TestContext {
    TestContext::with_config(label, |config| config.limits.rate_limits = Some(true)).await
}

fn header(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .map(|v| v.to_str().unwrap().to_owned())
}

/// A request from `ip`, as a proxy in front of eunha says it.
fn from(
    ctx: &TestContext,
    method: reqwest::Method,
    path: &str,
    ip: &str,
) -> reqwest::RequestBuilder {
    ctx.api
        .http
        .request(method, ctx.api.url(path))
        .header("host", &ctx.api.host)
        .header("x-forwarded-for", ip)
}

/// When `reset` is, it is on a boundary of `period` seconds, with the
/// microseconds of the moment it was asked.
fn on_boundary(reset: &str, period: i64) -> bool {
    let at = chrono::DateTime::parse_from_rfc3339(reset).unwrap();
    reset.ends_with('Z')
        && reset.len() == "2026-01-01T00:00:00.000000Z".len()
        && at.timestamp() % period == 0
}

#[tokio::test]
async fn api_responses_say_what_is_left_of_the_tightest_throttle() {
    let ctx = limited("ratelimit-headers").await;

    // Unauthenticated: by address.
    let resp = from(
        &ctx,
        reqwest::Method::GET,
        "/api/v1/instance",
        "203.0.113.7",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(header(&resp, "x-ratelimit-limit").as_deref(), Some("300"));
    assert_eq!(
        header(&resp, "x-ratelimit-remaining").as_deref(),
        Some("299")
    );
    let reset = header(&resp, "x-ratelimit-reset").unwrap();
    assert!(on_boundary(&reset, 300), "{reset}");

    // Authenticated: per user (1,500) and per token (300), the latter
    // tighter.
    for remaining in ["299", "298"] {
        let resp = from(
            &ctx,
            reqwest::Method::GET,
            "/api/v1/accounts/verify_credentials",
            "203.0.113.7",
        )
        .bearer_auth(&ctx.alice_token)
        .send()
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(header(&resp, "x-ratelimit-limit").as_deref(), Some("300"));
        assert_eq!(
            header(&resp, "x-ratelimit-remaining").as_deref(),
            Some(remaining)
        );
    }

    // A page asked for counts against paging's 300 a quarter hour as well.
    let resp = from(
        &ctx,
        reqwest::Method::GET,
        "/api/v1/timelines/public?max_id=1",
        "203.0.113.7",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(
        header(&resp, "x-ratelimit-remaining").as_deref(),
        Some("298")
    );

    // No controller answered: no headers, as a routing error has none.
    let resp = from(
        &ctx,
        reqwest::Method::GET,
        "/api/v1/nothing_here",
        "203.0.113.7",
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(header(&resp, "x-ratelimit-limit"), None);

    // Pages that are not the API are throttled but say nothing.
    let resp = from(&ctx, reqwest::Method::GET, "/about", "203.0.113.7")
        .send()
        .await
        .unwrap();
    assert_eq!(header(&resp, "x-ratelimit-limit"), None);
}

#[tokio::test]
async fn a_throttle_exceeded_answers_429() {
    let ctx = limited("ratelimit-apps").await;
    let register = || {
        from(&ctx, reqwest::Method::POST, "/api/v1/apps", "198.51.100.9")
            .json(&json!({"client_name": "x", "redirect_uris": "urn:ietf:wg:oauth:2.0:oob"}))
            .send()
    };
    for _ in 0..5 {
        assert_eq!(register().await.unwrap().status(), StatusCode::OK);
    }
    let resp = register().await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&resp, "x-ratelimit-limit").as_deref(), Some("5"));
    assert_eq!(header(&resp, "x-ratelimit-remaining").as_deref(), Some("0"));
    assert!(on_boundary(
        &header(&resp, "x-ratelimit-reset").unwrap(),
        600
    ));
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({"error": "Too many requests"})
    );

    // Another address is counted on its own, and so is another instance.
    assert_eq!(
        from(&ctx, reqwest::Method::POST, "/api/v1/apps", "198.51.100.10")
            .json(&json!({"client_name": "x", "redirect_uris": "urn:ietf:wg:oauth:2.0:oob"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let other = limited("ratelimit-apps-other").await;
    assert_eq!(
        from(
            &other,
            reqwest::Method::POST,
            "/api/v1/apps",
            "198.51.100.9"
        )
        .json(&json!({"client_name": "x", "redirect_uris": "urn:ietf:wg:oauth:2.0:oob"}))
        .send()
        .await
        .unwrap()
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn sign_in_attempts_are_throttled_by_address() {
    let ctx = limited("ratelimit-login").await;
    let attempt = || {
        from(&ctx, reqwest::Method::POST, "/account/login", "192.0.2.44")
            .form(&[("email", "nobody@test.invalid"), ("password", "wrong")])
            .send()
    };
    for _ in 0..25 {
        assert_ne!(
            attempt().await.unwrap().status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    let resp = attempt().await.unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&resp, "x-ratelimit-limit").as_deref(), Some("25"));
}

/// The key `RateLimiter` counts an account's family in this period.
fn family_key(ctx: &TestContext, account_id: &str, family: &str, period: i64) -> String {
    let epoch = chrono::Utc::now().timestamp();
    ctx.state.redis_keys.key(format!(
        "rate_limit:{account_id}:{family}:{}",
        epoch / period
    ))
}

async fn redis_get(ctx: &TestContext, key: &str) -> Option<i64> {
    let mut redis = ctx.state.redis.clone();
    redis::cmd("GET")
        .arg(key)
        .query_async(&mut redis)
        .await
        .unwrap()
}

#[tokio::test]
async fn statuses_are_counted_and_refused_past_the_family_limit() {
    let ctx = limited("ratelimit-statuses").await;
    let key = family_key(&ctx, &ctx.alice_id, "statuses", 3 * 3600);

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "one"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(redis_get(&ctx, &key).await, Some(1));
    assert_eq!(header(&resp, "x-ratelimit-limit").as_deref(), Some("300"));
    assert_eq!(
        header(&resp, "x-ratelimit-remaining").as_deref(),
        Some("299")
    );
    assert!(on_boundary(
        &header(&resp, "x-ratelimit-reset").unwrap(),
        3 * 3600
    ));
    let id = resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // An edit counts too.
    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.alice_token),
            &json!({"status": "one, edited"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(redis_get(&ctx, &key).await, Some(2));

    // A boost says the family's headers but is not counted.
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bob_key = family_key(&ctx, &ctx.bob_id, "statuses", 3 * 3600);
    assert_eq!(redis_get(&ctx, &bob_key).await, None);
    assert_eq!(header(&resp, "x-ratelimit-limit").as_deref(), Some("300"));

    // At the limit, the next post is refused and nothing is written.
    let mut redis = ctx.state.redis.clone();
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg(300)
        .query_async(&mut redis)
        .await
        .unwrap();
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "one too many"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(header(&resp, "x-ratelimit-limit").as_deref(), Some("300"));
    assert_eq!(header(&resp, "x-ratelimit-remaining").as_deref(), Some("0"));
    assert_eq!(
        resp.json::<Value>().await.unwrap(),
        json!({"error": "Too many requests"})
    );
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(before, after);
}

#[tokio::test]
async fn only_new_follows_are_counted() {
    let ctx = limited("ratelimit-follows").await;
    let key = family_key(&ctx, &ctx.alice_id, "follows", 24 * 3600);
    for _ in 0..2 {
        let resp = ctx
            .api
            .post_json(
                &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(header(&resp, "x-ratelimit-limit").as_deref(), Some("300"));
    }
    assert_eq!(redis_get(&ctx, &key).await, Some(1));

    for _ in 0..2 {
        let resp = ctx
            .api
            .post_json(
                "/api/v1/tags/rust/follow",
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    assert_eq!(redis_get(&ctx, &key).await, Some(2));
}
