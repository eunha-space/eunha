use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// POST /api/v1/apps with valid params returns client_id and client_secret.
#[tokio::test]
async fn test_register_app_returns_credentials() {
    let ctx = TestContext::new("apps-reg").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &serde_json::json!({
                "client_name": "Test App",
                "redirect_uris": "urn:ietf:wg:oauth:2.0:oob",
                "scopes": "read write"
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.unwrap();
    assert!(body["client_id"].as_str().is_some(), "missing client_id");
    assert!(
        body["client_secret"].as_str().is_some(),
        "missing client_secret"
    );
    assert_eq!(body["name"].as_str(), Some("Test App"));
    let timestamps: (bool, bool) = sqlx::query_as("SELECT created_at IS NOT NULL, updated_at IS NOT NULL FROM oauth_applications WHERE uid=$1")
        .bind(body["client_id"].as_str().unwrap()).fetch_one(&ctx.db).await.unwrap();
    assert_eq!(timestamps, (true, true));
}

/// GET /oauth/authorize rejects a scope broader than the app registered
/// (Doorkeeper invalid_scope), but accepts a subset.
#[tokio::test]
async fn test_authorize_rejects_scope_escalation() {
    let ctx = TestContext::new("authz-scope").await;

    let app: Value = ctx.api.post_json(
        "/api/v1/apps",
        None,
        &json!({ "client_name": "Narrow App", "redirect_uris": "urn:ietf:wg:oauth:2.0:oob", "scopes": "read" }),
    ).await.json().await.unwrap();
    let client_id = app["client_id"].as_str().unwrap();

    let escalate = ctx.api.get(
        &format!("/oauth/authorize?client_id={client_id}&redirect_uri=urn:ietf:wg:oauth:2.0:oob&scope=read+write&response_type=code"),
        None,
    ).await;
    assert_eq!(
        escalate.status(),
        StatusCode::BAD_REQUEST,
        "requesting an unregistered scope must be rejected"
    );

    let ok = ctx.api.get(
        &format!("/oauth/authorize?client_id={client_id}&redirect_uri=urn:ietf:wg:oauth:2.0:oob&scope=read&response_type=code"),
        None,
    ).await;
    assert_eq!(
        ok.status(),
        StatusCode::OK,
        "a subset scope should be accepted"
    );
}

/// POST /api/v1/apps with an unknown scope is rejected (Doorkeeper
/// enforce_configured_scopes). A granular known scope is still accepted.
#[tokio::test]
async fn test_register_app_rejects_unknown_scope() {
    let ctx = TestContext::new("apps-bad-scope").await;

    let bad = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({ "client_name": "Bad", "scopes": "read write:everything" }),
        )
        .await;
    assert_eq!(
        bad.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown scope must be rejected"
    );

    let ok = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({ "client_name": "Good", "scopes": "read:statuses write:statuses push" }),
        )
        .await;
    assert_eq!(
        ok.status(),
        StatusCode::OK,
        "known granular scopes should be accepted"
    );
}

/// Registered app response includes the redirect_uri and redirect_uris fields.
#[tokio::test]
async fn test_register_app_response_shape() {
    let ctx = TestContext::new("apps-shape").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &serde_json::json!({
                "client_name": "Shape Test",
                "redirect_uris": "https://app.example/callback",
                "scopes": "read"
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["redirect_uri"].as_str(),
        Some("https://app.example/callback")
    );
    assert_eq!(
        body["redirect_uris"][0].as_str(),
        Some("https://app.example/callback")
    );
    assert!(
        body["scopes"]
            .as_array()
            .is_some_and(|a| a.iter().any(|s| s.as_str() == Some("read"))),
        "scopes array should contain 'read'"
    );
}

/// Omitting scopes defaults to read.
#[tokio::test]
async fn test_register_app_default_scope() {
    let ctx = TestContext::new("apps-dflt").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &serde_json::json!({
                "client_name": "Default Scope App",
                "redirect_uris": "urn:ietf:wg:oauth:2.0:oob"
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["scopes"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|s| s.as_str()),
        Some("read"),
        "default scope should be read"
    );
}

/// Omitting client_name returns 422.
#[tokio::test]
async fn test_register_app_missing_client_name_unprocessable() {
    let ctx = TestContext::new("apps-noname").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &serde_json::json!({
                "redirect_uris": "urn:ietf:wg:oauth:2.0:oob",
                "scopes": "read"
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Multiple apps can be registered independently (distinct client_ids).
#[tokio::test]
async fn test_register_multiple_apps() {
    let ctx = TestContext::new("apps-multi").await;

    let base_payload = |name: &str| {
        serde_json::json!({
            "client_name": name,
            "redirect_uris": "urn:ietf:wg:oauth:2.0:oob",
            "scopes": "read"
        })
    };

    let r1: Value = ctx
        .api
        .post_json("/api/v1/apps", None, &base_payload("App One"))
        .await
        .json()
        .await
        .unwrap();
    let r2: Value = ctx
        .api
        .post_json("/api/v1/apps", None, &base_payload("App Two"))
        .await
        .json()
        .await
        .unwrap();

    assert_ne!(
        r1["client_id"].as_str(),
        r2["client_id"].as_str(),
        "two apps should get distinct client_ids"
    );
}

/// POST /oauth/revoke invalidates a token so it can no longer be used.
#[tokio::test]
async fn test_revoke_token() {
    let ctx = TestContext::new("apps-revoke").await;

    // Verify that the token is currently valid.
    let before = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(
        before.status(),
        StatusCode::OK,
        "token should be valid before revocation"
    );

    // Revoke it.
    let revoke_resp = ctx
        .api
        .post_json(
            "/oauth/revoke",
            None,
            &serde_json::json!({"token": ctx.alice_token}),
        )
        .await;
    assert_eq!(revoke_resp.status(), StatusCode::OK);

    // After revocation the token should no longer work.
    let after = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(
        after.status(),
        StatusCode::UNAUTHORIZED,
        "revoked token should return 401"
    );
}

// ── POST /oauth/token — grant type tests ──────────────────────────────────────

/// Helper: register an app and return (client_id, client_secret).
async fn register_test_app(ctx: &TestContext) -> (String, String) {
    let body: Value = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({
                "client_name": "OAuth Test App",
                "redirect_uris": "urn:ietf:wg:oauth:2.0:oob",
                "scopes": "read write"
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    (
        body["client_id"].as_str().unwrap().to_string(),
        body["client_secret"].as_str().unwrap().to_string(),
    )
}

/// client_credentials grant returns a bearer token.
#[tokio::test]
async fn test_client_credentials_grant() {
    let ctx = TestContext::new("oauth-cc").await;
    let (client_id, client_secret) = register_test_app(&ctx).await;

    let resp = ctx
        .api
        .post_json(
            "/oauth/token",
            None,
            &json!({
                "grant_type": "client_credentials",
                "client_id": client_id,
                "client_secret": client_secret,
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["access_token"].as_str().is_some(),
        "access_token missing"
    );
    assert_eq!(body["token_type"].as_str(), Some("Bearer"));
}

/// A grant with the wrong client_secret returns 401.
#[tokio::test]
async fn test_wrong_client_secret_returns_401() {
    let ctx = TestContext::new("oauth-badsecret").await;
    let (client_id, _) = register_test_app(&ctx).await;

    let resp = ctx
        .api
        .post_json(
            "/oauth/token",
            None,
            &json!({
                "grant_type": "client_credentials",
                "client_id": client_id,
                "client_secret": "not-the-real-secret",
            }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "bad client_secret should return 401"
    );
}

/// authorization_code grant with a seeded code issues a token.
#[tokio::test]
async fn test_authorization_code_grant() {
    let ctx = TestContext::new("oauth-ac").await;

    // Register an app to get a real application_id.
    let app: Value = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({
                "client_name": "Auth Code App",
                "redirect_uris": "https://app.example/callback",
                "scopes": "read write"
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let client_id = app["client_id"].as_str().unwrap();
    let client_secret = app["client_secret"].as_str().unwrap();

    // Look up the DB application_id by client_id.
    let app_id: i64 = sqlx::query_scalar!(
        "SELECT id FROM oauth_applications WHERE uid = $1",
        client_id,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();

    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let owner_id = crate::helpers::user_id_for(&ctx.db, alice_id).await;
    let code = "test-auth-code-12345";

    sqlx::query!(
        r#"INSERT INTO oauth_access_grants
             (application_id, resource_owner_id, token, redirect_uri, scopes, expires_in, created_at)
           VALUES ($1, $2, $3, $4, $5, 600, now())"#,
        app_id,
        owner_id,
        code,
        "https://app.example/callback",
        "read write",
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let resp = ctx
        .api
        .post_json(
            "/oauth/token",
            None,
            &json!({
                "grant_type": "authorization_code",
                "client_id": client_id,
                "client_secret": client_secret,
                "code": code,
                "redirect_uri": "https://app.example/callback",
            }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "authorization_code grant should succeed"
    );
    let body: Value = resp.json().await.unwrap();
    let token = body["access_token"].as_str().expect("access_token missing");

    let me = ctx
        .api
        .get("/api/v1/accounts/verify_credentials", Some(token))
        .await;
    assert_eq!(
        me.status(),
        StatusCode::OK,
        "token from authorization_code grant should authenticate"
    );
}

/// Expired authorization code returns 401.
#[tokio::test]
async fn test_authorization_code_expired_returns_401() {
    let ctx = TestContext::new("oauth-ac-exp").await;

    let app: Value = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({
                "client_name": "Expired Code App",
                "redirect_uris": "https://app.example/callback",
                "scopes": "read"
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let client_id = app["client_id"].as_str().unwrap();
    let client_secret = app["client_secret"].as_str().unwrap();

    let app_id: i64 = sqlx::query_scalar!(
        "SELECT id FROM oauth_applications WHERE uid = $1",
        client_id,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();

    let expired_code = format!("expired-code-{}", uuid::Uuid::new_v4());
    let owner_id = crate::helpers::user_id_for(&ctx.db, ctx.alice_id.parse::<i64>().unwrap()).await;
    sqlx::query!(
        r#"INSERT INTO oauth_access_grants
             (application_id, resource_owner_id, token, redirect_uri, scopes, expires_in, created_at)
           VALUES ($1, $2, $3, $4, $5, 600, now() - interval '1 hour')"#,
        app_id,
        owner_id,
        expired_code,
        "https://app.example/callback",
        "read",
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let resp = ctx
        .api
        .post_json(
            "/oauth/token",
            None,
            &json!({
                "grant_type": "authorization_code",
                "client_id": client_id,
                "client_secret": client_secret,
                "code": expired_code,
                "redirect_uri": "https://app.example/callback",
            }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "expired code should return 401"
    );
}

/// `grant_flows %w(authorization_code client_credentials)`: any other grant
/// type, the password grant among them, is Doorkeeper's
/// `unsupported_grant_type`, before the client is looked at.
#[tokio::test]
async fn test_unsupported_grant_types_are_refused_as_doorkeeper_refuses_them() {
    let ctx = TestContext::new("oauth-bad-grant").await;
    let (client_id, client_secret) = register_test_app(&ctx).await;

    for grant_type in ["password", "magic_token", "refresh_token"] {
        let resp = ctx
            .api
            .post_json(
                "/oauth/token",
                None,
                &json!({
                    "grant_type": grant_type,
                    "client_id": client_id,
                    "client_secret": client_secret,
                    "username": "alice@test.invalid",
                    "password": "testpassword123",
                }),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{grant_type}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(
            body,
            json!({
                "error": "unsupported_grant_type",
                "error_description": "The authorization grant type is not supported by the authorization server.",
            })
        );
    }
}

/// Doorkeeper's redirect URI checks: authorization only for a URI the app
/// registered (extra query allowed), and a code only good with the URI it was
/// issued for.
#[tokio::test]
async fn test_redirect_uri_must_be_registered_and_match_the_code() {
    let ctx = TestContext::new("oauth-redirect").await;
    let app: Value = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({
                "client_name": "Redirecting App",
                "redirect_uris": "https://app.example/callback\nurn:ietf:wg:oauth:2.0:oob",
                "scopes": "read"
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let client_id = app["client_id"].as_str().unwrap();
    let client_secret = app["client_secret"].as_str().unwrap();
    let authorize = |uri: &str| {
        format!(
            "/oauth/authorize?client_id={client_id}&redirect_uri={}&scope=read&response_type=code",
            urlencoding::encode(uri)
        )
    };
    for (uri, status) in [
        ("https://app.example/callback", StatusCode::OK),
        ("https://app.example/callback?state=x", StatusCode::OK),
        ("urn:ietf:wg:oauth:2.0:oob", StatusCode::OK),
        ("https://evil.example/callback", StatusCode::BAD_REQUEST),
        ("https://app.example/other", StatusCode::BAD_REQUEST),
        ("javascript:alert(1)", StatusCode::BAD_REQUEST),
    ] {
        assert_eq!(
            ctx.api.get(&authorize(uri), None).await.status(),
            status,
            "{uri}"
        );
    }

    let app_id: i64 = sqlx::query_scalar("SELECT id FROM oauth_applications WHERE uid = $1")
        .bind(client_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let owner = crate::helpers::user_id_for(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    sqlx::query(
        "INSERT INTO oauth_access_grants
           (application_id, resource_owner_id, token, redirect_uri, scopes, expires_in, created_at)
         VALUES ($1, $2, 'redirect-code', 'https://app.example/callback', 'read', 600, now())",
    )
    .bind(app_id)
    .bind(owner)
    .execute(&ctx.db)
    .await
    .unwrap();
    let exchange = |uri: Option<&str>| {
        let mut body = json!({
            "grant_type": "authorization_code",
            "client_id": client_id,
            "client_secret": client_secret,
            "code": "redirect-code",
        });
        if let Some(uri) = uri {
            body["redirect_uri"] = json!(uri);
        }
        body
    };
    for uri in [None, Some("https://evil.example/callback")] {
        let refused = ctx
            .api
            .post_json("/oauth/token", None, &exchange(uri))
            .await;
        assert_eq!(refused.status(), StatusCode::UNAUTHORIZED, "{uri:?}");
    }
    // A refused exchange does not use the code up.
    let ok = ctx
        .api
        .post_json(
            "/oauth/token",
            None,
            &exchange(Some("https://app.example/callback")),
        )
        .await;
    assert_eq!(ok.status(), StatusCode::OK);
}
