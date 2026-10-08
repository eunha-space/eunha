//! Followers collection synchronization (FEP-8fcf) as Mastodon does it, and
//! a remote collection asking to feature a local account, against a fake
//! remote server.
//!
//! A test binary of its own because it has to let eunha fetch from 127.0.0.1.
//! The SSRF guard's allowlist is process-wide and set once, so granting it in
//! the main suite would grant it to every test there.

// Test servers have no tenant span to keep.
#![allow(clippy::disallowed_methods)]

#[allow(dead_code)]
#[path = "integration/helpers.rs"]
mod helpers;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::Uri;
use axum::Router;
use ojak::synchronization::{CollectionSynchronization, Digest};
use reqwest::StatusCode;
use serde_json::{json, Value};

use helpers::TestContext;

/// A remote server: documents by path.
#[derive(Clone, Default)]
struct Remote {
    documents: Arc<Mutex<HashMap<String, Value>>>,
}

impl Remote {
    fn put(&self, path: &str, document: Value) {
        self.documents
            .lock()
            .unwrap()
            .insert(path.to_owned(), document);
    }
}

async fn serve(State(remote): State<Remote>, uri: Uri) -> axum::response::Response {
    use axum::response::IntoResponse;
    match remote.documents.lock().unwrap().get(uri.path()) {
        Some(document) => (
            [("content-type", "application/activity+json")],
            document.to_string(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The remote server, with `rob` known here: its base URL, rob's id and
/// URI, and his private key.
async fn remote(ctx: &TestContext, remote: &Remote) -> (String, i64, String, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(serve).with_state(remote.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (private_pem, public_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    let rob = format!("{base}/users/rob");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key,
                                 inbox_url, outbox_url, shared_inbox_url, followers_url,
                                 created_at, updated_at)
           VALUES ($1, 'rob', $2, 'rob', '', $3, $3, $4, $3 || '/inbox', $3 || '/outbox',
                   $5 || '/inbox', $3 || '/followers', now(), now())"#,
    )
    .bind(id)
    .bind(base.trim_start_matches("http://"))
    .bind(&rob)
    .bind(public_pem)
    .bind(&base)
    .execute(&ctx.db)
    .await
    .unwrap();
    (base, id, rob, private_pem)
}

/// POST `activity` to the shared inbox as `actor`, signed, with `extra`
/// headers beside the signature's.
async fn deliver(
    ctx: &TestContext,
    actor: &str,
    pem: &str,
    activity: &Value,
    extra: &[(&str, String)],
) -> StatusCode {
    let signing_url = format!("https://{}/inbox", ctx.domain);
    let (parts, body) =
        ojak::testing::signed_post(&signing_url, activity, &format!("{actor}#main-key"), pem)
            .into_parts();
    let mut request = reqwest::Client::new()
        .post(ctx.api.url("/inbox"))
        .headers(parts.headers);
    for (name, value) in extra {
        request = request.header(*name, value);
    }
    request.body(body).send().await.unwrap().status()
}

async fn follow(ctx: &TestContext, follower: i64, target: i64) {
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, now(), now())",
    )
    .bind(follower)
    .bind(target)
    .execute(&ctx.db)
    .await
    .unwrap();
}

async fn eventually(mut done: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..50 {
        if done().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// A followers-only post says, on each delivery, the digest of its author's
/// followers on that server, and the server can list them, signed.
#[tokio::test]
async fn a_followers_only_post_asks_its_receivers_to_check_their_followers() {
    let ctx = TestContext::reaching_loopback("sync-out").await;
    let store = Remote::default();
    let (base, rob_id, rob, rob_pem) = remote(&ctx, &store).await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    follow(&ctx, rob_id, alice).await;
    sqlx::query("UPDATE accounts SET private_key = 'test-private-key' WHERE id = $1")
        .bind(alice)
        .execute(&ctx.db)
        .await
        .unwrap();

    for visibility in ["private", "public"] {
        ctx.api
            .post_status(&ctx.alice_token, visibility, visibility)
            .await;
    }
    let queued: Vec<(Value, Option<bool>)> = sqlx::query_as(
        "SELECT payload->'activity', (payload->>'synchronize_collection')::boolean
         FROM eunha.ojak_queue WHERE queue IN ('delivery', 'delivery-priority')
           AND payload->>'inbox' = $1",
    )
    .bind(format!("{base}/inbox"))
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(queued.len(), 2, "{queued:?}");
    for (activity, synchronize) in queued {
        let private = activity["object"]["content"]
            .as_str()
            .unwrap()
            .contains("private");
        assert_eq!(synchronize.unwrap_or(false), private, "{activity}");
    }

    // The header the deliverer writes for that inbox.
    let header = eunha::federation::followers_synchronization::header_for(
        &ctx.db,
        &ctx.domain,
        alice,
        &format!("{base}/inbox"),
    )
    .await
    .unwrap();
    let header = CollectionSynchronization::parse(&header).unwrap();
    assert_eq!(
        header.collection_id,
        format!("https://{}/users/alice/followers", ctx.domain)
    );
    assert_eq!(header.digest, Digest::of([rob.as_str()]).to_hex());
    let path = "/users/alice/followers_synchronization";
    assert_eq!(header.url, format!("https://{}{path}", ctx.domain));

    // Listed to rob's server, signed, and to no one unsigned.
    assert_eq!(
        ctx.api.ap_get(path, None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let resp = ctx
        .api
        .ap_get_signed(path, &format!("{rob}#main-key"), &rob_pem)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()["cache-control"].to_str().unwrap(),
        "max-age=0, private"
    );
    let listed: Value = resp.json().await.unwrap();
    assert_eq!(listed["type"], "OrderedCollection");
    assert_eq!(listed["orderedItems"], json!([rob]));
}

/// A delivery that says rob's followers here are not who eunha thinks has
/// them fetched: a local account rob lists that does not follow him sends
/// the `Undo` of the follow it never knew of, and one he does not list
/// stops following him.
#[tokio::test]
async fn a_remote_account_s_followers_here_are_brought_in_line() {
    let ctx = TestContext::reaching_loopback("sync-in").await;
    let store = Remote::default();
    let (base, rob_id, rob, rob_pem) = remote(&ctx, &store).await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    follow(&ctx, alice, rob_id).await;
    sqlx::query("UPDATE accounts SET private_key = 'test-private-key' WHERE id = $1")
        .bind(bob)
        .execute(&ctx.db)
        .await
        .unwrap();
    let bob_uri = format!("https://{}/users/bob", ctx.domain);
    store.put(
        "/users/rob/followers_synchronization",
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{base}/users/rob/followers_synchronization"),
            "type": "OrderedCollection",
            "orderedItems": [bob_uri],
        }),
    );
    let header = CollectionSynchronization {
        collection_id: format!("{rob}/followers"),
        url: format!("{base}/users/rob/followers_synchronization"),
        digest: Digest::of([bob_uri.as_str()]).to_hex(),
    }
    .to_header();
    let status = deliver(
        &ctx,
        &rob,
        &rob_pem,
        &json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{rob}#likes/1"),
            "type": "Like",
            "actor": rob,
            "object": format!("https://{}/users/alice/statuses/1", ctx.domain),
        }),
        &[("collection-synchronization", header)],
    )
    .await;
    assert!(status.is_success(), "{status}");

    let db = ctx.db.clone();
    assert!(
        eventually(async || {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM follows WHERE account_id = $1 AND target_account_id = $2",
            )
            .bind(alice)
            .bind(rob_id)
            .fetch_one(&db)
            .await
            .unwrap()
                == 0
        })
        .await,
        "alice, whom rob does not list, no longer follows him"
    );
    let undo: Vec<Value> = sqlx::query_scalar(
        "SELECT payload->'activity' FROM eunha.ojak_queue
         WHERE payload->>'inbox' = $1 AND payload->'activity'->>'type' = 'Undo'
           AND payload->'activity'->>'actor' = $2",
    )
    .bind(format!("{rob}/inbox"))
    .bind(&bob_uri)
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(undo.len(), 1, "bob, whom rob lists, undoes the follow");
    assert_eq!(undo[0]["object"]["object"], json!(rob));
}

/// A `FeatureRequest` for a local account is accepted with a stamp at
/// Mastodon's address, `/ap/users/{id}/feature_authorizations/{item}`,
/// which is served there.
#[tokio::test]
async fn a_feature_request_is_stamped_where_mastodon_stamps_it() {
    let ctx = TestContext::reaching_loopback("feature-request").await;
    let store = Remote::default();
    let (base, _, rob, rob_pem) = remote(&ctx, &store).await;
    let collection = format!("{base}/collections/7");
    store.put(
        "/collections/7",
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": collection,
            "type": "FeaturedCollection",
            "name": "Rob's",
            "attributedTo": rob,
            "totalItems": 0,
            "orderedItems": [],
        }),
    );
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let alice_uri: String = sqlx::query_scalar("SELECT uri FROM accounts WHERE id = $1")
        .bind(alice)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let status = deliver(
        &ctx,
        &rob,
        &rob_pem,
        &json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{base}/feature_requests/1"),
            "type": "FeatureRequest",
            "actor": rob,
            "object": alice_uri,
            "instrument": collection,
        }),
        &[],
    )
    .await;
    assert!(status.is_success(), "{status}");
    let db = ctx.db.clone();
    let mut stamp = None;
    assert!(
        eventually(async || {
            stamp = sqlx::query_scalar::<_, Option<String>>(
                "SELECT ci.approval_uri FROM collection_items ci
                 JOIN collections c ON c.id = ci.collection_id
                 WHERE c.uri = $1 AND ci.account_id = $2",
            )
            .bind(&collection)
            .bind(alice)
            .fetch_optional(&db)
            .await
            .unwrap()
            .flatten();
            stamp.is_some()
        })
        .await,
        "the request is accepted"
    );
    let stamp = stamp.unwrap();
    let prefix = format!(
        "https://{}/ap/users/{}/feature_authorizations/",
        ctx.domain, ctx.alice_id
    );
    assert!(stamp.starts_with(&prefix), "{stamp}");
    let served: Value = ctx
        .api
        .ap_get(
            stamp.trim_start_matches(&format!("https://{}", ctx.domain)),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(served["id"].as_str(), Some(stamp.as_str()));
    assert_eq!(
        served["interactingObject"].as_str(),
        Some(collection.as_str())
    );
}
