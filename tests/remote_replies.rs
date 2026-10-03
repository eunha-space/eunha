//! Fetching a remote thread's replies through `replies` collections, served by
//! a fake remote server.
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

/// A remote server: documents by path, and how often each was fetched.
#[derive(Clone, Default)]
struct Remote {
    documents: Arc<Mutex<HashMap<String, Value>>>,
    fetches: Arc<Mutex<HashMap<String, usize>>>,
}

impl Remote {
    fn put(&self, path: &str, document: Value) {
        self.documents
            .lock()
            .unwrap()
            .insert(path.to_owned(), document);
    }

    fn fetches(&self, path: &str) -> usize {
        self.fetches.lock().unwrap().get(path).copied().unwrap_or(0)
    }
}

async fn serve(State(remote): State<Remote>, uri: Uri) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = uri.path().to_owned();
    *remote
        .fetches
        .lock()
        .unwrap()
        .entry(path.clone())
        .or_default() += 1;
    match remote.documents.lock().unwrap().get(&path) {
        Some(document) => (
            [("content-type", "application/activity+json")],
            document.to_string(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Starts the remote server with one actor, `eve`, and returns its base URL
/// and eve's private key.
async fn spawn_remote(remote: &Remote) -> (String, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (private_pem, public_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    let actor = format!("{base}/users/eve");
    remote.put(
        "/users/eve",
        json!({
            "@context": ["https://www.w3.org/ns/activitystreams", "https://w3id.org/security/v1"],
            "id": actor,
            "type": "Person",
            "preferredUsername": "eve",
            // The port is part of the handle, which `preferredUsername`
            // alone cannot say.
            "webfinger": format!("eve@{}", base.trim_start_matches("http://")),
            "inbox": format!("{actor}/inbox"),
            "outbox": format!("{actor}/outbox"),
            "publicKey": {
                "id": format!("{actor}#main-key"),
                "owner": actor,
                "publicKeyPem": public_pem,
            },
        }),
    );
    remote.put(
        "/.well-known/webfinger",
        json!({
            "subject": format!("acct:eve@{}", base.trim_start_matches("http://")),
            "links": [{"rel": "self", "type": "application/activity+json", "href": actor}],
        }),
    );
    let app = Router::new().fallback(serve).with_state(remote.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, private_pem)
}

/// A public note by eve at `/notes/{name}`.
fn note(base: &str, name: &str, in_reply_to: Option<&str>, replies: Option<Value>) -> Value {
    let mut note = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{base}/notes/{name}"),
        "type": "Note",
        "attributedTo": format!("{base}/users/eve"),
        "content": format!("<p>{name}</p>"),
        "published": "2026-01-01T00:00:00Z",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [format!("{base}/users/eve/followers")],
    });
    if let Some(parent) = in_reply_to {
        note["inReplyTo"] = json!(format!("{base}/notes/{parent}"));
    }
    if let Some(replies) = replies {
        note["replies"] = replies;
    }
    note
}

fn page(base: &str, path: &str, items: &[&str], next: Option<&str>) -> Value {
    let mut page = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{base}{path}"),
        "type": "CollectionPage",
        "items": items.iter().map(|n| format!("{base}/notes/{n}")).collect::<Vec<_>>(),
    });
    if let Some(next) = next {
        page["next"] = json!(format!("{base}{next}"));
    }
    page
}

async fn context_ctx(label: &str) -> (TestContext, Remote, String, String) {
    eunha::federation::webfinger::use_plain_http_for_tests();
    let ctx = TestContext::reaching_loopback(label).await;
    let remote = Remote::default();
    let (base, private_pem) = spawn_remote(&remote).await;
    (ctx, remote, base, private_pem)
}

/// Stores `/notes/{name}` the way a search or a boost would.
async fn store(ctx: &TestContext, base: &str, name: &str) -> i64 {
    eunha::api::ap::inbox::fetch_remote_status(&ctx.state, &format!("{base}/notes/{name}"))
        .await
        .unwrap()
        .expect("the remote note should be stored")
}

async fn context(ctx: &TestContext, id: i64, token: Option<&str>) -> (Option<String>, Value) {
    let resp = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/context"), token)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let header = resp
        .headers()
        .get("mastodon-async-refresh")
        .map(|v| v.to_str().unwrap().to_owned());
    (header, resp.json().await.unwrap())
}

/// Polls the refresh until it finishes, and returns its last state.
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

/// Opening a remote thread fetches its replies — page after page of the
/// root's collection until five are gathered, then each reply's own — counts
/// the new ones in the refresh, and does not go again within the cooldown.
#[tokio::test]
async fn test_context_fetches_remote_replies() {
    let (ctx, remote, base, _) = context_ctx("replies-walk").await;
    remote.put(
        "/notes/root",
        note(
            &base,
            "root",
            None,
            Some(json!(format!("{base}/notes/root/replies"))),
        ),
    );
    remote.put(
        "/notes/root/replies",
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{base}/notes/root/replies"),
            "type": "Collection",
            "first": format!("{base}/notes/root/replies/1"),
        }),
    );
    remote.put(
        "/notes/root/replies/1",
        page(
            &base,
            "/notes/root/replies/1",
            &["r1", "r2", "r3"],
            Some("/notes/root/replies/2"),
        ),
    );
    remote.put(
        "/notes/root/replies/2",
        page(
            &base,
            "/notes/root/replies/2",
            &["r4", "r5"],
            Some("/notes/root/replies/3"),
        ),
    );
    // Never read: the first two pages already gave five.
    remote.put(
        "/notes/root/replies/3",
        page(&base, "/notes/root/replies/3", &["r6"], None),
    );
    // r1 embeds its collection and its first page.
    remote.put(
        "/notes/r1",
        note(
            &base,
            "r1",
            Some("root"),
            Some(json!({
                "id": format!("{base}/notes/r1/replies"),
                "type": "Collection",
                "first": {
                    "type": "CollectionPage",
                    "partOf": format!("{base}/notes/r1/replies"),
                    "items": [format!("{base}/notes/r1a")],
                },
            })),
        ),
    );
    remote.put("/notes/r1a", note(&base, "r1a", Some("r1"), None));
    for name in ["r2", "r3", "r4", "r5", "r6"] {
        remote.put(
            &format!("/notes/{name}"),
            note(&base, name, Some("root"), None),
        );
    }

    let root = store(&ctx, &base, "root").await;
    let root_fetches = remote.fetches("/notes/root");

    // Signed out: nothing is started.
    let (header, _) = context(&ctx, root, None).await;
    assert!(header.is_none());
    assert_eq!(remote.fetches("/notes/root"), root_fetches);

    let (header, body) = context(&ctx, root, Some(&ctx.alice_token)).await;
    let header = header.expect("a signed-in view should start fetching replies");
    assert!(header.ends_with(", retry=3, result_count=0"), "{header}");
    assert!(body["descendants"].as_array().unwrap().is_empty());

    let finished = wait_finished(&ctx, &header).await;
    assert_eq!(finished["async_refresh"]["result_count"], 6);
    assert_eq!(remote.fetches("/notes/root/replies/3"), 0);
    assert_eq!(remote.fetches("/notes/r6"), 0);

    let fetched_replies_at: Option<chrono::NaiveDateTime> =
        sqlx::query_scalar("SELECT fetched_replies_at FROM statuses WHERE id = $1")
            .bind(root)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(fetched_replies_at.is_some());

    // Within the cooldown: the thread is served from what was fetched, and
    // nothing is fetched again.
    let fetches = remote.fetches("/notes/root");
    let (header, body) = context(&ctx, root, Some(&ctx.alice_token)).await;
    assert!(header.is_none(), "{header:?}");
    let mut contents: Vec<String> = body["descendants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["content"].as_str().unwrap().to_owned())
        .collect();
    contents.sort();
    assert_eq!(
        contents,
        [
            "<p>r1</p>",
            "<p>r1a</p>",
            "<p>r2</p>",
            "<p>r3</p>",
            "<p>r4</p>",
            "<p>r5</p>"
        ]
    );
    assert_eq!(remote.fetches("/notes/root"), fetches);

    // Once the cooldown has passed, the next view fetches again; replies
    // already held count for nothing.
    sqlx::query(
        "UPDATE statuses SET fetched_replies_at = now() - interval '16 minutes' WHERE id = $1",
    )
    .bind(root)
    .execute(&ctx.db)
    .await
    .unwrap();
    let (header, _) = context(&ctx, root, Some(&ctx.alice_token)).await;
    let finished = wait_finished(&ctx, &header.expect("due again after the cooldown")).await;
    assert_eq!(finished["async_refresh"]["result_count"], 0);
    assert_eq!(remote.fetches("/notes/root"), fetches + 1);
}

/// A status too new, local, or not public is not fetched for; a fetch already
/// running is reported to anyone, signed in or not.
#[tokio::test]
async fn test_context_refresh_conditions() {
    let (ctx, remote, base, _) = context_ctx("replies-when").await;
    let mut fresh = note(&base, "fresh", None, None);
    fresh["published"] = json!(chrono::Utc::now().to_rfc3339());
    remote.put("/notes/fresh", fresh);
    let mut private = note(&base, "private", None, None);
    private["to"] = json!([format!("{base}/users/eve/followers")]);
    private["cc"] = json!([]);
    remote.put("/notes/private", private);
    remote.put("/notes/old", note(&base, "old", None, None));

    let fresh = store(&ctx, &base, "fresh").await;
    let (header, _) = context(&ctx, fresh, Some(&ctx.alice_token)).await;
    assert!(header.is_none(), "a status under five minutes old");

    let private = store(&ctx, &base, "private").await;
    // Let alice see it.
    sqlx::query(
        "INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
         SELECT $1, $2, account_id, now(), now() FROM statuses WHERE id = $3",
    )
    .bind(eunha::snowflake::next_id())
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .bind(private)
    .execute(&ctx.db)
    .await
    .unwrap();
    let (header, _) = context(&ctx, private, Some(&ctx.alice_token)).await;
    assert!(header.is_none(), "a followers-only status");

    let local = ctx
        .api
        .post_status(&ctx.alice_token, "a local post", "public")
        .await;
    let local: i64 = local["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE statuses SET created_at = now() - interval '1 hour' WHERE id = $1")
        .bind(local)
        .execute(&ctx.db)
        .await
        .unwrap();
    let (header, _) = context(&ctx, local, Some(&ctx.alice_token)).await;
    assert!(header.is_none(), "a local status");

    // A fetch in progress is reported to a signed-out viewer too.
    let old = store(&ctx, &base, "old").await;
    let refresh = eunha::async_refresh::AsyncRefresh::create(
        &ctx.state,
        &eunha::federation::replies::refresh_key(old),
        true,
    )
    .await;
    let (header, _) = context(&ctx, old, None).await;
    assert_eq!(
        header.as_deref(),
        Some(format!("id=\"{}\", retry=3, result_count=0", refresh.id(&ctx.state)).as_str())
    );
    // And nothing new is started behind it.
    let (header, _) = context(&ctx, old, Some(&ctx.alice_token)).await;
    assert!(header.is_some());
    let fetched: Option<chrono::NaiveDateTime> =
        sqlx::query_scalar("SELECT fetched_replies_at FROM statuses WHERE id = $1")
            .bind(old)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(fetched.is_none());
}

/// A status arriving in a `Create` has the first page of its replies
/// fetched: replies on the author's server, up to five.
#[tokio::test]
async fn test_create_fetches_first_page_of_replies() {
    let (ctx, remote, base, private_pem) = context_ctx("replies-create").await;
    remote.put(
        "/notes/c/replies/1",
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{base}/notes/c/replies/1"),
            "type": "OrderedCollectionPage",
            "orderedItems": [
                format!("{base}/notes/c1"),
                "https://elsewhere.invalid/notes/x",
                format!("{base}/notes/c2"),
            ],
        }),
    );
    remote.put("/notes/c1", note(&base, "c1", Some("c"), None));
    remote.put("/notes/c2", note(&base, "c2", Some("c"), None));

    let object = note(
        &base,
        "c",
        None,
        Some(json!({
            "id": format!("{base}/notes/c/replies"),
            "type": "Collection",
            "first": format!("{base}/notes/c/replies/1"),
        })),
    );
    let actor = format!("{base}/users/eve");
    // A status is taken only when it concerns someone here: alice follows eve.
    let eve = eunha::api::ap::inbox::resolve_or_fetch_remote_account(&ctx.state, &actor)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(eunha::snowflake::next_id())
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .bind(eve)
    .execute(&ctx.db)
    .await
    .unwrap();
    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{base}/notes/c/activity"),
        "type": "Create",
        "actor": actor,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": object,
    });
    let resp = ctx
        .api
        .post_signed(
            "/inbox",
            &create,
            &format!("{actor}#main-key"),
            &private_pem,
        )
        .await;
    assert!(resp.status().is_success(), "{}", resp.status());

    let mut stored = Vec::new();
    for _ in 0..100 {
        stored = sqlx::query_scalar::<_, String>(
            "SELECT uri FROM statuses WHERE in_reply_to_id = (SELECT id FROM statuses WHERE uri = $1) ORDER BY uri",
        )
        .bind(format!("{base}/notes/c"))
        .fetch_all(&ctx.db)
        .await
        .unwrap();
        if stored.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let parent: Option<i64> = sqlx::query_scalar("SELECT id FROM statuses WHERE uri = $1")
        .bind(format!("{base}/notes/c"))
        .fetch_optional(&ctx.db)
        .await
        .unwrap();
    assert!(parent.is_some(), "the Create was not stored");
    assert_eq!(
        stored,
        [format!("{base}/notes/c1"), format!("{base}/notes/c2")],
        "page fetched {} times, c1 {} times",
        remote.fetches("/notes/c/replies/1"),
        remote.fetches("/notes/c1"),
    );
}
