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

/// Sign in on the authorization page with these fields besides the client's.
async fn authorize(
    ctx: &TestContext,
    client_id: &str,
    redirect_uri: &str,
    fields: &[(&str, &str)],
) -> reqwest::Response {
    let mut form = vec![
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("scope", "read"),
        ("email", "alice@test.invalid"),
        ("password", PASSWORD),
    ];
    form.extend_from_slice(fields);
    ctx.api.post_form("/oauth/authorize", None, &form).await
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
