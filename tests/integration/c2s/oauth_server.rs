//! Mastodon's OAuth server beyond the token endpoint: the RFC 8414 metadata,
//! the OpenID Connect UserInfo endpoint, and what Doorkeeper mounts with
//! `use_doorkeeper`.

use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::{seed_token_with_scopes, TestContext};

#[tokio::test]
async fn the_authorization_server_metadata_describes_doorkeeper() {
    let ctx = TestContext::new("oauth-metadata").await;
    let resp = ctx
        .api
        .get("/.well-known/oauth-authorization-server", None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let root = format!("https://{}", ctx.domain);
    assert_eq!(body["issuer"], format!("{root}/"));
    assert_eq!(
        body["authorization_endpoint"],
        format!("{root}/oauth/authorize")
    );
    assert_eq!(body["token_endpoint"], format!("{root}/oauth/token"));
    assert_eq!(body["revocation_endpoint"], format!("{root}/oauth/revoke"));
    assert_eq!(body["userinfo_endpoint"], format!("{root}/oauth/userinfo"));
    assert_eq!(
        body["app_registration_endpoint"],
        format!("{root}/api/v1/apps")
    );
    assert_eq!(body["scopes_supported"][0], "read");
    assert_eq!(body["scopes_supported"][1], "profile");
    assert_eq!(
        body["response_types_supported"],
        serde_json::json!(["code"])
    );
    assert_eq!(
        body["grant_types_supported"],
        serde_json::json!(["authorization_code", "client_credentials"])
    );
    assert_eq!(
        body["code_challenge_methods_supported"],
        serde_json::json!(["S256"])
    );
    let keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys[0], "issuer");
    assert_eq!(keys.last(), Some(&"app_registration_endpoint"));
}

#[tokio::test]
async fn userinfo_answers_a_profile_token_by_get_and_post() {
    let ctx = TestContext::new("oauth-userinfo").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let token = seed_token_with_scopes(&ctx.db, alice, "profile").await;

    let resp = ctx.api.get("/oauth/userinfo", Some(&token)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["iss"], format!("https://{}/", ctx.domain));
    assert!(body["sub"].as_str().unwrap().ends_with("/users/alice"));
    assert_eq!(body["preferred_username"], "alice");
    assert_eq!(body["profile"], format!("https://{}/@alice", ctx.domain));
    assert!(body["picture"].is_string());
    assert!(body.get("name").is_some());

    let resp = ctx
        .api
        .post_json("/oauth/userinfo", Some(&token), &serde_json::json!({}))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // `doorkeeper_authorize! :profile`: no token, or one without the scope.
    let resp = ctx.api.get("/oauth/userinfo", None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let read = seed_token_with_scopes(&ctx.db, alice, "read").await;
    let resp = ctx.api.get("/oauth/userinfo", Some(&read)).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
