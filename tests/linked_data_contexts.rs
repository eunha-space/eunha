//! A relayed activity's Linked Data Signature checked over a context eunha
//! does not ship, fetched from a fake remote server and cached as Mastodon's
//! document loader fetches and caches it (`JsonLdHelper#load_jsonld_context`),
//! and checked before an FEP-8b32 proof whose key the fake server publishes.
//!
//! A test binary of its own because it has to let eunha fetch from
//! 127.0.0.1, which is process-wide; the main suite checks that a context on
//! a private address is refused.

// Test servers have no tenant span to keep.
#![allow(clippy::disallowed_methods)]

#[allow(dead_code)]
#[path = "integration/helpers.rs"]
mod helpers;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Router;
use reqwest::StatusCode;
use serde_json::{json, Value};

use helpers::TestContext;

/// The context the fake server serves at `/ns`: one term, `mood`.
fn context_document() -> Value {
    json!({"@context": {"mood": "https://contexts.example/ns#mood"}})
}

/// The fake server, counting the requests for each path.
#[derive(Clone, Default)]
struct Remote {
    requests: Arc<AtomicUsize>,
}

async fn serve(State(remote): State<Remote>, Path(path): Path<String>) -> axum::response::Response {
    remote.requests.fetch_add(1, Ordering::SeqCst);
    match path.as_str() {
        "ns" => (
            [("content-type", "application/ld+json")],
            context_document().to_string(),
        )
            .into_response(),
        // Mastodon takes a context only when it is served as JSON-LD.
        "json" => (
            [("content-type", "application/json")],
            context_document().to_string(),
        )
            .into_response(),
        // Past `Request#body_with_limit`'s megabyte.
        "huge" => (
            [("content-type", "application/ld+json")],
            format!(
                "{{\"@context\": {{\"mood\": \"https://contexts.example/ns#mood\"}}, \"padding\": \"{}\"}}",
                "x".repeat(1024 * 1024)
            ),
        )
            .into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A remote account with a key, known here as Mastodon would know it.
async fn seed_remote(ctx: &TestContext, username: &str, domain: &str) -> (String, String) {
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    let uri = format!("https://{domain}/users/{username}");
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, protocol, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $5, $4 || '/inbox', $4 || '/outbox', 1, now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(username)
    .bind(domain)
    .bind(&uri)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();
    (uri, priv_pem)
}

/// Bob's public post, naming `context` beside ActivityStreams and using its
/// term, signed by bob over `signed_over` as the context's document.
fn signed_create(bob: &str, bob_key: &str, n: u32, context: &str, signed_over: &Value) -> Value {
    let create = json!({
        "@context": ["https://www.w3.org/ns/activitystreams", context],
        "id": format!("{bob}/statuses/{n}/activity"),
        "type": "Create",
        "actor": bob,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [format!("{bob}/followers")],
        "object": {
            "id": format!("{bob}/statuses/{n}"),
            "type": "Note",
            "attributedTo": bob,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [format!("{bob}/followers")],
            "content": "<p>signed over a context of bob's own</p>",
            "published": "2026-10-01T12:00:00Z",
            "mood": "sunny",
        },
    });
    let now = chrono::Utc::now().timestamp();
    ojak::sig::linked_data::sign(
        &ojak_jsonld::Registry::bundled().with(context, signed_over.clone()),
        &create,
        &format!("{bob}#main-key"),
        &ojak::sig::PrivateKey::from_pem(bob_key).unwrap(),
        now,
        now + ojak::sig::linked_data::DEFAULT_LIFETIME_SECONDS,
    )
    .unwrap()
}

async fn stored(ctx: &TestContext, bob: &str, n: u32) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
        .bind(format!("{bob}/statuses/{n}"))
        .fetch_one(&ctx.db)
        .await
        .unwrap()
        == 1
}

/// Signed over a context the fake server serves, a relayed post is taken
/// on bob's signature; the context is fetched once and kept, under the
/// instance's prefix, for the next. One served as plain JSON, or past a
/// megabyte, is refused, and the post naming it is not taken.
#[tokio::test]
async fn test_a_relayed_post_is_checked_over_a_fetched_context() {
    let ctx = TestContext::reaching_loopback("ldctx-fetch").await;
    let remote = Remote::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/{*path}", axum::routing::get(serve))
        .with_state(remote.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (bob, bob_key) = seed_remote(&ctx, "bob", "bob.invalid").await;
    let (relay, relay_key) = seed_remote(&ctx, "relay", "relay.invalid").await;
    sqlx::query(
        "INSERT INTO relays (inbox_url, state, created_at, updated_at) VALUES ($1, 2, now(), now())",
    )
    .bind(format!("{relay}/inbox"))
    .execute(&ctx.db)
    .await
    .unwrap();
    let relay_key_id = format!("{relay}#main-key");
    let context = format!("{base}/ns");

    let signed = signed_create(&bob, &bob_key, 1, &context, &context_document());
    let resp = ctx
        .api
        .post_signed("/inbox", &signed, &relay_key_id, &relay_key)
        .await;
    assert_eq!(resp.status(), 202);
    assert!(
        stored(&ctx, &bob, 1).await,
        "taken on bob's signature over the fetched context"
    );
    assert_eq!(remote.requests.load(Ordering::SeqCst), 1);

    // Kept for 30 days under the instance's prefix, as Rails.cache keeps it.
    let mut redis = ctx.state.redis.clone();
    let key = ctx
        .state
        .redis_keys
        .key(format!("jsonld:context:{context}"));
    assert!(key.starts_with(&format!("{}:", ctx.state.config.redis_key_prefix)));
    let ttl: i64 = redis::cmd("TTL")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .unwrap();
    assert!(ttl > 29 * 24 * 60 * 60, "{ttl}");

    // The next post over it is checked without fetching it again.
    let signed = signed_create(&bob, &bob_key, 2, &context, &context_document());
    let resp = ctx
        .api
        .post_signed("/inbox", &signed, &relay_key_id, &relay_key)
        .await;
    assert_eq!(resp.status(), 202);
    assert!(stored(&ctx, &bob, 2).await);
    assert_eq!(remote.requests.load(Ordering::SeqCst), 1, "from the cache");

    // Changed on the way, it does not verify over the context either.
    let mut changed = signed_create(&bob, &bob_key, 3, &context, &context_document());
    changed["object"]["mood"] = json!("stormy");
    let resp = ctx
        .api
        .post_signed("/inbox", &changed, &relay_key_id, &relay_key)
        .await;
    assert_eq!(resp.status(), 202);
    assert!(!stored(&ctx, &bob, 3).await);

    // Served as plain JSON, or too large: refused, and so is the post.
    for (n, path) in [(4, "json"), (5, "huge")] {
        let context = format!("{base}/{path}");
        let signed = signed_create(&bob, &bob_key, n, &context, &context_document());
        let resp = ctx
            .api
            .post_signed("/inbox", &signed, &relay_key_id, &relay_key)
            .await;
        assert_eq!(resp.status(), 202);
        assert!(
            !stored(&ctx, &bob, n).await,
            "{path} is not a context Mastodon takes"
        );
        let cached: bool = redis::cmd("EXISTS")
            .arg(
                ctx.state
                    .redis_keys
                    .key(format!("jsonld:context:{context}")),
            )
            .query_async(&mut redis)
            .await
            .unwrap();
        assert!(!cached, "a refused context is not kept");
    }
}

/// Relayed with both its author's Linked Data signature and an FEP-8b32
/// proof, a reply to a local post is taken on the signature, as
/// `ActivityPub::ProcessActivityService` checks it before the proof, and so
/// is passed on to the local author's followers (`forward_for_reply`). The
/// same reply with only its proof is taken on it, and not passed on: what
/// Mastodon forwards is what it took on a Linked Data signature.
#[tokio::test]
async fn test_a_relayed_reply_is_taken_on_its_signature_before_its_proof() {
    use ojak::sig::integrity;

    let ctx = TestContext::reaching_loopback("ldctx-before-proof").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let bob = format!("{base}/users/bob");
    let pem = integrity::generate_ed25519_key(&mut rsa::rand_core::OsRng).unwrap();
    let (seed, public) = integrity::parse_ed25519_key(&pem).unwrap();
    let (bob_rsa, bob_public_rsa) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    let actor = json!({
        "@context": [
            "https://www.w3.org/ns/activitystreams",
            "https://w3id.org/security/v1",
            "https://w3id.org/security/multikey/v1",
        ],
        "id": bob,
        "type": "Person",
        "preferredUsername": "bob",
        "inbox": format!("{bob}/inbox"),
        "publicKey": {
            "id": format!("{bob}#main-key"),
            "owner": bob,
            "publicKeyPem": bob_public_rsa,
        },
        "assertionMethod": [{
            "id": format!("{bob}#ed25519-key"),
            "type": "Multikey",
            "controller": bob,
            "publicKeyMultibase": integrity::encode_ed25519_multikey(&public),
        }],
    });
    let app = Router::new().route(
        "/users/bob",
        axum::routing::get(move || {
            let actor = actor.clone();
            async move {
                (
                    [("content-type", "application/activity+json")],
                    actor.to_string(),
                )
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, protocol, created_at, updated_at)
           VALUES ($1, 'bob', $2, 'bob', '', $3, $3, $4, $3 || '/inbox', $3 || '/outbox', 1, now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(base.trim_start_matches("http://"))
    .bind(&bob)
    .bind(&bob_public_rsa)
    .execute(&ctx.db)
    .await
    .unwrap();
    let (relay, relay_key) = seed_remote(&ctx, "relay", "relay.invalid").await;
    let (nina, _) = seed_remote(&ctx, "nina", "nina.invalid").await;
    let alice_id: i64 =
        sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'alice' AND domain IS NULL")
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
         SELECT $1, id, $2, now(), now() FROM accounts WHERE uri = $3",
    )
    .bind(eunha::snowflake::next_id())
    .bind(alice_id)
    .bind(&nina)
    .execute(&ctx.db)
    .await
    .unwrap();
    let post = ctx
        .api
        .post_status(&ctx.alice_token, "a post to reply to", "public")
        .await;
    let post_uri = post["uri"].as_str().unwrap().to_owned();
    let forwarded = || {
        let db = ctx.db.clone();
        let inbox = format!("{nina}/inbox");
        async move {
            sqlx::query_scalar::<_, Value>(
                "SELECT payload->'activity' FROM eunha.ojak_queue
                 WHERE queue IN ('delivery', 'delivery-priority') AND payload->>'inbox' = $1
                   AND payload->'activity'->>'type' = 'Create'
                 ORDER BY id",
            )
            .bind(inbox)
            .fetch_all(&db)
            .await
            .unwrap()
        }
    };
    let before = forwarded().await.len();

    let public = "https://www.w3.org/ns/activitystreams#Public";
    let reply = |n: u32| {
        json!({
            // The security context the signature would add, so that adding
            // it does not change what the proof covers.
            "@context": [
                "https://www.w3.org/ns/activitystreams",
                "https://w3id.org/security/v1",
            ],
            "id": format!("{bob}/statuses/{n}/activity"),
            "type": "Create",
            "actor": bob,
            "to": [public],
            "object": {
                "id": format!("{bob}/statuses/{n}"),
                "type": "Note",
                "attributedTo": bob,
                "inReplyTo": post_uri,
                "to": [public],
                "content": "a reply",
                "published": "2026-10-01T12:00:00Z",
            },
        })
    };
    let prove = |activity: &Value| {
        integrity::sign_object_integrity_proof(
            activity,
            &format!("{bob}#ed25519-key"),
            &seed,
            chrono::Utc::now().timestamp(),
        )
        .unwrap()
    };
    let relay_key_id = format!("{relay}#main-key");

    // Proved, then signed over the proof: taken on the signature, and passed
    // on as bob signed it.
    let now = chrono::Utc::now().timestamp();
    let both = ojak::sig::linked_data::sign(
        &ojak_jsonld::Registry::bundled(),
        &prove(&reply(1)),
        &format!("{bob}#main-key"),
        &ojak::sig::PrivateKey::from_pem(&bob_rsa).unwrap(),
        now,
        now + ojak::sig::linked_data::DEFAULT_LIFETIME_SECONDS,
    )
    .unwrap();
    let resp = ctx
        .api
        .post_signed("/inbox", &both, &relay_key_id, &relay_key)
        .await;
    assert_eq!(resp.status(), 202);
    assert!(stored(&ctx, &bob, 1).await, "taken on bob's signature");
    let queued = forwarded().await;
    assert_eq!(queued.len(), before + 1, "forwarded to alice's follower");
    assert_eq!(queued.last().unwrap(), &both, "as bob signed it");

    // Proved only: taken on the proof, and passed on to nobody.
    let resp = ctx
        .api
        .post_signed("/inbox", &prove(&reply(2)), &relay_key_id, &relay_key)
        .await;
    assert_eq!(resp.status(), 202);
    assert!(stored(&ctx, &bob, 2).await, "taken on bob's proof");
    assert_eq!(forwarded().await.len(), before + 1, "not forwarded");
}
