//! The HTTP methods each route answers, against Mastodon's *config/routes*.
//!
//! Rails routes the `update` action of `resources` and `resource` to both
//! `PUT` and `PATCH`, so a client may send either; a route declared with
//! `patch` alone answers only `PATCH`. An axum route answers `405` to a method
//! it was not given, which is what these look for: whatever else a request
//! gets (`404` for an id that does not exist, `403` for a role it lacks), it
//! was routed.

use reqwest::{Method, StatusCode};
use serde_json::json;

use crate::helpers::TestContext;

/// Every `update` Mastodon 4.7.2 routes under `/api`.
const MASTODON_UPDATES: &[&str] = &[
    "/api/v1/statuses/1",
    "/api/v1/statuses/1/interaction_policy",
    "/api/v1/scheduled_statuses/1",
    "/api/v1/announcements/1/reactions/%F0%9F%91%8D",
    "/api/v1/media/1",
    "/api/v1/filters/1",
    "/api/v1/profile",
    "/api/v1/notifications/policy",
    "/api/v1/lists/1",
    "/api/v1/push/subscription",
    "/api/v1/admin/reports/1",
    "/api/v1/admin/domain_blocks/1",
    "/api/v1/admin/ip_blocks/1",
    "/api/v1/admin/tags/1",
    "/api/v1/collections/1",
    "/api/v1_alpha/collections/1",
    "/api/v2/filters/1",
    "/api/v2/filters/keywords/1",
    "/api/v2/notifications/policy",
];

/// Eunha's REST forms of Mastodon's admin `resources`, whose `update` Rails
/// would route the same way.
const ADMIN_RESOURCE_UPDATES: &[&str] = &[
    "/api/v1/admin/custom_emojis/1",
    "/api/v1/admin/warning_presets/1",
    "/api/v1/admin/username_blocks/1",
    "/api/v1/admin/settings",
    "/api/v1/admin/rules/1",
    "/api/v1/admin/roles/1",
    "/api/v1/admin/announcements/1",
    "/api/v1/admin/webhooks/1",
    "/api/v1/admin/accounts/1/role",
    "/api/v1/admin/terms_of_service/draft",
    "/api/v1/admin/email_subscriptions/additional_footer_text",
];

async fn status(ctx: &TestContext, method: Method, path: &str, token: &str) -> StatusCode {
    ctx.api
        .http
        .request(method, ctx.api.url(path))
        .header("host", &ctx.api.host)
        .bearer_auth(token)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn every_update_answers_put_and_patch() {
    let ctx = TestContext::new("routes-update-methods").await;
    let mut unrouted = Vec::new();
    for path in MASTODON_UPDATES.iter().chain(ADMIN_RESOURCE_UPDATES) {
        for method in [Method::PUT, Method::PATCH] {
            let got = status(&ctx, method.clone(), path, &ctx.alice_token).await;
            if got == StatusCode::METHOD_NOT_ALLOWED {
                unrouted.push(format!("{method} {path}"));
            }
        }
    }
    assert!(unrouted.is_empty(), "not routed: {unrouted:?}");
}

#[tokio::test]
async fn routes_mastodon_lacks_are_not_answered() {
    let ctx = TestContext::new("routes-mastodon-lacks").await;
    // `patch :update_credentials`, not a resource.
    assert_eq!(
        status(
            &ctx,
            Method::PUT,
            "/api/v1/accounts/update_credentials",
            &ctx.alice_token
        )
        .await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    // `resources :lists` has no member `POST`.
    assert_eq!(
        status(&ctx, Method::POST, "/api/v1/lists/1", &ctx.alice_token).await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    // `namespace :v2` routes `resources :media, only: [:create]`, and
    // `Api::V1::Statuses` no longer has a card, nor accounts their pins.
    for path in [
        "/api/v2/media/1",
        "/api/v1/statuses/1/card",
        "/api/v1/accounts/1/pins",
    ] {
        for method in [Method::GET, Method::PUT] {
            let got = status(&ctx, method.clone(), path, &ctx.alice_token).await;
            assert!(
                got == StatusCode::NOT_FOUND || got == StatusCode::METHOD_NOT_ALLOWED,
                "{method} {path} answered {got}"
            );
        }
    }
}

/// `namespace :web` is Mastodon's own web UI's; eunha serves none of it
/// (`mastodon-web-ui-endpoints-not-served` in divergences.toml).
#[tokio::test]
async fn mastodon_web_ui_endpoints_are_not_served() {
    let ctx = TestContext::new("routes-api-web").await;
    let status_id = ctx
        .api
        .post_status(&ctx.alice_token, "embed me", "public")
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    for (method, path) in [
        (Method::PUT, "/api/web/settings".to_owned()),
        (Method::PATCH, "/api/web/settings".to_owned()),
        (Method::GET, format!("/api/web/embeds/{status_id}")),
        (Method::POST, "/api/web/push_subscriptions".to_owned()),
        (Method::PUT, "/api/web/push_subscriptions/1".to_owned()),
        (
            Method::DELETE,
            "/api/web/push_subscriptions/token".to_owned(),
        ),
    ] {
        let got = status(&ctx, method.clone(), &path, &ctx.alice_token).await;
        assert_eq!(got, StatusCode::NOT_FOUND, "{method} {path}");
    }
}
