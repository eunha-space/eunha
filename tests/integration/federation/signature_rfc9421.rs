//! Double-knocking: draft-cavage first, RFC 9421 if the peer rejects it.
//!
//! Mastodon 4.7 signs outgoing requests the way the network still verifies —
//! the cavage draft — and retries with RFC 9421 HTTP Message Signatures when a
//! peer answers 400 or 401. Eunha does the same, so a peer that has moved on
//! still receives what this instance sends.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use axum::{body::Body, extract::State, http::Request, response::Response, routing::any, Router};
use serde_json::json;

/// What one inbox saw of a delivery attempt.
#[derive(Default)]
struct Attempts {
    cavage: AtomicUsize,
    rfc9421: AtomicUsize,
}

/// An inbox that accepts only the signature style it is told to accept.
async fn spawn_inbox(accepts_cavage: bool) -> (String, Arc<Attempts>) {
    let attempts = Arc::new(Attempts::default());

    let app = Router::new()
        .fallback(any(
            |State(state): State<(Arc<Attempts>, bool)>, req: Request<Body>| async move {
                let (attempts, accepts_cavage) = state;
                let signed_rfc9421 = req.headers().contains_key("signature-input");
                if signed_rfc9421 {
                    attempts.rfc9421.fetch_add(1, Ordering::SeqCst);
                } else {
                    attempts.cavage.fetch_add(1, Ordering::SeqCst);
                }

                let accepted = if signed_rfc9421 { true } else { accepts_cavage };
                Response::builder()
                    .status(if accepted { 202 } else { 401 })
                    .body(Body::empty())
                    .unwrap()
            },
        ))
        .with_state((attempts.clone(), accepts_cavage));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (format!("http://{addr}/inbox"), attempts)
}

fn test_key() -> String {
    eunha::crypto::generate_rsa_keypair().unwrap().0
}

/// A peer that verifies the draft never sees an RFC 9421 request: the fallback
/// costs nothing when the first signature is accepted.
#[tokio::test]
async fn test_cavage_signature_is_enough_for_peers_that_take_it() {
    let (inbox, attempts) = spawn_inbox(true).await;
    let http = reqwest::Client::new();

    eunha::federation::delivery::deliver(
        &http,
        &json!({"type": "Create"}),
        &inbox,
        "https://local.test/users/alice#main-key",
        &test_key(),
    )
    .await
    .expect("delivery should succeed on the first attempt");

    assert_eq!(attempts.cavage.load(Ordering::SeqCst), 1);
    assert_eq!(attempts.rfc9421.load(Ordering::SeqCst), 0);
}

/// A peer that rejects the draft with 401 gets the same activity again, signed
/// with RFC 9421, and the delivery succeeds.
#[tokio::test]
async fn test_rejected_delivery_is_retried_with_rfc9421() {
    let (inbox, attempts) = spawn_inbox(false).await;
    let http = reqwest::Client::new();

    eunha::federation::delivery::deliver(
        &http,
        &json!({"type": "Create"}),
        &inbox,
        "https://local.test/users/alice#main-key",
        &test_key(),
    )
    .await
    .expect("delivery should succeed on the RFC 9421 retry");

    assert_eq!(attempts.cavage.load(Ordering::SeqCst), 1);
    assert_eq!(attempts.rfc9421.load(Ordering::SeqCst), 1);
}

/// The retried request carries what RFC 9421 requires: the covered component
/// list, a signature under the same label, and the digest it covers.
#[tokio::test]
async fn test_retry_carries_rfc9421_headers() {
    /// The RFC 9421 headers one request arrived with.
    type CapturedHeaders = (String, String, String);
    let captured: Arc<std::sync::Mutex<Vec<CapturedHeaders>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let app = Router::new()
        .fallback(any(
            |State(captured): State<Arc<std::sync::Mutex<Vec<CapturedHeaders>>>>,
             req: Request<Body>| async move {
                let header = |name: &str| {
                    req.headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string()
                };
                if req.headers().contains_key("signature-input") {
                    captured.lock().unwrap().push((
                        header("signature-input"),
                        header("signature"),
                        header("content-digest"),
                    ));
                    return Response::builder().status(202).body(Body::empty()).unwrap();
                }
                Response::builder().status(401).body(Body::empty()).unwrap()
            },
        ))
        .with_state(captured.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let key_id = "https://local.test/users/alice#main-key";
    eunha::federation::delivery::deliver(
        &reqwest::Client::new(),
        &json!({"type": "Create"}),
        &format!("http://{addr}/inbox"),
        key_id,
        &test_key(),
    )
    .await
    .unwrap();

    let seen = captured.lock().unwrap();
    let (signature_input, signature, content_digest) = seen.first().expect("no RFC 9421 request");
    assert!(
        signature_input.starts_with("sig1=(\"@method\" \"@target-uri\" \"content-digest\")"),
        "unexpected covered components: {signature_input}"
    );
    assert!(signature_input.contains(&format!("keyid=\"{key_id}\"")));
    assert!(
        !signature_input.contains("alg="),
        "Mastodon's signer states no algorithm; neither should this one"
    );
    assert!(signature.starts_with("sig1=:") && signature.ends_with(':'));
    assert!(
        content_digest.starts_with("sha-256=:") && content_digest.ends_with(':'),
        "expected an RFC 9530 digest, got {content_digest}"
    );
}

/// An inbound activity signed with RFC 9421 is accepted, so a peer that has
/// stopped sending cavage signatures can still reach this instance.
#[tokio::test]
async fn test_inbound_rfc9421_activity_is_accepted() {
    use crate::helpers::TestContext;

    let ctx = TestContext::new("rfc9421-in").await;
    let (priv_pem, pub_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    let uri = "https://remote.invalid/users/kim";
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'kim', 'remote.invalid', 'Kim', '', $2, $2, $3, $2 || '/inbox', $2 || '/outbox', now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(uri)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();

    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{uri}#update-1"),
        "type": "Update",
        "actor": uri,
        "object": {
            "id": uri,
            "type": "Person",
            "preferredUsername": "kim",
            "name": "Kim, updated",
            "inbox": format!("{uri}/inbox"),
        },
    });

    let resp = ctx
        .api
        .post_signed_rfc9421("/inbox", &activity, &format!("{uri}#main-key"), &priv_pem)
        .await;
    assert!(
        resp.status().is_success(),
        "an RFC 9421 signed activity should be accepted, got {}",
        resp.status()
    );

    let display_name: String =
        sqlx::query_scalar("SELECT display_name FROM accounts WHERE uri = $1")
            .bind(uri)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(display_name, "Kim, updated");
}

/// A body swapped after signing is rejected: the signature covers the digest,
/// and the digest covers the body.
#[tokio::test]
async fn test_inbound_rfc9421_rejects_a_swapped_body() {
    use crate::helpers::TestContext;

    let ctx = TestContext::new("rfc9421-swap").await;
    let (priv_pem, pub_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    let uri = "https://remote.invalid/users/lee";
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'lee', 'remote.invalid', 'Lee', '', $2, $2, $3, $2 || '/inbox', $2 || '/outbox', now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(uri)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();

    let signed_body = json!({"id": format!("{uri}#u"), "type": "Update", "actor": uri,
                             "object": {"id": uri, "type": "Person", "name": "signed"}});
    let signed = ojak_runtime::rfc9421::sign_request(
        "post",
        &format!("https://{}/inbox", ctx.api.host),
        Some(&serde_json::to_vec(&signed_body).unwrap()),
        &format!("{uri}#main-key"),
        &ojak_runtime::rfc9421::SigningKey::RsaPem(&priv_pem),
    )
    .unwrap();

    let swapped = json!({"id": format!("{uri}#u"), "type": "Update", "actor": uri,
                         "object": {"id": uri, "type": "Person", "name": "swapped"}});
    let resp = ctx
        .api
        .http
        .post(ctx.api.url("/inbox"))
        .header("host", &ctx.api.host)
        .header("signature-input", signed.signature_input)
        .header("signature", signed.signature)
        .header("content-digest", signed.content_digest.unwrap())
        .header("content-type", "application/activity+json")
        .body(serde_json::to_vec(&swapped).unwrap())
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        401,
        "a body the signature does not cover must be rejected"
    );
}
