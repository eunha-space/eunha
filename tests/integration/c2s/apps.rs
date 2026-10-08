use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{client_of, TestContext};

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

    // Revoke it, as the client it was issued to.
    let (client_id, client_secret) = client_of(&ctx, &ctx.alice_token).await;
    let revoke_resp = ctx
        .api
        .post_json(
            "/oauth/revoke",
            None,
            &serde_json::json!({
                "token": ctx.alice_token,
                "client_id": client_id,
                "client_secret": client_secret,
            }),
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

/// Whether `token` still works.
async fn token_works(ctx: &TestContext, token: &str) -> bool {
    ctx.api
        .get("/api/v1/accounts/verify_credentials", Some(token))
        .await
        .status()
        == StatusCode::OK
}

/// Doorkeeper's `validate_presence_of_client` and `authorized?`: a request
/// naming no client, or a confidential client without its secret, or a
/// client the token was not issued to, is refused with `403
/// unauthorized_client`, and the token keeps working.
#[tokio::test]
async fn test_revoke_requires_the_tokens_client() {
    let ctx = TestContext::new("apps-revoke-client").await;
    let (client_id, _) = client_of(&ctx, &ctx.alice_token).await;
    let (other_id, other_secret) = client_of(&ctx, &ctx.bob_token).await;
    let refusals = [
        json!({ "token": ctx.alice_token }),
        json!({ "token": ctx.alice_token, "client_id": client_id }),
        json!({ "token": ctx.alice_token, "client_id": client_id, "client_secret": "wrong" }),
        json!({ "token": ctx.alice_token, "client_id": other_id, "client_secret": other_secret }),
    ];
    for body in refusals {
        let resp = ctx.api.post_json("/oauth/revoke", None, &body).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{body}");
        let answer: Value = resp.json().await.unwrap();
        assert_eq!(answer["error"], "unauthorized_client");
        assert_eq!(
            answer["error_description"],
            "You are not authorized to revoke this token"
        );
    }
    assert!(token_works(&ctx, &ctx.alice_token).await);

    // A token nobody holds is a `200` for an authenticated client.
    let resp = ctx
        .api
        .post_json(
            "/oauth/revoke",
            None,
            &json!({ "token": "nonexistent", "client_id": other_id, "client_secret": other_secret }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.json::<Value>().await.unwrap(), json!({}));
}

/// `client_secret_basic` authenticates as well as the parameters; a
/// public client (`confidential = false`) names itself without a secret;
/// a secret by two methods is `400 invalid_request`.
#[tokio::test]
async fn test_revoke_client_authentication_methods() {
    let ctx = TestContext::new("apps-revoke-basic").await;
    let (client_id, client_secret) = client_of(&ctx, &ctx.alice_token).await;

    let both = ctx
        .api
        .http
        .post(ctx.api.url("/oauth/revoke"))
        .header("host", &ctx.api.host)
        .basic_auth(&client_id, Some(&client_secret))
        .form(&[
            ("token", ctx.alice_token.as_str()),
            ("client_id", &client_id),
            ("client_secret", &client_secret),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(both.status(), StatusCode::BAD_REQUEST);
    let answer: Value = both.json().await.unwrap();
    assert_eq!(answer["error"], "invalid_request");
    assert!(token_works(&ctx, &ctx.alice_token).await);

    let basic = ctx
        .api
        .http
        .post(ctx.api.url("/oauth/revoke"))
        .header("host", &ctx.api.host)
        .basic_auth(&client_id, Some(&client_secret))
        .form(&[("token", ctx.alice_token.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(basic.status(), StatusCode::OK);
    assert!(!token_works(&ctx, &ctx.alice_token).await);

    // A public client.
    let (bob_client, _) = client_of(&ctx, &ctx.bob_token).await;
    sqlx::query!(
        "UPDATE oauth_applications SET confidential = false WHERE uid = $1",
        bob_client
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let public = ctx
        .api
        .post_form(
            "/oauth/revoke",
            None,
            &[("token", &ctx.bob_token), ("client_id", &bob_client)],
        )
        .await;
    assert_eq!(public.status(), StatusCode::OK);
    assert!(!token_works(&ctx, &ctx.bob_token).await);
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

/// An expired authorization code is `invalid_grant`.
#[tokio::test]
async fn test_authorization_code_expired_is_invalid_grant() {
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
    // `validate_grant`: Doorkeeper's `invalid_grant`, a `400`.
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "expired code is invalid_grant"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "invalid_grant");
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
        // Doorkeeper: a missing redirect URI is `invalid_request`, another
        // one `invalid_grant`, both a `400`.
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST, "{uri:?}");
        let body: Value = refused.json().await.unwrap();
        let expected = if uri.is_none() {
            "invalid_request"
        } else {
            "invalid_grant"
        };
        assert_eq!(body["error"], expected, "{uri:?}");
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

/// Seed an authorization code for alice; the app's DB id.
async fn seed_code(ctx: &TestContext, client_id: &str, code: &str, scopes: &str) {
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let owner = crate::helpers::user_id_for(&ctx.db, alice_id).await;
    sqlx::query(
        "INSERT INTO oauth_access_grants
           (application_id, resource_owner_id, token, redirect_uri, scopes, expires_in, created_at)
         SELECT id, $2, $3, 'https://app.example/callback', $4, 600, now()
         FROM oauth_applications WHERE uid = $1",
    )
    .bind(client_id)
    .bind(owner)
    .bind(code)
    .bind(scopes)
    .execute(&ctx.db)
    .await
    .unwrap();
}

/// Mastodon's `reuse_access_token`: a second code for the same scopes gets
/// the token the first one did, with its `created_at`; other scopes get a
/// new one. A code is revoked, not deleted, once used, and using it again
/// is `invalid_grant`. The answer is kept out of caches.
#[tokio::test]
async fn test_a_code_reuses_a_matching_token() {
    let ctx = TestContext::new("oauth-reuse").await;
    let api = &ctx.api;
    let app: Value = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({
                "client_name": "Reuse",
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
    let exchange = |code: &'static str| {
        let body = json!({
            "grant_type": "authorization_code",
            "client_id": client_id,
            "client_secret": client_secret,
            "code": code,
            "redirect_uri": "https://app.example/callback",
        });
        async move { api.post_json("/oauth/token", None, &body).await }
    };

    seed_code(&ctx, client_id, "first-code", "read write").await;
    let first = exchange("first-code").await;
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(first.headers()["cache-control"], "no-store, no-cache");
    assert_eq!(first.headers()["pragma"], "no-cache");
    let first: Value = first.json().await.unwrap();
    assert!(first.get("expires_in").is_none());
    assert!(first.get("refresh_token").is_none());
    let revoked: bool = sqlx::query_scalar(
        "SELECT revoked_at IS NOT NULL FROM oauth_access_grants WHERE token = 'first-code'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(revoked, "the code is revoked, not deleted");
    let again = exchange("first-code").await;
    assert_eq!(again.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        again.json::<Value>().await.unwrap()["error"],
        "invalid_grant"
    );

    seed_code(&ctx, client_id, "second-code", "write read").await;
    let second: Value = exchange("second-code").await.json().await.unwrap();
    assert_eq!(second["access_token"], first["access_token"]);
    assert_eq!(second["created_at"], first["created_at"]);

    seed_code(&ctx, client_id, "third-code", "read").await;
    let third: Value = exchange("third-code").await.json().await.unwrap();
    assert_ne!(third["access_token"], first["access_token"]);
    assert_eq!(third["scope"], "read");
}

/// The client credentials grant: without `scope`, the default scopes the
/// application has (`read`); a scope beyond the application's is
/// `invalid_scope`; a token with the same scopes is reused.
#[tokio::test]
async fn test_client_credentials_scopes_and_reuse() {
    let ctx = TestContext::new("oauth-cc-scopes").await;
    let api = &ctx.api;
    let (client_id, client_secret) = register_test_app(&ctx).await;
    let grant = |scope: Option<&str>| {
        let mut body = json!({
            "grant_type": "client_credentials",
            "client_id": client_id,
            "client_secret": client_secret,
        });
        if let Some(scope) = scope {
            body["scope"] = json!(scope);
        }
        async move { api.post_json("/oauth/token", None, &body).await }
    };
    let default: Value = grant(None).await.json().await.unwrap();
    assert_eq!(default["scope"], "read");
    let again: Value = grant(Some("read")).await.json().await.unwrap();
    assert_eq!(again["access_token"], default["access_token"]);
    let wider: Value = grant(Some("read write")).await.json().await.unwrap();
    assert_eq!(wider["scope"], "read write");
    assert_ne!(wider["access_token"], default["access_token"]);
    let refused = grant(Some("read follow")).await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        refused.json::<Value>().await.unwrap()["error"],
        "invalid_scope"
    );
}

/// What `/oauth/token` refuses before it gets anywhere, as Doorkeeper does:
/// no grant type, no code, an unknown client (`401 invalid_client` with a
/// `WWW-Authenticate`), and a secret given two ways. HTTP Basic credentials
/// are taken as they decode, without URL-decoding.
#[tokio::test]
async fn test_token_requests_doorkeeper_refuses() {
    let ctx = TestContext::new("oauth-token-refusals").await;
    let api = &ctx.api;
    let (client_id, client_secret) = register_test_app(&ctx).await;
    let post = |body: Value| async move { api.post_json("/oauth/token", None, &body).await };

    let no_grant = post(json!({ "client_id": client_id })).await;
    assert_eq!(no_grant.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        no_grant.json::<Value>().await.unwrap()["error_description"],
        "Missing required parameter: grant_type."
    );
    let no_code = post(json!({
        "grant_type": "authorization_code",
        "client_id": client_id,
        "client_secret": client_secret,
        "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
    }))
    .await;
    assert_eq!(
        no_code.json::<Value>().await.unwrap()["error_description"],
        "Missing required parameter: code."
    );
    let unknown = post(json!({
        "grant_type": "client_credentials",
        "client_id": "nobody",
        "client_secret": "nothing",
    }))
    .await;
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);
    assert!(unknown.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .contains("error=\"invalid_client\""));
    assert_eq!(
        unknown.json::<Value>().await.unwrap()["error"],
        "invalid_client"
    );

    let basic = |id: &str, secret: &str| {
        use base64::Engine as _;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"))
        )
    };
    let both = ctx
        .api
        .http
        .post(ctx.api.url("/oauth/token"))
        .header("host", &ctx.api.host)
        .header("authorization", basic(&client_id, &client_secret))
        .form(&[
            ("grant_type", "client_credentials"),
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(both.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        both.json::<Value>().await.unwrap()["error"],
        "invalid_request"
    );

    // A secret that only matches once URL-decoded does not.
    sqlx::query("UPDATE oauth_applications SET secret = 'a b' WHERE uid = $1")
        .bind(&client_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    let encoded = ctx
        .api
        .http
        .post(ctx.api.url("/oauth/token"))
        .header("host", &ctx.api.host)
        .header("authorization", basic(&client_id, "a%20b"))
        .form(&[("grant_type", "client_credentials")])
        .send()
        .await
        .unwrap();
    assert_eq!(encoded.status(), StatusCode::UNAUTHORIZED);
    let plain = ctx
        .api
        .http
        .post(ctx.api.url("/oauth/token"))
        .header("host", &ctx.api.host)
        .header("authorization", basic(&client_id, "a b"))
        .form(&[("grant_type", "client_credentials")])
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status(), StatusCode::OK);
}
