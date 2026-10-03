//! An actor this instance has never seen, reached over the network.
//!
//! A test binary of its own because it has to let eunha fetch from 127.0.0.1.
//! The SSRF guard's allowlist is process-wide and set once, so granting it in
//! the main suite would grant it to every test there.

// Test servers have no tenant span to keep.
#![allow(clippy::disallowed_methods)]

#[allow(dead_code)]
#[path = "integration/helpers.rs"]
mod helpers;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};

use helpers::TestContext;

/// A remote server with one actor, counting how often its document is fetched.
async fn spawn_remote_actor(
    public_key_pem: String,
    shared_inbox: bool,
) -> (String, Arc<AtomicUsize>) {
    let fetches = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let actor_uri = format!("{base}/users/eve");

    let mut document = json!({
        "@context": ["https://www.w3.org/ns/activitystreams", "https://w3id.org/security/v1"],
        "id": actor_uri,
        "type": "Person",
        "preferredUsername": "eve",
        "webfinger": format!("eve@{}", base.trim_start_matches("http://")),
        "inbox": format!("{actor_uri}/inbox"),
        "outbox": format!("{actor_uri}/outbox"),
        "publicKey": {
            "id": format!("{actor_uri}#main-key"),
            "owner": actor_uri,
            "publicKeyPem": public_key_pem,
        },
    });
    if shared_inbox {
        document["endpoints"] = json!({ "sharedInbox": format!("{base}/inbox") });
    }
    let jrd = json!({
        "subject": format!("acct:eve@{}", base.trim_start_matches("http://")),
        "links": [{"rel": "self", "type": "application/activity+json", "href": actor_uri}],
    });
    let app = Router::new()
        .route(
            "/.well-known/webfinger",
            get(move || async move { axum::Json(jrd) }),
        )
        .route(
            "/users/eve",
            get(
                |State((document, fetches)): State<(Value, Arc<AtomicUsize>)>| async move {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    // As a server does: a fetch trusts ActivityPub's media
                    // types only, never plain JSON.
                    (
                        [("content-type", "application/activity+json")],
                        document.to_string(),
                    )
                },
            ),
        )
        .with_state((document, fetches.clone()));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    (actor_uri, fetches)
}

/// Sends a first Follow from a new actor and returns how often it was fetched.
async fn follow_from_new_actor(label: &str, shared_inbox: bool) -> usize {
    eunha::federation::webfinger::use_plain_http_for_tests();
    let ctx = TestContext::reaching_loopback(label).await;

    let (private_pem, public_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    let (actor_uri, fetches) = spawn_remote_actor(public_pem, shared_inbox).await;

    let follow = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{actor_uri}#follows/1"),
        "type": "Follow",
        "actor": actor_uri,
        "object": format!("https://{}/users/alice", ctx.domain),
    });
    let resp = ctx
        .api
        .post_signed(
            "/inbox",
            &follow,
            &format!("{actor_uri}#main-key"),
            &private_pem,
        )
        .await;
    let status = resp.status();
    assert!(
        status.is_success(),
        "inbox refused: {status} {}",
        resp.text().await.unwrap_or_default()
    );

    let follower = sqlx::query_scalar::<_, i64>(
        "SELECT f.account_id FROM follows f JOIN accounts a ON a.id = f.account_id WHERE a.uri = $1",
    )
    .bind(&actor_uri)
    .fetch_optional(&ctx.db)
    .await
    .unwrap();
    assert!(
        follower.is_some(),
        "the follow from the new actor did not land"
    );
    fetches.load(Ordering::SeqCst)
}

/// The document fetched to verify a first activity's signature is the one the
/// account is created from: the actor is fetched once, not once for its key
/// and again for the account.
#[tokio::test]
async fn test_first_contact_fetches_the_actor_once() {
    assert_eq!(
        follow_from_new_actor("first-contact", true).await,
        1,
        "the actor was fetched more than once"
    );
}

/// An actor with no shared inbox is an account like any other. Mastodon's
/// column is NOT NULL, and the missing endpoint is stored as ''.
#[tokio::test]
async fn test_actor_without_shared_inbox_is_accepted() {
    follow_from_new_actor("no-shared-inbox", false).await;
}

/// A gateway serving one portable actor (FEP-ef61), recording what is
/// delivered to its inbox.
async fn spawn_gateway(
    signer: &ojak::portable::Ed25519Signer,
) -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
    use ojak::portable::ProofSigner;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = format!("http://{}", listener.local_addr().unwrap());
    let did = signer.did().to_owned();
    let actor = json!({
        "@context": [
            "https://www.w3.org/ns/activitystreams",
            "https://w3id.org/security/data-integrity/v1",
            "https://w3id.org/fep/ef61"
        ],
        "id": format!("ap://{did}/actor"),
        "type": "Person",
        "preferredUsername": "portable",
        "inbox": format!("ap://{did}/actor/inbox"),
        "outbox": format!("ap://{did}/actor/outbox"),
        "gateways": [gateway],
    });
    let actor = signer.prove(&actor).await.unwrap();
    let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
    let app = Router::new()
        .route(
            &format!("/.well-known/apgateway/{did}/actor"),
            get(move || async move {
                (
                    [(
                        "content-type",
                        "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\"",
                    )],
                    actor.to_string(),
                )
            }),
        )
        .route(
            &format!("/.well-known/apgateway/{did}/actor/inbox"),
            axum::routing::post(
                |State(delivered): State<Arc<std::sync::Mutex<Vec<Value>>>>,
                 body: axum::body::Bytes| async move {
                    delivered
                        .lock()
                        .unwrap()
                        .push(serde_json::from_slice(&body).unwrap());
                    axum::http::StatusCode::ACCEPTED
                },
            ),
        )
        .with_state(delivered.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (gateway, delivered)
}

/// A portable actor follows a local account: it is authenticated by its
/// proof alone, fetched from the gateway its id hints at, stored by its
/// canonical id with the gateway as its domain, and reached at the gateway.
#[tokio::test]
async fn test_a_portable_actor_follows() {
    use ojak::portable::{Ed25519Signer, ProofSigner};

    let ctx = TestContext::reaching_loopback("portable-follow").await;
    // Only an account with a signing key answers a Follow.
    let (private_pem, public_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    sqlx::query(
        "UPDATE accounts SET private_key = $1, public_key = $2 WHERE username = 'alice' AND domain IS NULL",
    )
    .bind(&private_pem)
    .bind(&public_pem)
    .execute(&ctx.db)
    .await
    .unwrap();
    let signer = Ed25519Signer::generate();
    let (gateway, _delivered) = spawn_gateway(&signer).await;
    let did = signer.did();
    let hinted = format!(
        "ap://{did}/actor?@gateway={}",
        gateway.replace(':', "%3A").replace('/', "%2F")
    );
    let follow = signer
        .prove(&json!({
            "@context": [
                "https://www.w3.org/ns/activitystreams",
                "https://w3id.org/security/data-integrity/v1"
            ],
            "id": format!("ap://{did}/follows/1"),
            "type": "Follow",
            "actor": hinted,
            "object": format!("https://{}/users/alice", ctx.domain),
        }))
        .await
        .unwrap();

    let resp = ctx.api.post_json("/inbox", None, &follow).await;
    let status = resp.status();
    assert!(
        status.is_success(),
        "inbox refused: {status} {}",
        resp.text().await.unwrap_or_default()
    );

    let canonical = format!("ap://{did}/actor");
    let row = sqlx::query_as::<_, (Option<String>, String, String)>(
        "SELECT domain, inbox_url, username FROM accounts WHERE uri = $1",
    )
    .bind(&canonical)
    .fetch_optional(&ctx.db)
    .await
    .unwrap()
    .expect("the portable actor is stored by its canonical id");
    let host = gateway.trim_start_matches("http://");
    let host = host.split(':').next().unwrap();
    assert_eq!(row.0.as_deref(), Some(host));
    assert_eq!(
        row.1,
        format!("{gateway}/.well-known/apgateway/{did}/actor/inbox")
    );
    assert_eq!(row.2, "portable");
    let follows = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM follows f JOIN accounts a ON a.id = f.account_id WHERE a.uri = $1",
    )
    .bind(&canonical)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(follows, 1, "the follow landed");

    // The Accept goes to the portable actor's inbox at its gateway: queued
    // for the gateway's URL, or already delivered there.
    let queued = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM eunha.ojak_queue WHERE payload->>'inbox' = $1",
    )
    .bind(format!("{gateway}/.well-known/apgateway/{did}/actor/inbox"))
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let delivered = _delivered.lock().unwrap().clone();
    assert!(
        queued == 1 || delivered.iter().any(|a| a["type"] == "Accept"),
        "the Accept is neither queued for the gateway nor delivered: {delivered:?}; queued: {:?}",
        sqlx::query_scalar::<_, String>("SELECT payload::text FROM eunha.ojak_queue")
            .fetch_all(&ctx.db)
            .await
            .unwrap()
    );
    if let Some(accept) = delivered.iter().find(|a| a["type"] == "Accept") {
        assert_eq!(accept["object"]["actor"], canonical.as_str());
    }

    // Signed by another key, it is refused.
    let mallory = Ed25519Signer::generate();
    let mut forged = follow.clone();
    forged.as_object_mut().unwrap().remove("proof");
    forged["id"] = json!(format!("ap://{did}/follows/2"));
    let forged = mallory.prove(&forged).await.unwrap();
    let resp = ctx.api.post_json("/inbox", None, &forged).await;
    assert_eq!(resp.status(), 401);
}
