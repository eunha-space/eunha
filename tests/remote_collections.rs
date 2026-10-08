//! Remote collections and feature consent as Mastodon keeps them
//! (`ProcessFeaturedCollectionService`, `ProcessFeaturedItemService`,
//! `VerifyFeaturedItemService`, `FeatureRequest`, and the `Add`, `Remove`,
//! `Accept`, `Reject` and `Delete` around them), against a fake remote
//! server.
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

/// Give a local account a key to sign with.
async fn give_key(ctx: &TestContext, account_id: i64) {
    let (private_pem, public_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    sqlx::query("UPDATE accounts SET private_key = $2, public_key = $3 WHERE id = $1")
        .bind(account_id)
        .bind(private_pem)
        .bind(public_pem)
        .execute(&ctx.db)
        .await
        .unwrap();
}

/// The activities of `kind` queued for `inbox`, oldest first.
async fn queued_for(ctx: &TestContext, kind: &str, inbox: &str) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT payload->'activity' FROM eunha.ojak_queue
         WHERE payload->'activity'->>'type' = $1 AND payload->>'inbox' = $2
         ORDER BY id",
    )
    .bind(kind)
    .bind(inbox)
    .fetch_all(&ctx.db)
    .await
    .unwrap()
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

/// A `FeatureRequest` is answered as `AccountPolicy#feature?` says: refused
/// with a `Reject`, or accepted with an `Accept` naming the stamp, each sent
/// to the sender's own inbox. Only the sender's own collection, asked for
/// from its own host, is answered.
#[tokio::test]
async fn feature_requests_are_answered_as_the_policy_says() {
    let ctx = TestContext::reaching_loopback("feature-policy").await;
    let store = Remote::default();
    let base = spawn_remote(&store).await;
    let (_, rob, rob_pem) = account(&ctx, &base, "rob").await;
    let (_, mallory, mallory_pem) = account(&ctx, &base, "mallory").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    give_key(&ctx, alice).await;
    let alice_uri = format!("https://{}/users/alice", ctx.domain);
    let collection_uri = format!("{base}/collections/1");
    deliver(
        &ctx,
        &rob,
        &rob_pem,
        &json!({"@context": AS, "id": format!("{rob}#adds/1"), "type": "Add", "actor": rob,
                "target": format!("{rob}/featured_collections"),
                "object": featured_collection(&base, &rob, "Rob's", vec![])}),
    )
    .await;
    let request = |actor: &str, id: String| {
        json!({"@context": AS, "id": id, "type": "FeatureRequest", "actor": actor,
               "object": alice_uri, "instrument": collection_uri})
    };
    let items = || async {
        sqlx::query_as::<_, (i64, i32, Option<String>, Option<String>)>(
            "SELECT ci.id, ci.state, ci.activity_uri, ci.approval_uri FROM collection_items ci
             JOIN collections c ON c.id = ci.collection_id WHERE c.uri = $1",
        )
        .bind(&collection_uri)
        .fetch_all(&ctx.db)
        .await
        .unwrap()
    };

    // Alice is locked and rob does not follow her: refused.
    sqlx::query("UPDATE accounts SET locked = true WHERE id = $1")
        .bind(alice)
        .execute(&ctx.db)
        .await
        .unwrap();
    deliver(
        &ctx,
        &rob,
        &rob_pem,
        &request(&rob, format!("{base}/feature_requests/1")),
    )
    .await;
    assert!(items().await.is_empty());
    let rejects = queued_for(&ctx, "Reject", &format!("{rob}/inbox")).await;
    assert_eq!(rejects.len(), 1, "{rejects:?}");
    assert_eq!(
        rejects[0]["object"],
        json!(format!("{base}/feature_requests/1"))
    );
    assert_eq!(
        rejects[0]["id"],
        json!(format!("{alice_uri}#rejects/feature_requests/"))
    );

    sqlx::query("UPDATE accounts SET locked = false WHERE id = $1")
        .bind(alice)
        .execute(&ctx.db)
        .await
        .unwrap();
    // Named on another host than its sender's (whether it is turned away at
    // the door or ignored inside), or for a collection that is not the
    // sender's: not answered.
    ctx.api
        .post_signed(
            "/inbox",
            &request(&rob, "https://elsewhere.invalid/feature_requests/2".into()),
            &format!("{rob}#main-key"),
            &rob_pem,
        )
        .await;
    deliver(
        &ctx,
        &mallory,
        &mallory_pem,
        &request(&mallory, format!("{base}/feature_requests/3")),
    )
    .await;
    assert!(items().await.is_empty());
    assert!(queued_for(&ctx, "Accept", &format!("{rob}/inbox"))
        .await
        .is_empty());
    assert!(queued_for(&ctx, "Reject", &format!("{mallory}/inbox"))
        .await
        .is_empty());

    // Accepted: an item with the request's id and no authorization of its
    // own, and an Accept naming alice's stamp at rob's own inbox.
    deliver(
        &ctx,
        &rob,
        &rob_pem,
        &request(&rob, format!("{base}/feature_requests/4")),
    )
    .await;
    let accepted = items().await;
    assert_eq!(accepted.len(), 1);
    let (item_id, state, activity_uri, approval_uri) = accepted[0].clone();
    assert_eq!(state, 1);
    assert_eq!(activity_uri, Some(format!("{base}/feature_requests/4")));
    assert_eq!(approval_uri, None);
    let accepts = queued_for(&ctx, "Accept", &format!("{rob}/inbox")).await;
    assert_eq!(accepts.len(), 1, "{accepts:?}");
    assert_eq!(
        accepts[0]["result"],
        json!(format!(
            "https://{}/ap/users/{alice}/feature_authorizations/{item_id}",
            ctx.domain
        ))
    );
    assert!(queued_for(&ctx, "Accept", &format!("{base}/inbox"))
        .await
        .is_empty());
}

/// A local collection's request is answered only by the account it asked:
/// accepted under an authorization on that account's host, whose `Add` goes
/// to the collection's reach, and taken back by deleting it, whose `Remove`
/// goes there too. An account that may not be featured is not asked.
#[tokio::test]
async fn a_local_collection_s_request_is_answered_by_the_featured_account() {
    let ctx = TestContext::reaching_loopback("feature-answer").await;
    let store = Remote::default();
    let base = spawn_remote(&store).await;
    let (rob_id, rob, rob_pem) = account(&ctx, &base, "rob").await;
    let (mallory_id, mallory, mallory_pem) = account(&ctx, &base, "mallory").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    give_key(&ctx, alice).await;
    // Mallory follows alice, so her reach is his server.
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, now(), now())",
    )
    .bind(mallory_id)
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();
    let c: Value = ctx
        .api
        .post_json(
            "/api/v1/collections",
            Some(&ctx.alice_token),
            &json!({"name": "Friends"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let cid = c["collection"]["id"].as_str().unwrap().to_owned();
    let add_rob = || {
        let api = &ctx.api;
        let token = ctx.alice_token.clone();
        let path = format!("/api/v1/collections/{cid}/items");
        let body = json!({"account_id": rob_id.to_string()});
        async move { api.post_json(&path, Some(&token), &body).await }
    };

    // Rob says nothing of who may feature him: he is not asked.
    assert_eq!(add_rob().await.status(), StatusCode::FORBIDDEN);
    assert!(queued_for(&ctx, "FeatureRequest", &format!("{rob}/inbox"))
        .await
        .is_empty());

    // Anyone may: he is asked, at his own inbox.
    sqlx::query("UPDATE accounts SET feature_approval_policy = $2 WHERE id = $1")
        .bind(rob_id)
        .bind(eunha::db::models::feature_policy::PUBLIC << 16)
        .execute(&ctx.db)
        .await
        .unwrap();
    assert_eq!(add_rob().await.status(), StatusCode::OK);
    let requests = queued_for(&ctx, "FeatureRequest", &format!("{rob}/inbox")).await;
    assert_eq!(requests.len(), 1, "{requests:?}");
    let request_uri = requests[0]["id"].as_str().unwrap().to_owned();
    let item = || async {
        sqlx::query_as::<_, (i64, i32, Option<String>, Option<chrono::NaiveDateTime>)>(
            "SELECT id, state, approval_uri, approval_last_verified_at
             FROM collection_items WHERE collection_id = $1",
        )
        .bind(cid.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap()
    };
    assert_eq!(item().await.1, 0);

    let accept = |actor: &str, result: String| {
        json!({"@context": AS, "id": format!("{actor}#accepts/1"), "type": "Accept",
               "actor": actor, "object": request_uri, "result": result})
    };
    // Mallory cannot answer for rob, nor rob with a stamp elsewhere.
    deliver(
        &ctx,
        &mallory,
        &mallory_pem,
        &accept(&mallory, format!("{base}/stamps/1")),
    )
    .await;
    deliver(
        &ctx,
        &rob,
        &rob_pem,
        &accept(&rob, "https://elsewhere.invalid/stamps/1".into()),
    )
    .await;
    assert_eq!(item().await.1, 0);

    let stamp = format!("{base}/users/rob/feature_authorizations/1");
    deliver(&ctx, &rob, &rob_pem, &accept(&rob, stamp.clone())).await;
    let (item_id, state, approval_uri, verified_at) = item().await;
    assert_eq!(
        (state, approval_uri.as_deref(), verified_at),
        (1, Some(stamp.as_str()), None)
    );
    // The item's `Add` reaches the collection's members.
    let adds: Vec<Value> = queued_for(&ctx, "Add", &format!("{base}/inbox"))
        .await
        .into_iter()
        .filter(|add| add["object"]["type"] == "FeaturedItem")
        .collect();
    assert_eq!(adds.len(), 1, "{adds:?}");
    assert_eq!(adds[0]["object"]["type"], "FeaturedItem");
    assert_eq!(adds[0]["object"]["featuredObject"], json!(rob));
    assert_eq!(adds[0]["object"]["featureAuthorization"], json!(stamp));

    // Rob takes it back: revoked, and its `Remove` sent.
    deliver(
        &ctx,
        &rob,
        &rob_pem,
        &json!({"@context": AS, "id": format!("{stamp}#delete"), "type": "Delete",
                "actor": rob, "object": stamp}),
    )
    .await;
    assert_eq!(item().await.1, 3);
    let removes: Vec<Value> = sqlx::query_scalar(
        "SELECT payload->'activity' FROM eunha.ojak_queue
         WHERE payload->'activity'->>'type' = 'Remove'",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(removes.len(), 1, "{removes:?}");
    assert_eq!(
        removes[0]["object"],
        json!(format!(
            "https://{}/ap/users/{alice}/collection_items/{item_id}",
            ctx.domain
        ))
    );
}
