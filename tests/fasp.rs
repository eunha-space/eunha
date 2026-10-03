//! Fediverse Auxiliary Service Providers, against a fake provider: its
//! registration and confirmation, capability activation, requests signed both
//! ways, lifecycle and trend announcements, backfill, debug callbacks, and the
//! follow recommendations and account searches eunha asks it for.
//!
//! A test binary of its own because it has to let eunha reach 127.0.0.1,
//! where the fake provider listens. The SSRF guard's allowlist is process-wide
//! and set once, so granting it in the main suite would grant it to every test
//! there.

// Test servers have no tenant span to keep.
#![allow(clippy::disallowed_methods)]

#[allow(dead_code)]
#[path = "integration/helpers.rs"]
mod helpers;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde_json::{json, Value};

use helpers::TestContext;

/// A request the fake provider received.
#[derive(Debug, Clone)]
struct Received {
    method: String,
    path: String,
    query: String,
    body: Value,
    /// Whether eunha's signature on it verified against eunha's key.
    verified: bool,
}

#[derive(Default)]
struct FakeState {
    seed: [u8; 32],
    public: [u8; 32],
    /// eunha's public key for this provider, learnt at registration.
    server_key: Option<[u8; 32]>,
    received: Vec<Received>,
    documents: HashMap<String, Value>,
    search_results: Vec<String>,
    recommendations: Vec<String>,
}

#[derive(Clone)]
struct Fake {
    base: String,
    state: Arc<Mutex<FakeState>>,
}

impl Fake {
    fn received(&self) -> Vec<Received> {
        self.state.lock().unwrap().received.clone()
    }

    /// Waits for a request matching `wanted`, and returns it.
    async fn wait_for(&self, what: &str, wanted: impl Fn(&Received) -> bool) -> Received {
        for _ in 0..200 {
            if let Some(found) = self.received().into_iter().find(|r| wanted(r)) {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "the provider never received {what}; it received {:#?}",
            self.received()
        );
    }

    fn announcements(&self) -> Vec<Value> {
        self.received()
            .into_iter()
            .filter(|r| r.path == "/data_sharing/v0/announcements")
            .map(|r| r.body)
            .collect()
    }

    async fn wait_for_announcement(&self, what: &str, wanted: impl Fn(&Value) -> bool) -> Value {
        self.wait_for(what, |r| {
            r.path == "/data_sharing/v0/announcements" && wanted(&r.body)
        })
        .await
        .body
    }

    fn public_key_base64(&self) -> String {
        BASE64.encode(self.state.lock().unwrap().public)
    }

    fn seed(&self) -> [u8; 32] {
        self.state.lock().unwrap().seed
    }
}

/// A JSON answer signed over `@status` and `content-digest`, as a provider
/// signs one.
fn signed(fake: &Fake, status: StatusCode, body: Option<Value>) -> Response {
    let bytes = body.map(|b| b.to_string()).unwrap_or_default();
    let digest = eunha::fasp::signature::content_digest(bytes.as_bytes());
    let (input, signature) = eunha::fasp::signature::sign_response(
        status.as_u16(),
        &digest,
        &fake.seed(),
        chrono::Utc::now().timestamp(),
    );
    (
        status,
        [
            ("content-type", "application/json".to_owned()),
            ("content-digest", digest),
            ("signature-input", input),
            ("signature", signature),
        ],
        bytes,
    )
        .into_response()
}

async fn serve(
    axum::extract::State(fake): axum::extract::State<Fake>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_owned();
    // WebFinger, which confirms each actor's handle before it is stored.
    if path == "/.well-known/webfinger" {
        let host = fake.base.trim_start_matches("http://");
        let name = uri
            .query()
            .and_then(|q| q.strip_prefix("resource="))
            .map(|r| r.replace("%3A", ":").replace("%40", "@"))
            .and_then(|r| {
                r.strip_prefix("acct:")
                    .and_then(|r| r.strip_suffix(&format!("@{host}")))
                    .map(str::to_owned)
            });
        return match name {
            Some(name) => axum::Json(json!({
                "subject": format!("acct:{name}@{host}"),
                "links": [{
                    "rel": "self",
                    "type": "application/activity+json",
                    "href": format!("{}/users/{name}", fake.base),
                }],
            }))
            .into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    if path.starts_with("/users/") {
        return match fake.state.lock().unwrap().documents.get(&path) {
            Some(document) => (
                [("content-type", "application/activity+json")],
                document.to_string(),
            )
                .into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    let server_key = fake.state.lock().unwrap().server_key;
    let target = format!("{}{}", fake.base, uri.path_and_query().unwrap().as_str());
    let verified = server_key.is_some_and(|key| {
        eunha::fasp::signature::verify_request(
            method.as_str(),
            &target,
            &headers,
            &body,
            &key,
            chrono::Utc::now().timestamp(),
        )
        .is_ok()
            && headers.get("content-digest").and_then(|v| v.to_str().ok())
                == Some(eunha::fasp::signature::content_digest(&body).as_str())
    });
    let received = Received {
        method: method.to_string(),
        path: path.clone(),
        query: uri.query().unwrap_or("").to_owned(),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        verified,
    };
    fake.state.lock().unwrap().received.push(received);
    match (method.as_str(), path.as_str()) {
        ("GET", "/provider_info") => signed(
            &fake,
            StatusCode::OK,
            Some(json!({
                "name": "Fake FASP",
                "privacyPolicy": [{"url": "https://fasp.example/privacy", "language": "en"}],
                "capabilities": [
                    {"id": "account_search", "version": "0.1"},
                    {"id": "follow_recommendation", "version": "0.1"},
                    {"id": "data_sharing", "version": "0.1"},
                    {"id": "callback", "version": "0.1"},
                ],
                "signInUrl": "https://fasp.example/sign_in",
                "contactEmail": "admin@fasp.example",
                "fediverseAccount": "@fasp@fasp.example",
            })),
        ),
        ("GET", "/account_search/v0/search") => {
            let results = fake.state.lock().unwrap().search_results.clone();
            signed(&fake, StatusCode::OK, Some(json!(results)))
        }
        ("GET", "/follow_recommendation/v0/accounts") => {
            let results = fake.state.lock().unwrap().recommendations.clone();
            signed(&fake, StatusCode::OK, Some(json!(results)))
        }
        _ => signed(&fake, StatusCode::NO_CONTENT, None),
    }
}

/// Starts the fake provider, with two actors it can name.
async fn spawn_fake() -> Fake {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let pem = eunha::fasp::keys::generate_private_key_pem().unwrap();
    let (seed, public) = eunha::fasp::keys::parse_private_key_pem(&pem).unwrap();
    let (_, rsa_public) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    let mut documents = HashMap::new();
    for name in ["eve", "frank"] {
        let actor = format!("{base}/users/{name}");
        documents.insert(
            format!("/users/{name}"),
            json!({
                "@context": ["https://www.w3.org/ns/activitystreams", "https://w3id.org/security/v1"],
                "id": actor,
                "type": "Person",
                "preferredUsername": name,
                "webfinger": format!("{name}@{}", base.trim_start_matches("http://")),
                "discoverable": true,
                "inbox": format!("{actor}/inbox"),
                "outbox": format!("{actor}/outbox"),
                "publicKey": {
                    "id": format!("{actor}#main-key"),
                    "owner": actor,
                    "publicKeyPem": rsa_public,
                },
            }),
        );
    }
    let fake = Fake {
        base: base.clone(),
        state: Arc::new(Mutex::new(FakeState {
            seed,
            public,
            documents,
            ..FakeState::default()
        })),
    };
    let app = Router::new().fallback(serve).with_state(fake.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    fake
}

async fn fasp_ctx(label: &str) -> TestContext {
    eunha::federation::webfinger::use_plain_http_for_tests();
    TestContext::with_config(label, |config| {
        config.allowed_private_networks = vec!["127.0.0.0/8".into()];
        config.instance.experimental_features = vec!["fasp".into()];
    })
    .await
}

/// A request the provider signs, as it signs one to eunha: over `@method`,
/// `@target-uri` and `content-digest`, keyed by its id there.
async fn provider_call(
    ctx: &TestContext,
    fake: &Fake,
    method: &str,
    path: &str,
    body: Option<Value>,
    key_id: &str,
) -> reqwest::Response {
    let bytes = body.map(|b| b.to_string()).unwrap_or_default();
    let target = format!("https://{}{}", ctx.domain, path);
    let signed = ojak::sig::rfc9421::sign_request(
        method,
        &target,
        Some(bytes.as_bytes()),
        key_id,
        &ojak::sig::rfc9421::SigningKey::Ed25519(&fake.seed()),
        chrono::Utc::now().timestamp(),
    )
    .unwrap();
    reqwest::Client::new()
        .request(method.parse().unwrap(), ctx.api.url(path))
        .header("host", &ctx.domain)
        .header("content-type", "application/json")
        .header("accept", "application/json")
        .header("content-digest", signed.content_digest.unwrap())
        .header("signature-input", signed.signature_input)
        .header("signature", signed.signature)
        .body(bytes)
        .send()
        .await
        .unwrap()
}

/// Checks that an answer from eunha is signed with its key for the provider.
async fn assert_signed_by_eunha(fake: &Fake, response: reqwest::Response) -> Value {
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.bytes().await.unwrap();
    assert_eq!(
        headers.get("content-digest").unwrap().to_str().unwrap(),
        eunha::fasp::signature::content_digest(&body)
    );
    let key = fake.state.lock().unwrap().server_key.unwrap();
    eunha::fasp::signature::verify_response(status, &headers, &key, chrono::Utc::now().timestamp())
        .expect("eunha's answer should be signed with its key for the provider");
    serde_json::from_slice(&body).unwrap_or(Value::Null)
}

/// Registers the fake provider, confirms it as an administrator, and returns
/// its id at eunha.
async fn register_and_confirm(ctx: &TestContext, fake: &Fake) -> String {
    let response = ctx
        .api
        .post_json(
            "/api/fasp/registration",
            None,
            &json!({
                "name": "Fake FASP",
                "baseUrl": fake.base,
                "serverId": "server-1",
                "publicKey": fake.public_key_base64(),
            }),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let headers = response.headers().clone();
    let registration: Value = {
        let body = response.bytes().await.unwrap();
        let key = eunha::fasp::keys::public_key_from_base64(
            serde_json::from_slice::<Value>(&body).unwrap()["publicKey"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        fake.state.lock().unwrap().server_key = Some(key);
        eunha::fasp::signature::verify_response(
            200,
            &headers,
            &key,
            chrono::Utc::now().timestamp(),
        )
        .expect("the registration answer is signed with the new key");
        serde_json::from_slice(&body).unwrap()
    };
    let fasp_id = registration["faspId"].as_str().unwrap().to_owned();
    assert_eq!(
        registration["registrationCompletionUri"],
        format!(
            "https://{}/admin/fasp/providers/{fasp_id}/registration/new",
            ctx.domain
        )
    );

    let alice: i64 = ctx.alice_id.parse().unwrap();
    helpers::make_admin(&ctx.db, alice).await;
    helpers::grant_admin_scopes(&ctx.db, alice).await;

    // Unconfirmed, it is listed but cannot call anything signed.
    let listed: Value = ctx
        .api
        .get("/api/v1/admin/fasp/providers", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed[0]["confirmed"], false);
    assert_eq!(
        listed[0]["provider_public_key_fingerprint"],
        eunha::fasp::keys::fingerprint(&fake.state.lock().unwrap().public)
    );

    let confirmed = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/fasp/providers/{fasp_id}/registration"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(confirmed.status(), StatusCode::OK);
    let confirmed: Value = confirmed.json().await.unwrap();
    assert_eq!(confirmed["confirmed"], true);
    assert_eq!(confirmed["sign_in_url"], "https://fasp.example/sign_in");
    assert_eq!(confirmed["contact_email"], "admin@fasp.example");
    assert_eq!(confirmed["capabilities"].as_array().unwrap().len(), 4);
    let info = fake
        .wait_for("the provider info request", |r| r.path == "/provider_info")
        .await;
    assert!(info.verified, "eunha signs the provider info request");
    fasp_id
}

#[tokio::test]
async fn test_fasp_is_off_unless_configured() {
    let ctx = TestContext::reaching_loopback("fasp-off").await;
    let response = ctx
        .api
        .post_json("/api/fasp/registration", None, &json!({"name": "x"}))
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// Registration, confirmation, capability activation, and requests signed
/// both ways, with every way a request can fail to authenticate.
#[tokio::test]
async fn test_fasp_registration_capabilities_and_signatures() {
    let ctx = fasp_ctx("fasp-register").await;
    let fake = spawn_fake().await;
    let fasp_id = register_and_confirm(&ctx, &fake).await;

    // Enabling three capabilities and disabling the fourth tells the provider
    // of each.
    let updated = ctx
        .api
        .put_json(
            &format!("/api/v1/admin/fasp/providers/{fasp_id}"),
            Some(&ctx.alice_token),
            &json!({"capabilities": [
                {"id": "account_search", "version": "0.1", "enabled": true},
                {"id": "follow_recommendation", "version": "0.1", "enabled": true},
                {"id": "data_sharing", "version": "0.1", "enabled": true},
                {"id": "callback", "version": "0.1", "enabled": false},
            ]}),
        )
        .await;
    assert_eq!(updated.status(), StatusCode::OK);
    for capability in ["account_search", "follow_recommendation", "data_sharing"] {
        let path = format!("/capabilities/{capability}/0/activation");
        let call = fake
            .wait_for(&path, |r| r.method == "POST" && r.path == path)
            .await;
        assert!(call.verified);
    }
    let deactivated = fake
        .wait_for("the callback deactivation", |r| {
            r.method == "DELETE" && r.path == "/capabilities/callback/0/activation"
        })
        .await;
    assert!(deactivated.verified);
    // Saving the same capabilities again tells it nothing.
    let before = fake.received().len();
    ctx.api
        .put_json(
            &format!("/api/v1/admin/fasp/providers/{fasp_id}"),
            Some(&ctx.alice_token),
            &json!({"capabilities": [
                {"id": "account_search", "version": "0.1", "enabled": true},
                {"id": "follow_recommendation", "version": "0.1", "enabled": true},
                {"id": "data_sharing", "version": "0.1", "enabled": true},
                {"id": "callback", "version": "0.1", "enabled": false},
            ]}),
        )
        .await;
    assert_eq!(fake.received().len(), before);

    // A signed subscription is accepted, and the answer is signed.
    let response = provider_call(
        &ctx,
        &fake,
        "POST",
        "/api/fasp/data_sharing/v0/event_subscriptions",
        Some(json!({"category": "content", "subscriptionType": "lifecycle", "maxBatchSize": 10})),
        &fasp_id,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = assert_signed_by_eunha(&fake, response).await;
    let subscription_id = created["subscription"]["id"].as_i64().unwrap();

    // An unknown category is refused.
    let response = provider_call(
        &ctx,
        &fake,
        "POST",
        "/api/fasp/data_sharing/v0/event_subscriptions",
        Some(json!({"category": "weather", "subscriptionType": "lifecycle", "maxBatchSize": 10})),
        &fasp_id,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Unsigned, signed by another key, naming another provider, or with a
    // body its digest does not cover: each is a bare 401.
    let unsigned = reqwest::Client::new()
        .post(ctx.api.url("/api/fasp/data_sharing/v0/event_subscriptions"))
        .header("host", &ctx.domain)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(unsigned.status(), StatusCode::UNAUTHORIZED);
    let impostor = spawn_fake().await;
    let response = provider_call(
        &ctx,
        &impostor,
        "POST",
        "/api/fasp/data_sharing/v0/event_subscriptions",
        Some(json!({"category": "content", "subscriptionType": "lifecycle", "maxBatchSize": 10})),
        &fasp_id,
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = provider_call(
        &ctx,
        &fake,
        "POST",
        "/api/fasp/data_sharing/v0/event_subscriptions",
        Some(json!({"category": "content", "subscriptionType": "lifecycle", "maxBatchSize": 10})),
        "999999",
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let target = format!(
        "https://{}/api/fasp/data_sharing/v0/event_subscriptions",
        ctx.domain
    );
    let signed = ojak::sig::rfc9421::sign_request(
        "post",
        &target,
        Some(b"{}"),
        &fasp_id,
        &ojak::sig::rfc9421::SigningKey::Ed25519(&fake.seed()),
        chrono::Utc::now().timestamp(),
    )
    .unwrap();
    let tampered = reqwest::Client::new()
        .post(ctx.api.url("/api/fasp/data_sharing/v0/event_subscriptions"))
        .header("host", &ctx.domain)
        .header("content-digest", signed.content_digest.unwrap())
        .header("signature-input", signed.signature_input)
        .header("signature", signed.signature)
        .body(r#"{"category":"account"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(tampered.status(), StatusCode::UNAUTHORIZED);

    // The subscription can be withdrawn, once.
    let path = format!("/api/fasp/data_sharing/v0/event_subscriptions/{subscription_id}");
    let response = provider_call(&ctx, &fake, "DELETE", &path, None, &fasp_id).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_signed_by_eunha(&fake, response).await;
    let response = provider_call(&ctx, &fake, "DELETE", &path, None, &fasp_id).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // A debug call asks the provider to call back, and its callback is kept.
    let response = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/fasp/providers/{fasp_id}/debug_calls"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let call = fake
        .wait_for("the debug call", |r| r.path == "/debug/v0/callback/logs")
        .await;
    assert!(call.verified);
    assert_eq!(call.body, json!({"hello": "world"}));
    let response = provider_call(
        &ctx,
        &fake,
        "POST",
        "/api/fasp/debug/v0/callback/responses",
        Some(json!({"hello": "back"})),
        &fasp_id,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let callbacks: Value = ctx
        .api
        .get("/api/v1/admin/fasp/debug/callbacks", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(callbacks[0]["request_body"], r#"{"hello":"back"}"#);
    assert_eq!(callbacks[0]["provider"]["id"], fasp_id);

    // Deleting the provider takes its subscriptions and callbacks with it.
    let response = ctx
        .api
        .delete(
            &format!("/api/v1/admin/fasp/providers/{fasp_id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM fasp_debug_callbacks")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// Statuses and accounts are announced to subscribed providers as they are
/// created, changed and deleted, and a favourite past the threshold makes a
/// status trend.
#[tokio::test]
async fn test_fasp_announces_lifecycle_events_and_trends() {
    let ctx = fasp_ctx("fasp-events").await;
    let fake = spawn_fake().await;
    let fasp_id = register_and_confirm(&ctx, &fake).await;
    sqlx::query("UPDATE accounts SET indexable = true, discoverable = true WHERE id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let mut subscriptions = HashMap::new();
    for (category, kind, threshold) in [
        ("content", "lifecycle", None),
        ("account", "lifecycle", None),
        (
            "content",
            "trends",
            Some(json!({"likes": 1, "timeframe": 60})),
        ),
    ] {
        let mut body = json!({"category": category, "subscriptionType": kind, "maxBatchSize": 10});
        if let Some(threshold) = threshold {
            body["threshold"] = threshold;
        }
        let response = provider_call(
            &ctx,
            &fake,
            "POST",
            "/api/fasp/data_sharing/v0/event_subscriptions",
            Some(body),
            &fasp_id,
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let created: Value = response.json().await.unwrap();
        subscriptions.insert(
            (category, kind),
            created["subscription"]["id"].as_i64().unwrap().to_string(),
        );
    }

    // A public post is announced as new.
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "hello providers", "public")
        .await;
    let status_id = status["id"].as_str().unwrap().to_owned();
    let uri = status["uri"].as_str().unwrap().to_owned();
    let announced = fake
        .wait_for_announcement("the new status", |b| {
            b["eventType"] == "new" && b["objectUris"] == json!([uri])
        })
        .await;
    assert_eq!(
        announced,
        json!({
            "source": {"subscription": {"id": subscriptions[&("content", "lifecycle")]}},
            "category": "content",
            "eventType": "new",
            "objectUris": [uri],
        })
    );
    let request = fake
        .wait_for("the announcement", |r| {
            r.path == "/data_sharing/v0/announcements"
        })
        .await;
    assert!(request.verified, "announcements are signed");

    // A private one is not.
    let private = ctx
        .api
        .post_status(&ctx.alice_token, "not for providers", "private")
        .await;
    let private_uri = private["uri"].as_str().unwrap().to_owned();

    // Edited, then favourited past the threshold, then deleted.
    let response = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{status_id}"),
            Some(&ctx.alice_token),
            &json!({"status": "hello again, providers"}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    fake.wait_for_announcement("the update", |b| {
        b["eventType"] == "update" && b["objectUris"] == json!([uri])
    })
    .await;
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{status_id}/favourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    let trending = fake
        .wait_for_announcement("the trend", |b| b["eventType"] == "trending")
        .await;
    assert_eq!(
        trending["source"]["subscription"]["id"],
        subscriptions[&("content", "trends")]
    );
    assert_eq!(trending["objectUris"], json!([uri]));
    ctx.api
        .delete(&format!("/api/v1/statuses/{status_id}"), &ctx.alice_token)
        .await;
    fake.wait_for_announcement("the deletion", |b| {
        b["eventType"] == "delete" && b["objectUris"] == json!([uri])
    })
    .await;

    // A profile change is announced for a discoverable account.
    let response = ctx
        .api
        .patch_json(
            "/api/v1/accounts/update_credentials",
            Some(&ctx.alice_token),
            &json!({"display_name": "Alice of the providers"}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let account = fake
        .wait_for_announcement("the account update", |b| b["category"] == "account")
        .await;
    assert_eq!(account["eventType"], "update");
    assert_eq!(
        account["source"]["subscription"]["id"],
        subscriptions[&("account", "lifecycle")]
    );
    assert_eq!(account["objectUris"], json!([alice_uri(&ctx).await]));

    assert!(
        !fake
            .announcements()
            .iter()
            .any(|b| b["objectUris"] == json!([private_uri])),
        "a private status is never announced"
    );
}

/// A backfill request is answered a batch at a time, newest first, each batch
/// saying whether more are left, until a continuation asks for the rest.
#[tokio::test]
async fn test_fasp_backfill() {
    let ctx = fasp_ctx("fasp-backfill").await;
    let fake = spawn_fake().await;
    let fasp_id = register_and_confirm(&ctx, &fake).await;
    sqlx::query("UPDATE accounts SET indexable = true WHERE id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let older = ctx
        .api
        .post_status(&ctx.alice_token, "older", "public")
        .await;
    let newer = ctx
        .api
        .post_status(&ctx.alice_token, "newer", "public")
        .await;

    let response = provider_call(
        &ctx,
        &fake,
        "POST",
        "/api/fasp/data_sharing/v0/backfill_requests",
        Some(json!({"category": "content", "maxCount": 1})),
        &fasp_id,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = assert_signed_by_eunha(&fake, response).await;
    let request_id = created["backfillRequest"]["id"].as_i64().unwrap();

    let first = fake
        .wait_for_announcement("the first batch", |b| {
            b["source"]["backfillRequest"].is_object()
        })
        .await;
    assert_eq!(
        first,
        json!({
            "source": {"backfillRequest": {"id": request_id.to_string()}},
            "category": "content",
            "objectUris": [newer["uri"]],
            "moreObjectsAvailable": true,
        })
    );

    let response = provider_call(
        &ctx,
        &fake,
        "POST",
        &format!("/api/fasp/data_sharing/v0/backfill_requests/{request_id}/continuation"),
        None,
        &fasp_id,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let second = fake
        .wait_for_announcement("the second batch", |b| {
            b["source"]["backfillRequest"].is_object() && b["objectUris"] == json!([older["uri"]])
        })
        .await;
    assert_eq!(second["moreObjectsAvailable"], false);
    for _ in 0..100 {
        let fulfilled: bool =
            sqlx::query_scalar("SELECT fulfilled FROM fasp_backfill_requests WHERE id = $1")
                .bind(request_id)
                .fetch_one(&ctx.db)
                .await
                .unwrap();
        if fulfilled {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the backfill request was never fulfilled");
}

/// Alice's actor URI, as her account says it.
async fn alice_uri(ctx: &TestContext) -> String {
    let account: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.alice_id), None)
        .await
        .json()
        .await
        .unwrap();
    account["uri"].as_str().unwrap().to_owned()
}

/// Polls an async refresh until it finishes, and returns its last state.
async fn wait_finished(ctx: &TestContext, header: &str) -> Value {
    let id = header
        .strip_prefix("id=\"")
        .and_then(|rest| rest.split_once('"'))
        .unwrap()
        .0
        .to_owned();
    for _ in 0..200 {
        let body: Value = ctx
            .api
            .get(
                &format!("/api/v1_alpha/async_refreshes/{id}"),
                Some(&ctx.alice_token),
            )
            .await
            .json()
            .await
            .unwrap();
        if body["async_refresh"]["status"] == "finished" {
            return body;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the async refresh never finished");
}

/// Follow suggestions ask providers for recommendations in the background,
/// fetch the accounts they name, and suggest them once fetched.
#[tokio::test]
async fn test_fasp_follow_recommendations() {
    let ctx = fasp_ctx("fasp-follow").await;
    let fake = spawn_fake().await;
    let fasp_id = register_and_confirm(&ctx, &fake).await;
    ctx.api
        .put_json(
            &format!("/api/v1/admin/fasp/providers/{fasp_id}"),
            Some(&ctx.alice_token),
            &json!({"capabilities": [
                {"id": "follow_recommendation", "version": "0.1", "enabled": true},
            ]}),
        )
        .await;
    let eve = format!("{}/users/eve", fake.base);
    fake.state.lock().unwrap().recommendations = vec![eve.clone()];

    let response = ctx
        .api
        .get("/api/v2/suggestions", Some(&ctx.alice_token))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let header = response
        .headers()
        .get("mastodon-async-refresh")
        .expect("the retrieval is announced in a header")
        .to_str()
        .unwrap()
        .to_owned();
    let finished = wait_finished(&ctx, &header).await;
    assert_eq!(finished["async_refresh"]["result_count"], 1);
    let asked = fake
        .wait_for("the recommendation request", |r| {
            r.path == "/follow_recommendation/v0/accounts"
        })
        .await;
    assert!(asked.verified);
    assert_eq!(
        asked.query,
        format!("accountUri={}", urlencoding::encode(&alice_uri(&ctx).await))
    );

    // Eve's actor says she is discoverable, which is what lets her be
    // suggested.
    let (eve_id, discoverable): (i64, Option<bool>) =
        sqlx::query_as("SELECT id, discoverable FROM accounts WHERE uri = $1")
            .bind(&eve)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(discoverable, Some(true));
    let suggestions: Value = ctx
        .api
        .http
        .get(ctx.api.url("/api/v2/suggestions"))
        .header("host", &ctx.domain)
        .header("mastodon-async-refresh-id", "follow-up")
        .bearer_auth(&ctx.alice_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let eve_key = eve_id.to_string();
    let suggested = suggestions
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["account"]["id"].as_str() == Some(eve_key.as_str()))
        .expect("eve is suggested");
    assert_eq!(suggested["sources"], json!(["fasp"]));
}

/// An account search asks providers in the background, and fetches the
/// accounts they name for the next search to find.
#[tokio::test]
async fn test_fasp_account_search() {
    let ctx = fasp_ctx("fasp-search").await;
    let fake = spawn_fake().await;
    let fasp_id = register_and_confirm(&ctx, &fake).await;
    enable_account_search(&ctx, &fasp_id).await;
    let frank = format!("{}/users/frank", fake.base);
    fake.state.lock().unwrap().search_results = vec![frank.clone()];

    let response = ctx
        .api
        .get(
            "/api/v2/search?q=frank&type=accounts",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let header = async_refresh_header(&response).expect("the provider search is announced");
    let finished = wait_finished(&ctx, &header).await;
    assert_eq!(finished["async_refresh"]["result_count"], 1);
    let asked = fake
        .wait_for("the search request", |r| {
            r.path == "/account_search/v0/search"
        })
        .await;
    assert!(asked.verified);
    assert_eq!(asked.query, "limit=10&term=frank");
    let known: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts WHERE uri = $1")
        .bind(&frank)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(known, 1);
}

/// As upstream: the controller keys the refresh by `q` as sent and creates it
/// for any search type, while the worker, started only by the account search,
/// finishes the refresh keyed by the query it searched, without a leading
/// `@`. A statuses search, or an account search for `@name`, leaves the
/// refresh its header names running.
#[tokio::test]
async fn test_fasp_account_search_refresh_keys() {
    let ctx = fasp_ctx("fasp-search-keys").await;
    let fake = spawn_fake().await;
    let fasp_id = register_and_confirm(&ctx, &fake).await;
    enable_account_search(&ctx, &fasp_id).await;

    let response = ctx
        .api
        .get(
            "/api/v2/search?q=%40grace&type=accounts",
            Some(&ctx.alice_token),
        )
        .await;
    let at_header = async_refresh_header(&response).expect("a refresh for `@grace`");
    let asked = fake
        .wait_for("the search request", |r| {
            r.path == "/account_search/v0/search"
        })
        .await;
    assert_eq!(asked.query, "limit=10&term=grace");
    // The worker finishes the refresh keyed by `grace`.
    let mut finished = None;
    for _ in 0..200 {
        finished = key_status(&ctx, "grace").await;
        if finished.as_deref() == Some("finished") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(finished.as_deref(), Some("finished"));
    assert_eq!(refresh_status(&ctx, &at_header).await, "running");

    // A statuses search announces a refresh that nothing finishes.
    let response = ctx
        .api
        .get(
            "/api/v2/search?q=heidi&type=statuses",
            Some(&ctx.alice_token),
        )
        .await;
    let header = async_refresh_header(&response).expect("a refresh for any search type");
    assert_eq!(refresh_status(&ctx, &header).await, "running");
    // And while it runs, the same `q` announces none.
    let response = ctx
        .api
        .get(
            "/api/v2/search?q=heidi&type=accounts",
            Some(&ctx.alice_token),
        )
        .await;
    assert!(async_refresh_header(&response).is_none());
}

async fn enable_account_search(ctx: &TestContext, fasp_id: &str) {
    ctx.api
        .put_json(
            &format!("/api/v1/admin/fasp/providers/{fasp_id}"),
            Some(&ctx.alice_token),
            &json!({"capabilities": [
                {"id": "account_search", "version": "0.1", "enabled": true},
            ]}),
        )
        .await;
}

fn async_refresh_header(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get("mastodon-async-refresh")
        .map(|v| v.to_str().unwrap().to_owned())
}

async fn refresh_status(ctx: &TestContext, header: &str) -> String {
    let id = header
        .strip_prefix("id=\"")
        .and_then(|rest| rest.split_once('"'))
        .unwrap()
        .0
        .to_owned();
    let body: Value = ctx
        .api
        .get(
            &format!("/api/v1_alpha/async_refreshes/{id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    body["async_refresh"]["status"].as_str().unwrap().to_owned()
}

/// The status of the account search refresh keyed by `query`.
async fn key_status(ctx: &TestContext, query: &str) -> Option<String> {
    let key = eunha::fasp::workers::account_search_refresh_key(query);
    eunha::async_refresh::AsyncRefresh::new(&ctx.state, &key)
        .await
        .status
}

/// Each announcement is a `Fasp::*Worker` job on the `fasp` queue, which a
/// restart does not lose.
#[tokio::test]
async fn test_fasp_announcements_wait_on_the_fasp_queue() {
    let ctx = fasp_ctx("fasp-queue").await;
    let fake = spawn_fake().await;
    let fasp_id = register_and_confirm(&ctx, &fake).await;
    sqlx::query("UPDATE accounts SET indexable = true WHERE id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let response = provider_call(
        &ctx,
        &fake,
        "POST",
        "/api/fasp/data_sharing/v0/event_subscriptions",
        Some(json!({"category": "content", "subscriptionType": "lifecycle", "maxBatchSize": 10})),
        &fasp_id,
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);

    ctx.state.jobs.set_mode(eunha::jobs::Mode::Durable);
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "queued for providers", "public")
        .await;
    let uri = status["uri"].as_str().unwrap().to_owned();
    let queued = eunha::jobs::queued(&ctx.state, "Fasp::AnnounceContentLifecycleEventWorker")
        .await
        .unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].queue, "fasp");
    assert_eq!(queued[0].args, json!({"uri": uri, "event_type": "new"}));
    assert!(fake.announcements().is_empty());

    eunha::jobs::drain(&ctx.state).await.unwrap();
    fake.wait_for_announcement("the queued announcement", |b| {
        b["objectUris"] == json!([uri])
    })
    .await;
}
