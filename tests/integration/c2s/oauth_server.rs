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

const PASSWORD: &str = "testpassword123";
const CALLBACK: &str = "https://client.example/cb";

/// Register an app with these redirect URIs; its id and secret.
async fn register_app(ctx: &TestContext, redirect_uris: &str) -> (String, String) {
    let app: Value = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &serde_json::json!({
                "client_name": "oauth flow",
                "redirect_uris": redirect_uris,
                "scopes": "read write",
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    (
        app["client_id"].as_str().unwrap().to_owned(),
        app["client_secret"].as_str().unwrap().to_owned(),
    )
}

/// Press the authorization page's authorize button as alice, signed in,
/// with these fields besides the client's.
async fn authorize(
    ctx: &TestContext,
    client_id: &str,
    redirect_uri: &str,
    fields: &[(&str, &str)],
) -> reqwest::Response {
    let cookie =
        crate::helpers::account_session_cookie(&ctx.api, "alice@test.invalid", PASSWORD).await;
    let mut form = vec![
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("scope", "read"),
    ];
    form.extend_from_slice(fields);
    crate::helpers::approve_authorization(&ctx.api, &cookie, &form).await
}

fn location(resp: &reqwest::Response) -> url::Url {
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .expect("a redirect");
    url::Url::options()
        .base_url(Some(&url::Url::parse("https://base.invalid").unwrap()))
        .parse(location)
        .unwrap()
}

fn param(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

#[tokio::test]
async fn the_code_carries_the_state_and_wants_the_pkce_verifier() {
    let ctx = TestContext::new("oauth-pkce").await;
    let (client_id, client_secret) = register_app(&ctx, CALLBACK).await;
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    // The page keeps what the request asked with.
    let page = ctx
        .api
        .get(
            &format!(
                "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}&scope=read&state=xyz&code_challenge={challenge}&code_challenge_method=S256",
                urlencoding::encode(CALLBACK)
            ),
            None,
        )
        .await;
    assert_eq!(page.status(), StatusCode::OK);
    let page = page.text().await.unwrap();
    assert!(page.contains(r#"name="state" value="xyz""#));
    assert!(page.contains(&format!(r#"name="code_challenge" value="{challenge}""#)));

    let granted = authorize(
        &ctx,
        &client_id,
        CALLBACK,
        &[
            ("state", "xyz"),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ],
    )
    .await;
    assert!(granted.status().is_redirection());
    let to = location(&granted);
    assert_eq!(to.as_str().split('?').next(), Some(CALLBACK));
    assert_eq!(param(&to, "state").as_deref(), Some("xyz"));
    let code = param(&to, "code").expect("a code");

    let exchange = |verifier: Option<&str>| {
        let mut form = vec![
            ("grant_type", "authorization_code".to_owned()),
            ("code", code.clone()),
            ("redirect_uri", CALLBACK.to_owned()),
        ];
        if let Some(v) = verifier {
            form.push(("code_verifier", v.to_owned()));
        }
        ctx.api
            .http
            .post(ctx.api.url("/oauth/token"))
            .header("host", &ctx.api.host)
            // `client_secret_basic`.
            .basic_auth(&client_id, Some(&client_secret))
            .form(&form)
            .send()
    };
    let missing = exchange(None).await.unwrap();
    assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    let body: Value = missing.json().await.unwrap();
    assert_eq!(body["error"], "invalid_request");
    let wrong = exchange(Some("not-the-verifier")).await.unwrap();
    assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
    let body: Value = wrong.json().await.unwrap();
    assert_eq!(body["error"], "invalid_grant");
    let right = exchange(Some(verifier)).await.unwrap();
    assert_eq!(right.status(), StatusCode::OK);
    let body: Value = right.json().await.unwrap();
    assert!(body["access_token"].is_string());
}

#[tokio::test]
async fn response_modes_and_the_out_of_band_page() {
    let ctx = TestContext::new("oauth-modes").await;
    let (client_id, _) =
        register_app(&ctx, &format!("{CALLBACK}\nurn:ietf:wg:oauth:2.0:oob")).await;

    let granted = authorize(
        &ctx,
        &client_id,
        CALLBACK,
        &[("response_mode", "fragment"), ("state", "s1")],
    )
    .await;
    let to = location(&granted);
    assert!(to.query().is_none());
    let fragment = to.fragment().unwrap();
    assert!(fragment.starts_with("code="), "{fragment}");
    assert!(fragment.ends_with("&state=s1"), "{fragment}");

    let posted = authorize(
        &ctx,
        &client_id,
        CALLBACK,
        &[("response_mode", "form_post")],
    )
    .await;
    assert_eq!(posted.status(), StatusCode::OK);
    let page = posted.text().await.unwrap();
    assert!(page.contains(&format!(r#"action="{CALLBACK}""#)));
    assert!(page.contains(r#"name="code""#));

    // `oob_redirect`: the code is shown to copy.
    let granted = authorize(&ctx, &client_id, "urn:ietf:wg:oauth:2.0:oob", &[]).await;
    let to = location(&granted);
    assert_eq!(to.path(), "/oauth/authorize/native");
    let code = param(&to, "code").unwrap();
    let page = ctx
        .api
        .get(&format!("/oauth/authorize/native?code={code}"), None)
        .await;
    assert_eq!(page.status(), StatusCode::OK);
    assert!(page.text().await.unwrap().contains(&code));
}

#[tokio::test]
async fn the_authorization_page_refuses_what_doorkeeper_refuses() {
    let ctx = TestContext::new("oauth-refusals").await;
    let (client_id, _) = register_app(&ctx, CALLBACK).await;
    let base = format!(
        "/oauth/authorize?client_id={client_id}&redirect_uri={}&scope=read",
        urlencoding::encode(CALLBACK)
    );
    for query in [
        "",
        "&response_type=token",
        "&response_type=code&response_mode=web_message",
        "&response_type=code&code_challenge=abc&code_challenge_method=plain",
        "&response_type=code&code_challenge=abc",
    ] {
        let resp = ctx.api.get(&format!("{base}{query}"), None).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{query}");
    }
}

#[tokio::test]
async fn denying_sends_access_denied_to_the_client() {
    let ctx = TestContext::new("oauth-deny").await;
    let (client_id, _) = register_app(&ctx, CALLBACK).await;
    let path = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}&scope=read&state=xyz",
        urlencoding::encode(CALLBACK)
    );
    let deny = |cookie: Option<String>, path: String| {
        let mut req = ctx
            .api
            .http
            .delete(ctx.api.url(&path))
            .header("host", &ctx.api.host);
        if let Some(cookie) = cookie {
            req = req.header("cookie", cookie);
        }
        req.send()
    };

    // `authenticate_resource_owner!`.
    let resp = deny(None, path.clone()).await.unwrap();
    assert!(resp.status().is_redirection());
    assert_eq!(location(&resp).path(), "/account/login");

    let cookie =
        crate::helpers::account_session_cookie(&ctx.api, "alice@test.invalid", PASSWORD).await;
    let resp = deny(Some(cookie.clone()), path).await.unwrap();
    assert!(resp.status().is_redirection());
    let to = location(&resp);
    assert_eq!(to.as_str().split('?').next(), Some(CALLBACK));
    assert_eq!(param(&to, "error").as_deref(), Some("access_denied"));
    assert_eq!(param(&to, "state").as_deref(), Some("xyz"));
    assert!(param(&to, "error_description").is_some());

    let elsewhere = format!(
        "/oauth/authorize?client_id={client_id}&redirect_uri={}",
        urlencoding::encode("https://evil.example/")
    );
    let resp = deny(Some(cookie), elsewhere).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// `Oauth::AuthorizationsController#new` for a signed-in user: the
/// authorize-or-deny page, until a confidential client holds a token with
/// the same scopes, when the code comes at once (`can_authorize_response?`)
/// unless `force_login` says to ask; the instance's own app never asks
/// (`skip_authorization`). Both buttons answer with a `302`.
#[tokio::test]
async fn the_page_asks_until_a_matching_token_exists() {
    let ctx = TestContext::new("oauth-consent").await;
    let (client_id, client_secret) = register_app(&ctx, CALLBACK).await;
    let cookie =
        crate::helpers::account_session_cookie(&ctx.api, "alice@test.invalid", PASSWORD).await;
    let page_path = |extra: &str| {
        format!(
            "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}&scope=read&state=s{extra}",
            urlencoding::encode(CALLBACK)
        )
    };
    let get = |path: String, cookie: Option<String>| {
        let mut req = ctx
            .api
            .http
            .get(ctx.api.url(&path))
            .header("host", &ctx.api.host);
        if let Some(cookie) = cookie {
            req = req.header("cookie", cookie);
        }
        req.send()
    };

    // Signed out: the page asks to sign in first.
    let signed_out = get(page_path(""), None).await.unwrap();
    assert_eq!(signed_out.status(), StatusCode::OK);
    assert!(signed_out
        .text()
        .await
        .unwrap()
        .contains(r#"name="password""#));

    // Signed in: authorize or deny.
    let page = get(page_path(""), Some(cookie.clone())).await.unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    let page = page.text().await.unwrap();
    assert!(page.contains("Authorization required"), "{page}");
    assert!(page.contains("Review permissions"));
    assert!(page.contains("Full access to your account"));
    assert!(page.contains("Read-only access"));
    assert!(page.contains(r#"name="_method" value="delete""#));
    assert!(page.contains(">Deny<"));
    assert!(page.contains(">Authorize<"));
    assert!(page.contains("@alice@"));

    // Deny, as the page's form sends it.
    let denied = crate::helpers::approve_authorization(
        &ctx.api,
        &cookie,
        &[
            ("_method", "delete"),
            ("client_id", &client_id),
            ("redirect_uri", CALLBACK),
            ("scope", "read"),
            ("state", "s"),
        ],
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FOUND);
    assert_eq!(
        param(&location(&denied), "error").as_deref(),
        Some("access_denied")
    );

    // Authorize: a `302` with the code.
    let granted = crate::helpers::approve_authorization(
        &ctx.api,
        &cookie,
        &[
            ("client_id", &client_id),
            ("redirect_uri", CALLBACK),
            ("scope", "read"),
            ("state", "s"),
        ],
    )
    .await;
    assert_eq!(granted.status(), StatusCode::FOUND);
    let code = param(&location(&granted), "code").expect("a code");
    let token = ctx
        .api
        .post_form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("redirect_uri", CALLBACK),
                ("client_id", &client_id),
                ("client_secret", &client_secret),
            ],
        )
        .await;
    assert_eq!(token.status(), StatusCode::OK);

    // A matching token: the code at once.
    let at_once = get(page_path(""), Some(cookie.clone())).await.unwrap();
    assert_eq!(at_once.status(), StatusCode::FOUND);
    let to = location(&at_once);
    assert_eq!(to.as_str().split('?').next(), Some(CALLBACK));
    assert!(param(&to, "code").is_some());
    assert_eq!(param(&to, "state").as_deref(), Some("s"));

    // Unless `force_login`, or other scopes, or a public client.
    let forced = get(page_path("&force_login=true"), Some(cookie.clone()))
        .await
        .unwrap();
    assert_eq!(forced.status(), StatusCode::OK);
    let not_forced = get(page_path("&force_login=false"), Some(cookie.clone()))
        .await
        .unwrap();
    assert_eq!(not_forced.status(), StatusCode::FOUND);
    let other_scopes = get(
        page_path("").replace("scope=read", "scope=read+write"),
        Some(cookie.clone()),
    )
    .await
    .unwrap();
    assert_eq!(other_scopes.status(), StatusCode::OK);
    sqlx::query("UPDATE oauth_applications SET confidential = false WHERE uid = $1")
        .bind(&client_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    let public = get(page_path(""), Some(cookie.clone())).await.unwrap();
    assert_eq!(public.status(), StatusCode::OK);

    // The instance's own app never asks, `force_login` or not.
    sqlx::query("UPDATE oauth_applications SET superapp = true WHERE uid = $1")
        .bind(&client_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    let superapp = get(page_path("&force_login=true"), Some(cookie))
        .await
        .unwrap();
    assert_eq!(superapp.status(), StatusCode::FOUND);
    assert!(param(&location(&superapp), "code").is_some());
}

/// `require_functional!` on the authorization page, for a browser already
/// signed in: a user pending approval goes to the account page, and one
/// whose role now requires two-factor authentication is set up first; the
/// authorize button and deny do the same, and nobody gets a code.
#[tokio::test]
async fn the_page_requires_a_functional_user() {
    let ctx = TestContext::new("oauth-functional").await;
    let (client_id, _) = register_app(&ctx, CALLBACK).await;
    let cookie =
        crate::helpers::account_session_cookie(&ctx.api, "alice@test.invalid", PASSWORD).await;
    let alice_user =
        crate::helpers::user_id_for(&ctx.db, ctx.alice_id.parse::<i64>().unwrap()).await;
    let path = format!(
        "/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}&scope=read",
        urlencoding::encode(CALLBACK)
    );
    let get = |cookie: String, path: String| {
        let api = &ctx.api;
        async move {
            api.http
                .get(api.url(&path))
                .header("host", &api.host)
                .header("cookie", cookie)
                .send()
                .await
                .unwrap()
        }
    };
    let form = [
        ("client_id", client_id.as_str()),
        ("redirect_uri", CALLBACK),
        ("scope", "read"),
    ];

    sqlx::query("UPDATE users SET approved = false WHERE id = $1")
        .bind(alice_user)
        .execute(&ctx.db)
        .await
        .unwrap();
    let pending = get(cookie.clone(), path.clone()).await;
    assert_eq!(pending.status(), StatusCode::FOUND);
    assert_eq!(location(&pending).path(), "/account");
    let approve = crate::helpers::approve_authorization(&ctx.api, &cookie, &form).await;
    assert_eq!(location(&approve).path(), "/account");
    let mut deny_form = form.to_vec();
    deny_form.push(("_method", "delete"));
    let deny = crate::helpers::approve_authorization(&ctx.api, &cookie, &deny_form).await;
    assert_eq!(location(&deny).path(), "/account");

    sqlx::query("UPDATE users SET approved = true WHERE id = $1")
        .bind(alice_user)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query("UPDATE user_roles SET require_2fa = true WHERE id = -99")
        .execute(&ctx.db)
        .await
        .unwrap();
    let setup = get(cookie.clone(), path.clone()).await;
    assert_eq!(setup.status(), StatusCode::OK);
    let page = setup.text().await.unwrap();
    assert!(page.contains(r#"name="setup_otp_attempt""#), "{page}");

    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_access_grants")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(grants, 0);
}

/// Signing in on the authorization page starts a session and comes back to
/// the page with a `302`; signing out from the page with `continue` comes
/// back to it signed out.
#[tokio::test]
async fn signing_in_on_the_page_comes_back_to_it() {
    let ctx = TestContext::new("oauth-sign-in-back").await;
    let (client_id, _) = register_app(&ctx, CALLBACK).await;
    let signed_in = ctx
        .api
        .post_form(
            "/oauth/authorize",
            None,
            &[
                ("client_id", &client_id),
                ("redirect_uri", CALLBACK),
                ("scope", "read"),
                ("state", "xyz"),
                ("force_login", "true"),
                ("email", "alice@test.invalid"),
                ("password", PASSWORD),
            ],
        )
        .await;
    assert_eq!(signed_in.status(), StatusCode::FOUND);
    let back = location(&signed_in);
    assert_eq!(back.path(), "/oauth/authorize");
    assert_eq!(param(&back, "state").as_deref(), Some("xyz"));
    assert_eq!(param(&back, "force_login").as_deref(), Some("true"));
    assert!(crate::helpers::session_cookie_of(&signed_in).is_some());
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM oauth_access_grants")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(grants, 0, "the page asks before granting");

    let cookie = crate::helpers::session_cookie_of(&signed_in).unwrap();
    let continue_to = format!("{}?{}", back.path(), back.query().unwrap());
    let signed_out = ctx
        .api
        .http
        .post(ctx.api.url("/account/logout"))
        .header("host", &ctx.api.host)
        .header("cookie", &cookie)
        .form(&[("continue", continue_to.as_str())])
        .send()
        .await
        .unwrap();
    assert_eq!(signed_out.status(), StatusCode::FOUND);
    assert_eq!(location(&signed_out).path(), "/oauth/authorize");
}

/// A client-credentials token of a newly registered app.
async fn client_token(ctx: &TestContext, client_id: &str, client_secret: &str) -> String {
    let body: Value = ctx
        .api
        .post_form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "client_credentials"),
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("scope", "read write"),
            ],
        )
        .await
        .json()
        .await
        .unwrap();
    body["access_token"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn token_info_describes_the_bearer_token() {
    let ctx = TestContext::new("oauth-token-info").await;
    let resp = ctx
        .api
        .get("/oauth/token/info", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["scope"],
        serde_json::json!(["read", "write", "follow", "push"])
    );
    assert!(body["resource_owner_id"].is_number());
    assert!(body["expires_in"].is_null());
    assert!(body["application"]["uid"].is_string());
    assert!(body["created_at"].is_number());

    let resp = ctx.api.get("/oauth/token/info", Some("nonsense")).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().contains_key("www-authenticate"));
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "invalid_token");
    assert_eq!(body["error_description"], "The access token is invalid");

    sqlx::query("UPDATE oauth_access_tokens SET revoked_at = now() WHERE token = $1")
        .bind(&ctx.alice_token)
        .execute(&ctx.db)
        .await
        .unwrap();
    let resp = ctx
        .api
        .get("/oauth/token/info", Some(&ctx.alice_token))
        .await;
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error_description"], "The access token was revoked");
}

#[tokio::test]
async fn introspection_answers_a_client_about_its_own_tokens() {
    let ctx = TestContext::new("oauth-introspect").await;
    let (client_id, client_secret) = register_app(&ctx, CALLBACK).await;
    let token = client_token(&ctx, &client_id, &client_secret).await;

    let introspect = |auth: Option<(&str, &str)>, bearer: Option<&str>, about: &str| {
        let mut req = ctx
            .api
            .http
            .post(ctx.api.url("/oauth/introspect"))
            .header("host", &ctx.api.host)
            .form(&[("token", about.to_owned())]);
        if let Some((id, secret)) = auth {
            req = req.basic_auth(id, Some(secret));
        }
        if let Some(bearer) = bearer {
            req = req.bearer_auth(bearer);
        }
        req.send()
    };

    let resp = introspect(Some((&client_id, &client_secret)), None, &token)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["active"], true);
    assert_eq!(body["scope"], "read write");
    assert_eq!(body["client_id"], client_id.as_str());
    assert_eq!(body["token_type"], "Bearer");
    assert!(body["iat"].is_number());
    assert!(body.get("exp").is_none());

    // Another application's token is none of this client's business.
    let resp = introspect(Some((&client_id, &client_secret)), None, &ctx.alice_token)
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, serde_json::json!({"active": false}));

    // A token of the same application may ask about another of it (with
    // other scopes, or `reuse_access_token` would hand back the same one).
    let other: Value = ctx
        .api
        .post_form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "client_credentials"),
                ("client_id", &client_id),
                ("client_secret", &client_secret),
                ("scope", "read"),
            ],
        )
        .await
        .json()
        .await
        .unwrap();
    let other = other["access_token"].as_str().unwrap().to_owned();
    assert_ne!(other, token);
    let resp = introspect(None, Some(&other), &token).await.unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["active"], true);

    let resp = introspect(Some((&client_id, "wrong")), None, &token)
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = introspect(None, None, &token).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "invalid_request");
}

#[tokio::test]
async fn the_applications_pages_are_closed() {
    let ctx = TestContext::new("oauth-applications").await;
    for path in [
        "/oauth/applications",
        "/oauth/applications/new",
        "/oauth/applications/1",
    ] {
        let resp = ctx.api.get(path, Some(&ctx.alice_token)).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{path}");
    }
}
