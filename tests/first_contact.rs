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
use axum::{Json, Router};
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
    let app = Router::new()
        .route(
            "/users/eve",
            get(
                |State((document, fetches)): State<(Value, Arc<AtomicUsize>)>| async move {
                    fetches.fetch_add(1, Ordering::SeqCst);
                    Json(document)
                },
            ),
        )
        .with_state((document, fetches.clone()));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    (actor_uri, fetches)
}

/// Sends a first Follow from a new actor and returns how often it was fetched.
async fn follow_from_new_actor(label: &str, shared_inbox: bool) -> usize {
    eunha::federation::safe_fetch::set_allowed_private_networks(vec!["127.0.0.0/8"
        .parse()
        .unwrap()]);
    let ctx = TestContext::new(label).await;

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

/// An actor with no shared inbox is an account like any other. Mastodon's
/// column is NOT NULL, and the missing endpoint is stored as ''.
#[tokio::test]
async fn test_actor_without_shared_inbox_is_accepted() {
    follow_from_new_actor("no-shared-inbox", false).await;
}
