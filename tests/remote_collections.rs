//! Remote collections as Mastodon keeps them
//! (`ProcessFeaturedCollectionService`, `ProcessFeaturedItemService`,
//! `VerifyFeaturedItemService`, and the `Add` and `Remove` around them),
//! against a fake remote server.
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
use reqwest::StatusCode;
use serde_json::{json, Value};

use helpers::TestContext;

const AS: &str = "https://www.w3.org/ns/activitystreams";

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

/// Starts the remote server, returning its base URL.
async fn spawn_remote(remote: &Remote) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(serve).with_state(remote.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// A remote account on `base`'s host (an `https://` URL elsewhere), known
/// here with a key: its id, URI and private key.
async fn account(ctx: &TestContext, base: &str, username: &str) -> (i64, String, String) {
    let (private_pem, public_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    let uri = format!("{base}/users/{username}");
    let domain = base
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key,
                                 inbox_url, outbox_url, shared_inbox_url, followers_url,
                                 featured_collection_url, collections_url,
                                 created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $5, $4 || '/inbox', $4 || '/outbox',
                   $6 || '/inbox', $4 || '/followers', $4 || '/collections/featured',
                   $4 || '/featured_collections', now(), now())"#,
    )
    .bind(id)
    .bind(username)
    .bind(domain)
    .bind(&uri)
    .bind(public_pem)
    .bind(base)
    .execute(&ctx.db)
    .await
    .unwrap();
    (id, uri, private_pem)
}

/// POST `activity` to the shared inbox as `actor`, signed.
async fn deliver(ctx: &TestContext, actor: &str, pem: &str, activity: &Value) {
    let status = ctx
        .api
        .post_signed("/inbox", activity, &format!("{actor}#main-key"), pem)
        .await
        .status();
    assert!(status.is_success(), "{status}");
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

/// A collection's row: owner, name, description, language, total, url and
/// topic.
#[derive(Debug, sqlx::FromRow)]
struct CollectionRow {
    id: i64,
    account_id: i64,
    name: String,
    description_html: Option<String>,
    language: Option<String>,
    original_number_of_items: Option<i32>,
    url: Option<String>,
    tag_name: Option<String>,
    item_count: i32,
}

async fn collection(ctx: &TestContext, uri: &str) -> Option<CollectionRow> {
    sqlx::query_as(
        "SELECT c.id, c.account_id, c.name, c.description_html, c.language,
                c.original_number_of_items, c.url, t.name AS tag_name, c.item_count
         FROM collections c LEFT JOIN tags t ON t.id = c.tag_id WHERE c.uri = $1",
    )
    .bind(uri)
    .fetch_optional(&ctx.db)
    .await
    .unwrap()
}

/// An item's state, featured account, object and authorization, by its URI.
async fn item(
    ctx: &TestContext,
    uri: &str,
) -> Option<(i32, Option<i64>, Option<String>, Option<String>)> {
    sqlx::query_as(
        "SELECT state, account_id, object_uri, approval_uri FROM collection_items WHERE uri = $1",
    )
    .bind(uri)
    .fetch_optional(&ctx.db)
    .await
    .unwrap()
}

fn featured_collection(base: &str, owner: &str, name: &str, items: Vec<Value>) -> Value {
    json!({
        "id": format!("{base}/collections/1"),
        "type": "FeaturedCollection",
        "attributedTo": owner,
        "name": name,
        "summaryMap": {"en": "<p>The best</p>"},
        "sensitive": false,
        "discoverable": true,
        "totalItems": items.len() + 1,
        "topic": {"type": "Hashtag", "href": format!("{base}/tags/rust"), "name": "#rust"},
        "url": format!("{base}/@rob/collections/1"),
        "orderedItems": items,
    })
}

/// A remote collection is its sender's alone: kept from its own host, under
/// its own name, with its items pending until their stamps are fetched and
/// hold, and no other actor can write over it or its items.
#[tokio::test]
async fn a_remote_collection_is_its_senders_and_its_items_wait_for_their_stamps() {
    let ctx = TestContext::reaching_loopback("remote-collection").await;
    let store = Remote::default();
    let base = spawn_remote(&store).await;
    let (rob_id, rob, rob_pem) = account(&ctx, &base, "rob").await;
    let (_, mallory, mallory_pem) = account(&ctx, &base, "mallory").await;
    let (carol_id, carol, _) = account(&ctx, &base, "carol").await;
    let (_, dave, _) = account(&ctx, &base, "dave").await;
    let (_, eve, eve_pem) = account(&ctx, "https://evil.invalid", "eve").await;
    let collection_uri = format!("{base}/collections/1");
    let stamp = format!("{base}/users/carol/feature_authorizations/1");
    store.put(
        "/users/carol/feature_authorizations/1",
        json!({
            "@context": AS,
            "id": stamp,
            "type": "FeatureAuthorization",
            "interactingObject": collection_uri,
            "interactionTarget": carol,
        }),
    );
    let carol_item = format!("{base}/items/1");
    let dave_item = format!("{base}/items/2");
    let items = vec![
        json!({"id": carol_item, "type": "FeaturedItem", "featuredObject": carol,
               "featureAuthorization": stamp}),
        // A stamp that is not there.
        json!({"id": dave_item, "type": "FeaturedItem", "featuredObject": dave,
               "featureAuthorization": format!("{base}/users/dave/feature_authorizations/9")}),
    ];
    deliver(
        &ctx,
        &rob,
        &rob_pem,
        &json!({
            "@context": AS,
            "id": format!("{rob}#adds/1"),
            "type": "Add",
            "actor": rob,
            "target": format!("{rob}/featured_collections"),
            "object": featured_collection(&base, &rob, "Rob's picks", items.clone()),
        }),
    )
    .await;

    let stored = collection(&ctx, &collection_uri).await.expect("stored");
    assert_eq!(stored.account_id, rob_id);
    assert_eq!(stored.name, "Rob's picks");
    assert_eq!(stored.description_html.as_deref(), Some("<p>The best</p>"));
    assert_eq!(stored.language.as_deref(), Some("en"));
    assert_eq!(stored.original_number_of_items, Some(3));
    assert_eq!(stored.tag_name.as_deref(), Some("rust"));
    assert_eq!(
        stored.url.as_deref(),
        Some(format!("{base}/@rob/collections/1").as_str())
    );

    assert!(
        eventually(async || {
            item(&ctx, &carol_item).await.is_some_and(|i| i.0 == 1)
                && item(&ctx, &dave_item).await.is_some_and(|i| i.0 == 2)
        })
        .await,
        "carol's item is accepted by her stamp, dave's rejected without one: {:?} {:?}",
        item(&ctx, &carol_item).await,
        item(&ctx, &dave_item).await,
    );
    assert_eq!(
        item(&ctx, &carol_item).await.unwrap(),
        (1, Some(carol_id), Some(carol.clone()), Some(stamp.clone()))
    );
    // Dave's item never featured anyone: it names him only.
    assert_eq!(
        item(&ctx, &dave_item).await.unwrap(),
        (2, None, Some(dave.clone()), None)
    );
    assert_eq!(
        collection(&ctx, &collection_uri).await.unwrap().item_count,
        2
    );

    // Mallory, on rob's host, cannot rename rob's collection, under his name
    // or under rob's.
    for owner in [&mallory, &rob] {
        deliver(
            &ctx,
            &mallory,
            &mallory_pem,
            &json!({
                "@context": AS,
                "id": format!("{mallory}#updates/{}", owner.len()),
                "type": "Update",
                "actor": mallory,
                "object": featured_collection(&base, owner, "Mallory's now", vec![]),
            }),
        )
        .await;
        let after = collection(&ctx, &collection_uri).await.unwrap();
        assert_eq!(
            (after.account_id, after.name.as_str()),
            (rob_id, "Rob's picks")
        );
    }
    // Nor take an item out of it, nor can eve, elsewhere, add one.
    deliver(
        &ctx,
        &mallory,
        &mallory_pem,
        &json!({"@context": AS, "id": format!("{mallory}#removes/1"), "type": "Remove",
                "actor": mallory, "target": collection_uri, "object": carol_item}),
    )
    .await;
    deliver(
        &ctx,
        &eve,
        &eve_pem,
        &json!({"@context": AS, "id": format!("{eve}#adds/1"), "type": "Add", "actor": eve,
                "target": collection_uri,
                "object": {"id": "https://evil.invalid/items/1", "type": "FeaturedItem",
                           "featuredObject": eve,
                           "featureAuthorization": "https://evil.invalid/stamps/1"}}),
    )
    .await;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM collection_items WHERE collection_id = $1")
            .bind(stored.id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(count, 2);

    // Rob lists carol alone: dave's item goes.
    deliver(
        &ctx,
        &rob,
        &rob_pem,
        &json!({
            "@context": AS,
            "id": format!("{rob}#updates/1"),
            "type": "Update",
            "actor": rob,
            "object": featured_collection(&base, &rob, "Rob's picks", items[..1].to_vec()),
        }),
    )
    .await;
    assert!(item(&ctx, &dave_item).await.is_none());
    assert!(item(&ctx, &carol_item).await.is_some());

    // Rob takes carol's item out, then the collection.
    deliver(
        &ctx,
        &rob,
        &rob_pem,
        &json!({"@context": AS, "id": format!("{rob}#removes/1"), "type": "Remove",
                "actor": rob, "target": collection_uri, "object": carol_item}),
    )
    .await;
    assert!(item(&ctx, &carol_item).await.is_none());
    deliver(
        &ctx,
        &rob,
        &rob_pem,
        &json!({"@context": AS, "id": format!("{rob}#removes/2"), "type": "Remove",
                "actor": rob, "target": format!("{rob}/featured_collections"),
                "object": collection_uri}),
    )
    .await;
    assert!(collection(&ctx, &collection_uri).await.is_none());
}

/// Only the sender's own posts are pinned, an unknown one fetched; a
/// `Hashtag` added to or removed from its featured collection is featured
/// or no longer.
#[tokio::test]
async fn pins_and_featured_hashtags_are_the_senders_own() {
    let ctx = TestContext::reaching_loopback("remote-pins").await;
    let store = Remote::default();
    let base = spawn_remote(&store).await;
    let (rob_id, rob, rob_pem) = account(&ctx, &base, "rob").await;
    let (_, mallory, mallory_pem) = account(&ctx, &base, "mallory").await;
    let featured = format!("{rob}/collections/featured");
    let pins = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT s.uri FROM status_pins p JOIN statuses s ON s.id = p.status_id
             WHERE p.account_id = $1 ORDER BY s.uri",
        )
        .bind(rob_id)
        .fetch_all(&ctx.db)
        .await
        .unwrap()
    };

    // Alice's post is not rob's to pin.
    let alice_post = ctx
        .api
        .post_status(&ctx.alice_token, "mine", "public")
        .await;
    let alice_uri: String = sqlx::query_scalar("SELECT uri FROM statuses WHERE id = $1")
        .bind(alice_post["id"].as_str().unwrap().parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let add = |n: u32, object: Value| {
        json!({"@context": AS, "id": format!("{rob}#adds/{n}"), "type": "Add", "actor": rob,
               "target": featured, "object": object})
    };
    deliver(&ctx, &rob, &rob_pem, &add(1, json!(alice_uri))).await;
    assert!(pins().await.is_empty());

    // His own, not known yet, is fetched and pinned.
    let post = format!("{rob}/statuses/1");
    store.put(
        "/users/rob/statuses/1",
        json!({
            "@context": AS,
            "id": post,
            "type": "Note",
            "attributedTo": rob,
            "content": "<p>pin me</p>",
            "published": "2026-01-01T00:00:00Z",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
        }),
    );
    deliver(&ctx, &rob, &rob_pem, &add(2, json!(post))).await;
    assert_eq!(pins().await, vec![post.clone()]);

    // Mallory cannot unpin it: rob's featured collection is not his.
    deliver(
        &ctx,
        &mallory,
        &mallory_pem,
        &json!({"@context": AS, "id": format!("{mallory}#removes/1"), "type": "Remove",
                "actor": mallory, "target": featured, "object": post}),
    )
    .await;
    assert_eq!(pins().await, vec![post.clone()]);

    // A hashtag added is featured, and removed is not.
    let hashtag = json!({"type": "Hashtag", "href": format!("{base}/tags/Rust"), "name": "#Rust"});
    deliver(&ctx, &rob, &rob_pem, &add(3, hashtag.clone())).await;
    let tags = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT t.name FROM featured_tags ft JOIN tags t ON t.id = ft.tag_id
             WHERE ft.account_id = $1",
        )
        .bind(rob_id)
        .fetch_all(&ctx.db)
        .await
        .unwrap()
    };
    assert_eq!(tags().await.len(), 1);
    deliver(
        &ctx,
        &rob,
        &rob_pem,
        &json!({"@context": AS, "id": format!("{rob}#removes/1"), "type": "Remove",
                "actor": rob, "target": featured, "object": hashtag}),
    )
    .await;
    assert!(tags().await.is_empty());

    deliver(
        &ctx,
        &rob,
        &rob_pem,
        &json!({"@context": AS, "id": format!("{rob}#removes/2"), "type": "Remove",
                "actor": rob, "target": featured, "object": post}),
    )
    .await;
    assert!(pins().await.is_empty());
}
