//! `GET /api/v1/donation_campaigns` asking a fake campaign API.
//!
//! A test binary of its own because it has to let eunha reach 127.0.0.1,
//! which the SSRF guard allows process-wide once granted.

// Test servers have no tenant span to keep.
#![allow(clippy::disallowed_methods)]

#[allow(dead_code)]
#[path = "integration/helpers.rs"]
mod helpers;

use std::sync::{Arc, Mutex};

use axum::extract::{RawQuery, State};
use axum::Router;
use reqwest::StatusCode;
use serde_json::{json, Value};

use helpers::TestContext;

/// What the campaign API was asked, query by query.
#[derive(Clone, Default)]
struct Api {
    queries: Arc<Mutex<Vec<String>>>,
}

async fn campaign(State(api): State<Api>, RawQuery(query): RawQuery) -> axum::Json<Value> {
    api.queries.lock().unwrap().push(query.unwrap_or_default());
    axum::Json(json!({
        "id": "autumn",
        "locale": "en",
        "banner_message": "Keep the lights on",
        "donation_url": "https://donate.example/autumn",
    }))
}

#[tokio::test]
async fn test_donation_campaign_is_fetched_and_cached() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let api = Api::default();
    let app = Router::new()
        .route("/campaigns", axum::routing::get(campaign))
        .with_state(api.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let url = format!("{base}/campaigns?ignored=1");
    let ctx = TestContext::with_config("donation-fetch", move |config| {
        config.allowed_private_networks = vec!["127.0.0.0/8".into()];
        config.instance.donation_campaigns.api_url = Some(url);
        config.instance.donation_campaigns.environment = Some("staging".into());
    })
    .await;
    let seed = eunha::api::mastodon::donation_campaigns::seed(ctx.alice_id.parse().unwrap());

    for _ in 0..2 {
        let resp = ctx
            .api
            .get("/api/v1/donation_campaigns", Some(&ctx.alice_token))
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["id"], "autumn");
    }
    // Asked once, as Mastodon asks; the second came from the cache.
    assert_eq!(
        *api.queries.lock().unwrap(),
        vec![format!(
            "environment=staging&locale=en&platform=web&seed={seed}"
        )]
    );
    let mut redis = ctx.state.redis.clone();
    let key: Option<String> = redis::cmd("GET")
        .arg(
            ctx.state
                .redis_keys
                .key(format!("cache:donation_campaign_request:{seed}:en")),
        )
        .query_async(&mut redis)
        .await
        .unwrap();
    assert_eq!(key.as_deref(), Some("autumn:en"));
}
