//! Remote actors stored as Mastodon's `ActivityPub::ProcessAccountService`
//! stores them, from documents a fake remote server serves.
//!
//! A test binary of its own because it has to let eunha fetch from 127.0.0.1,
//! and ask WebFinger over plain HTTP. Both switches are process-wide, so
//! turning them on in the main suite would turn them on for every test there.

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

/// FEP-521a's example Ed25519 key.
const MULTIKEY: &str = "z6MkrJVnaZkeFzdQyMZu1cgjg7k1pZZ6pvBQ7XJPt4swbTQ2";

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

struct Server {
    remote: Remote,
    base: String,
    /// `host:port`, the domain the actors' handles are on.
    host: String,
    private_pem: String,
    public_pem: String,
}

impl Server {
    fn actor(&self) -> String {
        format!("{}/users/eve", self.base)
    }

    /// WebFinger's answer for eve, naming `href` as her actor.
    fn webfinger(&self, href: &str) {
        self.remote.put(
            "/.well-known/webfinger",
            json!({
                "subject": format!("acct:eve@{}", self.host),
                "links": [{"rel": "self", "type": "application/activity+json", "href": href}],
            }),
        );
    }

    /// Eve's actor document, every property `ProcessAccountService` reads.
    fn full_actor(&self) -> Value {
        let base = &self.base;
        let actor = self.actor();
        json!({
            "@context": [
                "https://www.w3.org/ns/activitystreams",
                "https://w3id.org/security/v1",
                "https://w3id.org/security/multikey/v1"
            ],
            "id": actor,
            "type": "Person",
            "preferredUsername": "eve",
            "webfinger": format!("eve@{}", self.host),
            "name": "Eve :blobcat:",
            "summary": "<p>Hello</p>",
            "url": [
                {"type": "Link", "mimeType": "application/activity+json", "href": actor},
                {"type": "Link", "href": format!("{base}/@eve")}
            ],
            "published": "2020-01-02T03:04:05Z",
            "manuallyApprovesFollowers": true,
            "discoverable": true,
            "indexable": true,
            "memorial": true,
            "showMedia": false,
            "inbox": format!("{actor}/inbox"),
            "outbox": format!("{actor}/outbox"),
            "followers": format!("{actor}/followers"),
            "following": format!("{actor}/following"),
            "featured": format!("{actor}/collections/featured"),
            "featuredTags": format!("{actor}/collections/tags"),
            "endpoints": {"sharedInbox": format!("{base}/inbox")},
            "alsoKnownAs": [format!("{base}/users/old-eve"), {"id": format!("{base}/users/older-eve")}],
            "attributionDomains": ["eve.example", 5, "blog.example"],
            "attachment": [
                {"type": "PropertyValue", "name": "Web", "value": "<a href=\"https://eve.example\">https://eve.example</a>"},
                {"type": "Note", "name": "not a field"},
                {"type": "PropertyValue", "name": "Pronouns", "value": "she/her"}
            ],
            "icon": {"type": "Image", "url": format!("{base}/avatar.png"), "summary": "  A cat  "},
            "image": {"type": "Image", "url": {"type": "Link", "href": format!("{base}/header.png")}, "name": "The sea"},
            "tag": [{
                "type": "Emoji",
                "id": format!("{base}/emojis/blobcat"),
                "name": ":blobcat:",
                "icon": {"type": "Image", "url": format!("{base}/emoji/blobcat.png")}
            }],
            "interactionPolicy": {"canFeature": {
                "automaticApproval": format!("{actor}/followers"),
                "manualApproval": ["https://www.w3.org/ns/activitystreams#Public"]
            }},
            "publicKey": {
                "id": format!("{actor}#main-key"),
                "owner": actor,
                "publicKeyPem": self.public_pem,
            },
            "assertionMethod": [{
                "id": format!("{actor}#ed25519-key"),
                "type": "Multikey",
                "controller": actor,
                "publicKeyMultibase": MULTIKEY,
            }],
        })
    }
}

async fn spawn_server(label: &str) -> (TestContext, Server) {
    eunha::federation::safe_fetch::set_allowed_private_networks(vec!["127.0.0.0/8"
        .parse()
        .unwrap()]);
    eunha::federation::webfinger::use_plain_http_for_tests();
    let ctx = TestContext::new(label).await;
    let remote = Remote::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = listener.local_addr().unwrap().to_string();
    let base = format!("http://{host}");
    let (private_pem, public_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    let app = Router::new().fallback(serve).with_state(remote.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let server = Server {
        remote,
        base,
        host,
        private_pem,
        public_pem,
    };
    server.webfinger(&server.actor());
    (ctx, server)
}

/// Polls `query` until it returns true.
async fn eventually(what: &str, mut query: impl AsyncFnMut() -> bool) {
    for _ in 0..100 {
        if query().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{what} never happened");
}

/// Every column Mastodon writes from an actor document is written the same,
/// and the follow-up work it queues — pinned posts and featured hashtags —
/// is done.
#[tokio::test]
async fn test_a_new_actor_is_stored_as_mastodon_stores_it() {
    let (ctx, server) = spawn_server("actors-full").await;
    let actor = server.actor();
    let base = server.base.clone();
    server.remote.put("/users/eve", server.full_actor());
    server.remote.put(
        "/users/eve/outbox",
        json!({"@context": "https://www.w3.org/ns/activitystreams", "id": format!("{actor}/outbox"),
               "type": "OrderedCollection", "totalItems": 42}),
    );
    server.remote.put(
        "/users/eve/followers",
        json!({"@context": "https://www.w3.org/ns/activitystreams", "id": format!("{actor}/followers"),
               "type": "OrderedCollection", "totalItems": 7, "first": format!("{actor}/followers?page=1")}),
    );
    // No first page: the server keeps who eve follows to itself.
    server.remote.put(
        "/users/eve/following",
        json!({"@context": "https://www.w3.org/ns/activitystreams", "id": format!("{actor}/following"),
               "type": "OrderedCollection", "totalItems": 3}),
    );
    server.remote.put(
        "/users/eve/collections/featured",
        json!({"@context": "https://www.w3.org/ns/activitystreams",
               "id": format!("{actor}/collections/featured"),
               "type": "OrderedCollection", "orderedItems": [format!("{base}/notes/pinned")]}),
    );
    server.remote.put(
        "/users/eve/collections/tags",
        json!({"@context": "https://www.w3.org/ns/activitystreams",
               "id": format!("{actor}/collections/tags"),
               "type": "Collection",
               "items": [{"type": "Hashtag", "name": "#Rust", "href": format!("{base}/tags/rust")}]}),
    );
    server.remote.put(
        "/notes/pinned",
        json!({"@context": "https://www.w3.org/ns/activitystreams",
               "id": format!("{base}/notes/pinned"), "type": "Note",
               "attributedTo": actor, "content": "<p>pinned</p>",
               "published": "2026-01-01T00:00:00Z",
               "to": ["https://www.w3.org/ns/activitystreams#Public"]}),
    );

    let id = eunha::api::ap::inbox::resolve_or_fetch_remote_account(&ctx.state, &actor)
        .await
        .expect("the actor is stored");

    let row = sqlx::query!(
        r#"SELECT username, domain, display_name, note, uri, url, locked, discoverable,
                  indexable, memorial, fields, also_known_as, attribution_domains,
                  avatar_remote_url, avatar_description, header_remote_url, header_description,
                  inbox_url, outbox_url, shared_inbox_url, followers_url, following_url,
                  featured_collection_url, collections_url, actor_type, created_at,
                  feature_approval_policy, show_media, show_featured, show_media_replies,
                  last_webfingered_at, protocol, public_key, hide_collections, moved_to_account_id
           FROM accounts WHERE id = $1"#,
        id
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(row.username, "eve");
    assert_eq!(row.domain.as_deref(), Some(server.host.as_str()));
    assert_eq!(row.display_name, "Eve :blobcat:");
    assert_eq!(row.note, "<p>Hello</p>");
    assert_eq!(row.uri.as_deref(), Some(actor.as_str()));
    assert_eq!(row.url.as_deref(), Some(format!("{base}/@eve").as_str()));
    assert!(row.locked);
    assert_eq!(row.discoverable, Some(true));
    assert!(row.indexable);
    assert!(row.memorial);
    assert_eq!(
        row.fields,
        Some(json!([
            {"name": "Web", "value": "<a href=\"https://eve.example\">https://eve.example</a>"},
            {"name": "Pronouns", "value": "she/her"}
        ]))
    );
    assert_eq!(
        row.also_known_as,
        Some(vec![
            format!("{base}/users/old-eve"),
            format!("{base}/users/older-eve")
        ])
    );
    assert_eq!(
        row.attribution_domains,
        Some(vec!["eve.example".to_owned(), "blog.example".to_owned()])
    );
    assert_eq!(
        row.avatar_remote_url.as_deref(),
        Some(format!("{base}/avatar.png").as_str())
    );
    assert_eq!(row.avatar_description, "A cat");
    assert_eq!(row.header_remote_url, format!("{base}/header.png"));
    assert_eq!(row.header_description, "The sea");
    assert_eq!(row.inbox_url, format!("{actor}/inbox"));
    assert_eq!(row.outbox_url, format!("{actor}/outbox"));
    assert_eq!(row.shared_inbox_url, format!("{base}/inbox"));
    assert_eq!(row.followers_url, format!("{actor}/followers"));
    assert_eq!(row.following_url, format!("{actor}/following"));
    assert_eq!(
        row.featured_collection_url.as_deref(),
        Some(format!("{actor}/collections/featured").as_str())
    );
    assert_eq!(row.collections_url.as_deref(), Some(""));
    assert_eq!(row.actor_type.as_deref(), Some("Person"));
    assert_eq!(
        row.created_at,
        chrono::NaiveDate::from_ymd_opt(2020, 1, 2)
            .unwrap()
            .and_hms_opt(3, 4, 5)
            .unwrap()
    );
    // Followers automatically, anyone with approval.
    assert_eq!(row.feature_approval_policy, (1 << 2) << 16 | 1 << 1);
    assert!(!row.show_media);
    assert!(row.show_featured);
    assert!(row.show_media_replies);
    assert!(row.last_webfingered_at.is_some());
    assert_eq!(row.protocol, 1);
    assert_eq!(row.public_key, "", "keys live in `keypairs`");
    assert_eq!(row.hide_collections, Some(true));
    assert_eq!(row.moved_to_account_id, None);

    let keys = sqlx::query!(
        "SELECT uri, type, public_key FROM keypairs WHERE account_id = $1 ORDER BY type",
        id
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(keys.len(), 2);
    assert_eq!(
        keys[0].uri.as_deref(),
        Some(format!("{actor}#main-key").as_str())
    );
    assert_eq!(keys[0].r#type, 0);
    assert_eq!(keys[0].public_key, server.public_pem);
    assert_eq!(
        keys[1].uri.as_deref(),
        Some(format!("{actor}#ed25519-key").as_str())
    );
    assert_eq!(keys[1].r#type, 1);
    assert!(keys[1]
        .public_key
        .starts_with("-----BEGIN PUBLIC KEY-----\nMCow"));

    let stats = sqlx::query!(
        "SELECT statuses_count, following_count, followers_count FROM account_stats WHERE account_id = $1",
        id
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        (
            stats.statuses_count,
            stats.following_count,
            stats.followers_count
        ),
        (42, 3, 7)
    );

    let emoji = sqlx::query!(
        "SELECT image_remote_url, uri FROM custom_emojis WHERE shortcode = 'blobcat' AND domain = $1",
        server.host
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        emoji.image_remote_url.as_deref(),
        Some(format!("{base}/emoji/blobcat.png").as_str())
    );
    assert_eq!(
        emoji.uri.as_deref(),
        Some(format!("{base}/emojis/blobcat").as_str())
    );

    let db = ctx.db.clone();
    eventually("the pinned post", async || {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM status_pins p JOIN statuses s ON s.id = p.status_id
             WHERE p.account_id = $1 AND s.uri = $2",
        )
        .bind(id)
        .bind(format!("{base}/notes/pinned"))
        .fetch_one(&db)
        .await
        .unwrap()
            == 1
    })
    .await;
    eventually("the featured hashtag", async || {
        sqlx::query_scalar::<_, String>(
            "SELECT ft.name FROM featured_tags ft WHERE ft.account_id = $1",
        )
        .bind(id)
        .fetch_optional(&db)
        .await
        .unwrap()
        .as_deref()
            == Some("Rust")
    })
    .await;

    // What clients are told.
    let account: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{id}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(account["discoverable"], true);
    assert_eq!(account["indexable"], true);
    assert_eq!(account["locked"], true);
    assert_eq!(account["memorial"], true);
    assert_eq!(account["acct"], format!("eve@{}", server.host));
    // `CustomEmoji.from_text(emojifiable_text, domain)`: her server's emoji,
    // shown from where it was found.
    assert_eq!(account["emojis"][0]["shortcode"], "blobcat");
    assert_eq!(
        account["emojis"][0]["url"],
        format!("{base}/emoji/blobcat.png")
    );
}

/// An `Update` of the actor replaces what it says, signed with the key
/// stored in `keypairs`; what the new document leaves out goes back to
/// Mastodon's defaults.
#[tokio::test]
async fn test_an_update_replaces_the_profile() {
    let (ctx, server) = spawn_server("actors-update").await;
    let actor = server.actor();
    server.remote.put("/users/eve", server.full_actor());
    let id = eunha::api::ap::inbox::resolve_or_fetch_remote_account(&ctx.state, &actor)
        .await
        .unwrap();

    let update = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{actor}#updates/1"),
        "type": "Update",
        "actor": actor,
        "object": {
            "id": actor,
            "type": ["Person", "Application"],
            "preferredUsername": "eve",
            "webfinger": format!("eve@{}", server.host),
            "name": "x".repeat(3000),
            "inbox": format!("{actor}/inbox"),
            "publicKey": {
                "id": format!("{actor}#main-key"),
                "owner": actor,
                "publicKeyPem": server.public_pem,
            },
        },
    });
    let resp = ctx
        .api
        .post_signed(
            "/inbox",
            &update,
            &format!("{actor}#main-key"),
            &server.private_pem,
        )
        .await;
    assert!(resp.status().is_success(), "{}", resp.status());

    let row = sqlx::query!(
        r#"SELECT display_name, note, locked, discoverable, indexable, memorial, fields,
                  also_known_as, attribution_domains, featured_collection_url, actor_type,
                  avatar_remote_url, avatar_description, url, shared_inbox_url
           FROM accounts WHERE id = $1"#,
        id
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(row.display_name.chars().count(), 2048, "truncated");
    assert_eq!(row.note, "");
    assert!(!row.locked);
    assert_eq!(row.discoverable, Some(false));
    assert!(!row.indexable);
    assert!(!row.memorial);
    assert_eq!(row.fields, Some(json!({})), "Mastodon stores `{{}}`");
    assert_eq!(row.also_known_as, Some(vec![]));
    assert_eq!(row.attribution_domains, Some(vec![]));
    assert_eq!(row.featured_collection_url.as_deref(), Some(""));
    assert_eq!(row.actor_type.as_deref(), Some("Person"));
    assert_eq!(row.avatar_remote_url.as_deref(), Some(""));
    assert_eq!(row.avatar_description, "");
    assert_eq!(row.url.as_deref(), Some(actor.as_str()));
    assert_eq!(row.shared_inbox_url, "");
    let keys: Vec<Option<String>> =
        sqlx::query_scalar("SELECT uri FROM keypairs WHERE account_id = $1")
            .bind(id)
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    assert_eq!(
        keys,
        vec![Some(format!("{actor}#main-key"))],
        "a key the actor no longer publishes is forgotten"
    );

    // The actor's server suspends it: the flag is followed, and the profile
    // is left as it was.
    let suspended = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{actor}#updates/2"),
        "type": "Update",
        "actor": actor,
        "object": {
            "id": actor,
            "type": "Person",
            "preferredUsername": "eve",
            "webfinger": format!("eve@{}", server.host),
            "name": "Suspended Eve",
            "suspended": true,
            "inbox": format!("{actor}/inbox"),
            "publicKey": {
                "id": format!("{actor}#main-key"),
                "owner": actor,
                "publicKeyPem": server.public_pem,
            },
        },
    });
    let resp = ctx
        .api
        .post_signed(
            "/inbox",
            &suspended,
            &format!("{actor}#main-key"),
            &server.private_pem,
        )
        .await;
    assert!(resp.status().is_success(), "{}", resp.status());
    let row = sqlx::query!(
        "SELECT display_name, suspended_at, suspension_origin FROM accounts WHERE id = $1",
        id
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(row.suspended_at.is_some());
    assert_eq!(
        row.suspension_origin,
        Some(1),
        "suspended by its own server"
    );
    assert_eq!(row.display_name.chars().count(), 2048);
}

/// An actor WebFinger does not vouch for is never stored: its handle could
/// be anyone's.
#[tokio::test]
async fn test_an_actor_webfinger_disowns_is_not_stored() {
    let (ctx, server) = spawn_server("actors-webfinger").await;
    let actor = server.actor();
    server.remote.put("/users/eve", server.full_actor());
    server.webfinger(&format!("{}/users/someone-else", server.base));

    let result = eunha::api::ap::inbox::resolve_or_fetch_remote_account(&ctx.state, &actor).await;
    assert!(result.is_err());
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts WHERE uri = $1")
        .bind(&actor)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(stored, 0);
}

/// An account first seen from a silenced domain starts out limited.
#[tokio::test]
async fn test_new_account_from_blocked_domain_starts_limited() {
    let (ctx, server) = spawn_server("actors-silenced").await;
    sqlx::query(
        "INSERT INTO domain_blocks (domain, severity, created_at, updated_at) VALUES ($1, 0, now(), now())",
    )
    .bind(&server.host)
    .execute(&ctx.db)
    .await
    .unwrap();
    let actor = server.actor();
    let id = eunha::api::ap::inbox::resolve_or_fetch_remote_account_prefetched(
        &ctx.state,
        &actor,
        server.full_actor(),
    )
    .await
    .unwrap();
    let silenced: bool =
        sqlx::query_scalar("SELECT silenced_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(silenced);
}

/// An actor that renamed itself, as WebFinger confirms, keeps its account
/// and takes the new handle; another account that held it is left with an
/// invalid one.
#[tokio::test]
async fn test_a_confirmed_rename_takes_the_handle() {
    let (ctx, server) = spawn_server("actors-rename").await;
    let actor = server.actor();
    let mut document = server.full_actor();
    document["preferredUsername"] = json!("evelyn");
    document["webfinger"] = json!(format!("eve-old@{}", server.host));
    // Known first under another handle.
    server.remote.put(
        "/.well-known/webfinger",
        json!({
            "subject": format!("acct:eve-old@{}", server.host),
            "links": [{"rel": "self", "type": "application/activity+json", "href": actor}],
        }),
    );
    let id = eunha::api::ap::inbox::resolve_or_fetch_remote_account_prefetched(
        &ctx.state,
        &actor,
        document.clone(),
    )
    .await
    .unwrap();
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
        .bind(id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(username, "eve-old");

    // Someone else holds `eve` here.
    let squatter = eunha::snowflake::next_id();
    sqlx::query(
        "INSERT INTO accounts (id, username, domain, uri, created_at, updated_at)
         VALUES ($1, 'eve', $2, 'https://elsewhere.invalid/eve', now(), now())",
    )
    .bind(squatter)
    .bind(&server.host)
    .execute(&ctx.db)
    .await
    .unwrap();

    server.webfinger(&actor);
    server.remote.put("/users/eve", server.full_actor());
    eunha::api::ap::inbox::fetch_remote_account(&ctx.state, &actor)
        .await
        .unwrap();
    let username: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
        .bind(id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(username, "eve");
    let squatter_name: String = sqlx::query_scalar("SELECT username FROM accounts WHERE id = $1")
        .bind(squatter)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(squatter_name, format!("! {squatter}"));
}
