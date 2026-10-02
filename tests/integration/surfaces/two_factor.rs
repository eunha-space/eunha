//! Two-factor authentication: setting it up, signing in with it, the role
//! that requires it, and security keys.

use base64::Engine as _;
use reqwest::StatusCode;
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};

use crate::helpers::{user_id_for, TestContext};

const PASSWORD: &str = "testpassword123";

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The hidden `attempt` a sign-in page carries.
fn attempt_token(page: &str) -> String {
    let marker = "name=\"attempt\" value=\"";
    let start = page.find(marker).expect("a pending sign-in") + marker.len();
    let end = page[start..].find('"').unwrap();
    page[start..start + end].to_string()
}

/// Register an OAuth app and return its client id.
async fn register_app(ctx: &TestContext) -> String {
    let app: Value = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({ "client_name": "2fa test", "redirect_uris": "https://client.example/cb", "scopes": "read write" }),
        )
        .await
        .json()
        .await
        .unwrap();
    app["client_id"].as_str().unwrap().to_string()
}

async fn authorize(
    ctx: &TestContext,
    client_id: &str,
    fields: &[(&str, &str)],
) -> reqwest::Response {
    let mut form = vec![
        ("client_id", client_id),
        ("redirect_uri", "https://client.example/cb"),
        ("scope", "read"),
    ];
    form.extend_from_slice(fields);
    ctx.api.post_form("/oauth/authorize", None, &form).await
}

/// Turn on TOTP for a user through the API; returns the secret and the
/// recovery codes.
async fn enable_totp(ctx: &TestContext, token: &str) -> (String, Vec<String>) {
    let setup: Value = ctx
        .api
        .post_json(
            "/api/eunha/v1/two_factor_authentication/otp",
            Some(token),
            &json!({ "password": PASSWORD }),
        )
        .await
        .json()
        .await
        .unwrap();
    let secret = setup["secret"].as_str().unwrap().to_string();
    let code = eunha::two_factor::totp_at(&secret, now()).unwrap();
    let confirmed = ctx
        .api
        .post_json(
            "/api/eunha/v1/two_factor_authentication/otp/confirm",
            Some(token),
            &json!({ "otp_attempt": code }),
        )
        .await;
    assert_eq!(confirmed.status(), StatusCode::OK);
    let body: Value = confirmed.json().await.unwrap();
    let codes = body["recovery_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    (secret, codes)
}

/// Forget the consumed time step, so the current code is good again: what
/// waiting thirty seconds would do.
async fn forget_consumed_step(ctx: &TestContext, user_id: i64) {
    sqlx::query("UPDATE users SET consumed_timestep = NULL WHERE id = $1")
        .bind(user_id)
        .execute(&ctx.db)
        .await
        .unwrap();
}

#[tokio::test]
async fn test_totp_setup_stores_what_mastodon_stores() {
    let ctx = TestContext::new("2fa-setup").await;
    let alice_user = user_id_for(&ctx.db, ctx.alice_id.parse().unwrap()).await;

    let status: Value = ctx
        .api
        .get(
            "/api/eunha/v1/two_factor_authentication",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(status["otp_enabled"], false);
    assert_eq!(status["available"], true);

    // `ChallengableConcern`: the password first.
    let wrong = ctx
        .api
        .post_json(
            "/api/eunha/v1/two_factor_authentication/otp",
            Some(&ctx.alice_token),
            &json!({ "password": "wrong" }),
        )
        .await;
    assert_eq!(wrong.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let setup: Value = ctx
        .api
        .post_json(
            "/api/eunha/v1/two_factor_authentication/otp",
            Some(&ctx.alice_token),
            &json!({ "password": PASSWORD }),
        )
        .await
        .json()
        .await
        .unwrap();
    let secret = setup["secret"].as_str().unwrap();
    assert_eq!(secret.len(), 52, "ROTP::Base32.random(32)");
    assert!(setup["provisioning_uri"]
        .as_str()
        .unwrap()
        .starts_with(&format!(
            "otpauth://totp/{}:alice%40test.invalid?secret={secret}",
            ctx.domain
        )));
    assert!(setup["qr_code"].as_str().unwrap().contains("<svg"));

    let bad = ctx
        .api
        .post_json(
            "/api/eunha/v1/two_factor_authentication/otp/confirm",
            Some(&ctx.alice_token),
            &json!({ "otp_attempt": "000000" }),
        )
        .await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let code = eunha::two_factor::totp_at(secret, now()).unwrap();
    let confirmed: Value = ctx
        .api
        .post_json(
            "/api/eunha/v1/two_factor_authentication/otp/confirm",
            Some(&ctx.alice_token),
            &json!({ "otp_attempt": code }),
        )
        .await
        .json()
        .await
        .unwrap();
    let codes = confirmed["recovery_codes"].as_array().unwrap();
    assert_eq!(codes.len(), 10);
    assert!(codes.iter().all(|c| c.as_str().unwrap().len() == 16
        && c.as_str().unwrap().chars().all(|ch| ch.is_ascii_hexdigit())));

    // `encrypts :otp_secret`, and bcrypt digests of the recovery codes.
    let (required, sealed, digests, consumed): (bool, String, Vec<String>, Option<i32>) =
        sqlx::query_as(
            "SELECT otp_required_for_login, otp_secret, otp_backup_codes, consumed_timestep
             FROM users WHERE id = $1",
        )
        .bind(alice_user)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert!(required);
    assert!(sealed.starts_with("{\"p\":"), "a Rails encryption envelope");
    assert_eq!(
        ctx.state
            .encryptor
            .as_ref()
            .unwrap()
            .decrypt(&sealed)
            .unwrap(),
        secret
    );
    assert_eq!(digests.len(), 10);
    assert!(digests.iter().all(|d| d.starts_with("$2")));
    assert!(bcrypt::verify(codes[0].as_str().unwrap(), &digests[0]).unwrap());
    let step = eunha::two_factor::timestep(now()) as i32;
    assert!(
        consumed.is_some_and(|c| c == step || c == step - 1),
        "the confirming code is consumed"
    );

    // New recovery codes replace the old ones.
    let regenerated: Value = ctx
        .api
        .post_json(
            "/api/eunha/v1/two_factor_authentication/recovery_codes",
            Some(&ctx.alice_token),
            &json!({ "password": PASSWORD }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(regenerated["recovery_codes"].as_array().unwrap().len(), 10);
    assert_ne!(regenerated["recovery_codes"][0], codes[0]);

    // Turning it off needs the password, and clears it all.
    let refused = ctx
        .api
        .delete_json(
            "/api/eunha/v1/two_factor_authentication",
            &ctx.alice_token,
            &json!({ "password": "wrong" }),
        )
        .await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let off: Value = ctx
        .api
        .delete_json(
            "/api/eunha/v1/two_factor_authentication",
            &ctx.alice_token,
            &json!({ "password": PASSWORD }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(off["otp_enabled"], false);
    let (required, sealed): (bool, Option<String>) =
        sqlx::query_as("SELECT otp_required_for_login, otp_secret FROM users WHERE id = $1")
            .bind(alice_user)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(!required);
    assert!(sealed.is_none());
}

#[tokio::test]
async fn test_sign_in_asks_for_the_second_factor() {
    let ctx = TestContext::new("2fa-sign-in").await;
    let alice_user = user_id_for(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    sqlx::query("UPDATE users SET time_zone = 'Asia/Seoul' WHERE id = $1")
        .bind(alice_user)
        .execute(&ctx.db)
        .await
        .unwrap();
    let (secret, codes) = enable_totp(&ctx, &ctx.alice_token).await;
    let client_id = register_app(&ctx).await;

    // The password alone gets the second-factor form, not a code.
    let page = authorize(
        &ctx,
        &client_id,
        &[("email", "alice@test.invalid"), ("password", PASSWORD)],
    )
    .await;
    assert_eq!(page.status(), StatusCode::OK);
    let body = page.text().await.unwrap();
    assert!(body.contains("name=\"otp_attempt\""));
    let attempt = attempt_token(&body);

    // A wrong code is refused and recorded, as `on_authentication_failure` does.
    let wrong = authorize(
        &ctx,
        &client_id,
        &[("attempt", &attempt), ("otp_attempt", "123456")],
    )
    .await;
    assert_eq!(wrong.status(), StatusCode::OK);
    assert!(wrong
        .text()
        .await
        .unwrap()
        .contains("Invalid two-factor code"));
    let failures: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM login_activities WHERE user_id = $1 AND NOT success
           AND authentication_method = 'otp' AND failure_reason = 'invalid_otp_token'",
    )
    .bind(alice_user)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(failures, 1);
    // `UserMailer#failed_2fa`, with the time in the user's own zone.
    let mail = ctx
        .mail_to("alice@test.invalid", "Second factor authentication failure")
        .await
        .expect("a failed second factor mail");
    assert!(mail.html.contains(" KST"), "{}", mail.html);

    // A recovery code works once.
    let granted = authorize(
        &ctx,
        &client_id,
        &[("attempt", &attempt), ("otp_attempt", &codes[0])],
    )
    .await;
    assert_eq!(granted.status(), StatusCode::SEE_OTHER);
    assert!(granted.headers()["location"]
        .to_str()
        .unwrap()
        .starts_with("https://client.example/cb?code="));
    let remaining: Vec<String> =
        sqlx::query_scalar("SELECT otp_backup_codes FROM users WHERE id = $1")
            .bind(alice_user)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(remaining.len(), 9);

    // The pending sign-in is gone once used.
    let replay = authorize(
        &ctx,
        &client_id,
        &[("attempt", &attempt), ("otp_attempt", &codes[1])],
    )
    .await;
    assert!(replay
        .text()
        .await
        .unwrap()
        .contains("Your session expired"));

    // The account pages ask too, and a TOTP code finishes the sign-in.
    let page = ctx
        .api
        .post_form(
            "/account/login",
            None,
            &[("email", "alice@test.invalid"), ("password", PASSWORD)],
        )
        .await;
    let attempt = attempt_token(&page.text().await.unwrap());
    forget_consumed_step(&ctx, alice_user).await;
    let code = eunha::two_factor::totp_at(&secret, now()).unwrap();
    let signed_in = ctx
        .api
        .post_form(
            "/account/login",
            None,
            &[("attempt", &attempt), ("otp_attempt", &code)],
        )
        .await;
    assert_eq!(signed_in.status(), StatusCode::SEE_OTHER);
    assert_eq!(signed_in.headers()["location"], "/account");
    assert!(signed_in.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .starts_with("account_session="));
    let otp_successes: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM login_activities WHERE user_id = $1 AND success
           AND authentication_method = 'otp'",
    )
    .bind(alice_user)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(otp_successes, 2);

    // The same code cannot be used twice.
    let page = ctx
        .api
        .post_form(
            "/account/login",
            None,
            &[("email", "alice@test.invalid"), ("password", PASSWORD)],
        )
        .await;
    let attempt = attempt_token(&page.text().await.unwrap());
    let reused = ctx
        .api
        .post_form(
            "/account/login",
            None,
            &[("attempt", &attempt), ("otp_attempt", &code)],
        )
        .await;
    assert_eq!(reused.status(), StatusCode::OK);

    // There is no password grant to go around the second factor with.
    let app: Value = ctx
        .api
        .post_json(
            "/api/v1/apps",
            None,
            &json!({ "client_name": "pw", "redirect_uris": "urn:ietf:wg:oauth:2.0:oob", "scopes": "read" }),
        )
        .await
        .json()
        .await
        .unwrap();
    let grant = ctx
        .api
        .post_form(
            "/oauth/token",
            None,
            &[
                ("grant_type", "password"),
                ("client_id", app["client_id"].as_str().unwrap()),
                ("client_secret", app["client_secret"].as_str().unwrap()),
                ("username", "alice@test.invalid"),
                ("password", PASSWORD),
            ],
        )
        .await;
    assert_eq!(grant.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_second_factor_attempts_are_rate_limited() {
    let ctx = TestContext::new("2fa-rate-limit").await;
    enable_totp(&ctx, &ctx.alice_token).await;
    let client_id = register_app(&ctx).await;
    let page = authorize(
        &ctx,
        &client_id,
        &[("email", "alice@test.invalid"), ("password", PASSWORD)],
    )
    .await;
    let attempt = attempt_token(&page.text().await.unwrap());
    let mut last = String::new();
    for _ in 0..10 {
        last = authorize(
            &ctx,
            &client_id,
            &[("attempt", &attempt), ("otp_attempt", "000001")],
        )
        .await
        .text()
        .await
        .unwrap();
    }
    assert!(last.contains("Too many authentication attempts"));
}

#[tokio::test]
async fn test_a_role_requiring_two_factor_forces_setup() {
    let ctx = TestContext::new("2fa-required").await;
    let bob_account: i64 = ctx.bob_id.parse().unwrap();
    let bob_user = user_id_for(&ctx.db, bob_account).await;
    let role_id: i64 = sqlx::query_scalar(
        "INSERT INTO user_roles (id, name, position, permissions, require_2fa, created_at, updated_at)
         VALUES (5000, 'Careful', 10, 0, true, now(), now()) RETURNING id",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query("UPDATE users SET role_id = $1 WHERE id = $2")
        .bind(role_id)
        .bind(bob_user)
        .execute(&ctx.db)
        .await
        .unwrap();

    // `require_user!` refuses the API until it is set up.
    let refused = ctx
        .api
        .get("/api/v1/accounts/verify_credentials", Some(&ctx.bob_token))
        .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let status: Value = ctx
        .api
        .get(
            "/api/eunha/v1/two_factor_authentication",
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(status["required"], true);

    // Signing in leads to the setup before the authorization.
    let client_id = register_app(&ctx).await;
    let page = authorize(
        &ctx,
        &client_id,
        &[("email", "bob@test.invalid"), ("password", PASSWORD)],
    )
    .await;
    let body = page.text().await.unwrap();
    assert!(body.contains("requires you to set up Two-Factor Authentication"));
    assert!(body.contains("<svg"));
    let attempt = attempt_token(&body);
    let marker = "<p class=\"secret\">";
    let start = body.find(marker).unwrap() + marker.len();
    let secret = body[start..start + body[start..].find('<').unwrap()].to_string();

    let wrong = authorize(
        &ctx,
        &client_id,
        &[("attempt", &attempt), ("setup_otp_attempt", "000000")],
    )
    .await;
    assert!(wrong
        .text()
        .await
        .unwrap()
        .contains("The entered code was invalid"));

    let code = eunha::two_factor::totp_at(&secret, now()).unwrap();
    let codes_page = authorize(
        &ctx,
        &client_id,
        &[("attempt", &attempt), ("setup_otp_attempt", &code)],
    )
    .await;
    let body = codes_page.text().await.unwrap();
    assert!(body.contains("Resume application authorization"));
    assert_eq!(body.matches("<li>").count(), 10);

    let resumed = authorize(&ctx, &client_id, &[("attempt", &attempt), ("resume", "1")]).await;
    assert_eq!(resumed.status(), StatusCode::SEE_OTHER);
    assert!(resumed.headers()["location"]
        .to_str()
        .unwrap()
        .contains("code="));

    let ok = ctx
        .api
        .get("/api/v1/accounts/verify_credentials", Some(&ctx.bob_token))
        .await;
    assert_eq!(
        ok.status(),
        StatusCode::OK,
        "set up, the account works again"
    );
}

// ── Security keys ───────────────────────────────────────────────────────────

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn b64d(text: &str) -> Vec<u8> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text)
        .unwrap()
}

fn cose_key(key: &p256::ecdsa::SigningKey) -> Vec<u8> {
    use eunha::webauthn::cbor::Value as C;
    let point = key.verifying_key().to_encoded_point(false);
    eunha::webauthn::cbor::encode(&C::Map(vec![
        (C::Int(1), C::Int(2)),
        (C::Int(3), C::Int(-7)),
        (C::Int(-1), C::Int(1)),
        (C::Int(-2), C::Bytes(point.x().unwrap().to_vec())),
        (C::Int(-3), C::Bytes(point.y().unwrap().to_vec())),
    ]))
}

fn authenticator_data(
    domain: &str,
    flags: u8,
    count: u32,
    attested: Option<(&[u8], &[u8])>,
) -> Vec<u8> {
    let mut out = Sha256::digest(domain.as_bytes()).to_vec();
    out.push(flags);
    out.extend_from_slice(&count.to_be_bytes());
    if let Some((id, key)) = attested {
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(&(id.len() as u16).to_be_bytes());
        out.extend_from_slice(id);
        out.extend_from_slice(key);
    }
    out
}

fn client_data(kind: &str, challenge: &str, domain: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "type": kind,
        "challenge": challenge,
        "origin": format!("https://{domain}"),
    }))
    .unwrap()
}

#[tokio::test]
async fn test_security_keys_register_and_sign_in() {
    use eunha::webauthn::cbor::Value as C;
    use p256::ecdsa::signature::Signer as _;

    let ctx = TestContext::new("2fa-webauthn").await;
    let alice_user = user_id_for(&ctx.db, ctx.alice_id.parse().unwrap()).await;

    // Security keys come after an authenticator app.
    let early = ctx
        .api
        .post_json(
            "/api/eunha/v1/two_factor_authentication/webauthn_credentials/options",
            Some(&ctx.alice_token),
            &json!({ "password": PASSWORD }),
        )
        .await;
    assert_eq!(early.status(), StatusCode::UNPROCESSABLE_ENTITY);
    enable_totp(&ctx, &ctx.alice_token).await;

    let options: Value = ctx
        .api
        .post_json(
            "/api/eunha/v1/two_factor_authentication/webauthn_credentials/options",
            Some(&ctx.alice_token),
            &json!({ "password": PASSWORD }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(options["rp"]["name"], "Mastodon");
    assert_eq!(options["user"]["name"], "alice");
    assert_eq!(
        options["authenticatorSelection"]["userVerification"],
        "discouraged"
    );
    let handle: String = sqlx::query_scalar("SELECT webauthn_id FROM users WHERE id = $1")
        .bind(alice_user)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(options["user"]["id"], handle.as_str());
    assert_eq!(b64d(&handle).len(), 64);

    let key = p256::ecdsa::SigningKey::random(&mut p256::elliptic_curve::rand_core::OsRng);
    let credential_id = b"alice-key";
    let challenge = options["challenge"].as_str().unwrap();
    let attestation = eunha::webauthn::cbor::encode(&C::Map(vec![
        (C::Text("fmt".into()), C::Text("none".into())),
        (C::Text("attStmt".into()), C::Map(vec![])),
        (
            C::Text("authData".into()),
            C::Bytes(authenticator_data(
                &ctx.domain,
                0x41,
                0,
                Some((credential_id, &cose_key(&key))),
            )),
        ),
    ]));
    let created = ctx
        .api
        .post_json(
            "/api/eunha/v1/two_factor_authentication/webauthn_credentials",
            Some(&ctx.alice_token),
            &json!({
                "nickname": "Yubikey",
                "credential": {
                    "id": b64(credential_id),
                    "rawId": b64(credential_id),
                    "type": "public-key",
                    "response": {
                        "clientDataJSON": b64(&client_data("webauthn.create", challenge, &ctx.domain)),
                        "attestationObject": b64(&attestation),
                    },
                },
            }),
        )
        .await;
    assert_eq!(created.status(), StatusCode::OK);
    let (external_id, nickname): (String, String) =
        sqlx::query_as("SELECT external_id, nickname FROM webauthn_credentials WHERE user_id = $1")
            .bind(alice_user)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(external_id, b64(credential_id));
    assert_eq!(nickname, "Yubikey");

    // Signing in offers the key, and a signed assertion finishes it.
    let client_id = register_app(&ctx).await;
    let page = authorize(
        &ctx,
        &client_id,
        &[("email", "alice@test.invalid"), ("password", PASSWORD)],
    )
    .await
    .text()
    .await
    .unwrap();
    assert!(page.contains("id=\"webauthn-form\""));
    let attempt = attempt_token(&page);
    let options: Value = ctx
        .api
        .post_form(
            "/auth/sessions/security_key_options",
            None,
            &[("attempt", &attempt)],
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(options["allowCredentials"][0]["id"], b64(credential_id));
    let challenge = options["challenge"].as_str().unwrap();
    let data = authenticator_data(&ctx.domain, 0x01, 1, None);
    let client = client_data("webauthn.get", challenge, &ctx.domain);
    let mut signed = data.clone();
    signed.extend_from_slice(&Sha256::digest(&client));
    let signature: p256::ecdsa::Signature = key.sign(&signed);
    let assertion = json!({
        "id": b64(credential_id),
        "rawId": b64(credential_id),
        "type": "public-key",
        "response": {
            "clientDataJSON": b64(&client),
            "authenticatorData": b64(&data),
            "signature": b64(signature.to_der().as_bytes()),
        },
    })
    .to_string();
    let granted = authorize(
        &ctx,
        &client_id,
        &[("attempt", &attempt), ("credential", &assertion)],
    )
    .await;
    assert_eq!(granted.status(), StatusCode::SEE_OTHER);
    let count: i64 =
        sqlx::query_scalar("SELECT sign_count FROM webauthn_credentials WHERE user_id = $1")
            .bind(alice_user)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(count, 1);
    let method: String = sqlx::query_scalar(
        "SELECT authentication_method FROM login_activities WHERE user_id = $1 AND success ORDER BY id DESC LIMIT 1",
    )
    .bind(alice_user)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(method, "webauthn");

    // Removing the key needs the password.
    let id: i64 = sqlx::query_scalar("SELECT id FROM webauthn_credentials WHERE user_id = $1")
        .bind(alice_user)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let path = format!("/api/eunha/v1/two_factor_authentication/webauthn_credentials/{id}");
    let refused = ctx
        .api
        .delete_json(&path, &ctx.alice_token, &json!({ "password": "nope" }))
        .await;
    assert_eq!(refused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let removed: Value = ctx
        .api
        .delete_json(&path, &ctx.alice_token, &json!({ "password": PASSWORD }))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(removed["webauthn_enabled"], false);
}
