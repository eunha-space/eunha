//! Verifying a remote post's quote (`ActivityPub::VerifyQuoteService`)
//! against a fake remote server: how its stamp is fetched and read, and how
//! far a chain of quoted posts is followed.
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

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::Router;
use serde_json::{json, Value};

use helpers::TestContext;

/// What the remote server answers at a path.
#[derive(Clone)]
struct Answer {
    status: StatusCode,
    document: Value,
}

/// A remote server: answers by path, and the signature headers of each
/// request made of it.
#[derive(Clone, Default)]
struct Remote {
    answers: Arc<Mutex<HashMap<String, Answer>>>,
    signatures: Arc<Mutex<HashMap<String, Vec<String>>>>,
}

impl Remote {
    fn put(&self, path: &str, document: Value) {
        self.answer(path, StatusCode::OK, document);
    }

    fn answer(&self, path: &str, status: StatusCode, document: Value) {
        self.answers
            .lock()
            .unwrap()
            .insert(path.to_owned(), Answer { status, document });
    }

    /// The signature headers of every request for `path`, as they came.
    fn signatures(&self, path: &str) -> Vec<String> {
        self.signatures
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .unwrap_or_default()
    }

    fn fetched(&self, path: &str) -> bool {
        self.signatures.lock().unwrap().contains_key(path)
    }
}

async fn serve(
    State(remote): State<Remote>,
    uri: Uri,
    headers: HeaderMap,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let path = uri.path().to_owned();
    let signed = ["signature", "signature-input"]
        .iter()
        .filter_map(|name| headers.get(*name)?.to_str().ok())
        .collect::<Vec<_>>()
        .join(" ");
    remote
        .signatures
        .lock()
        .unwrap()
        .entry(path.clone())
        .or_default()
        .push(signed);
    // `acct:{name}@{host}` names `/users/{name}`.
    if path == "/.well-known/webfinger" {
        let resource = uri
            .query()
            .and_then(|q| q.split('&').find_map(|p| p.strip_prefix("resource=")))
            .map(|r| r.replace("%40", "@").replace("%3A", ":"))
            .unwrap_or_default();
        let Some((name, host)) = resource
            .strip_prefix("acct:")
            .and_then(|acct| acct.split_once('@'))
        else {
            return StatusCode::NOT_FOUND.into_response();
        };
        return (
            [("content-type", "application/jrd+json")],
            json!({
                "subject": resource,
                "links": [{
                    "rel": "self",
                    "type": "application/activity+json",
                    "href": format!("http://{host}/users/{name}"),
                }],
            })
            .to_string(),
        )
            .into_response();
    }
    match remote.answers.lock().unwrap().get(&path) {
        Some(answer) => (
            answer.status,
            [("content-type", "application/activity+json")],
            answer.document.to_string(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Starts the remote server with two actors, `eve`, who quotes, and `fay`,
/// who is quoted, and returns its base URL.
async fn spawn_remote(remote: &Remote) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let host = base.trim_start_matches("http://").to_owned();
    for name in ["eve", "fay"] {
        let (_, public_pem) = ojak::sig::signature::generate_rsa_keypair(
            &mut rand::rand_core::UnwrapErr(rand::rngs::SysRng),
        )
        .unwrap();
        let actor = format!("{base}/users/{name}");
        remote.put(
            &format!("/users/{name}"),
            json!({
                "@context": ["https://www.w3.org/ns/activitystreams", "https://w3id.org/security/v1"],
                "id": actor,
                "type": "Person",
                "preferredUsername": name,
                "webfinger": format!("{name}@{host}"),
                "inbox": format!("{actor}/inbox"),
                "outbox": format!("{actor}/outbox"),
                "followers": format!("{actor}/followers"),
                "publicKey": {
                    "id": format!("{actor}#main-key"),
                    "owner": actor,
                    "publicKeyPem": public_pem,
                },
            }),
        );
    }
    let app = Router::new().fallback(serve).with_state(remote.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// A public note by `author` at `/notes/{name}`, quoting `quote` with the
/// stamp `stamp`, when they are given.
fn note(base: &str, author: &str, name: &str, quote: Option<&str>, stamp: Option<&str>) -> Value {
    let mut note = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{base}/notes/{name}"),
        "type": "Note",
        "attributedTo": format!("{base}/users/{author}"),
        "content": format!("<p>{name}</p>"),
        "published": "2026-01-01T00:00:00Z",
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
    });
    if let Some(quote) = quote {
        note["quote"] = json!(format!("{base}/notes/{quote}"));
    }
    if let Some(stamp) = stamp {
        note["quoteAuthorization"] = json!(format!("{base}{stamp}"));
    }
    note
}

/// Fay's stamp at `path`, letting eve's `quoting` quote fay's `quoted`.
fn stamp(base: &str, path: &str, quoting: &str, quoted: &str) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{base}{path}"),
        "type": "QuoteAuthorization",
        "attributedTo": format!("{base}/users/fay"),
        "interactingObject": format!("{base}/notes/{quoting}"),
        "interactionTarget": format!("{base}/notes/{quoted}"),
    })
}

async fn context(label: &str) -> (TestContext, Remote, String) {
    eunha::federation::webfinger::use_plain_http_for_tests();
    let ctx = TestContext::reaching_loopback(label).await;
    let remote = Remote::default();
    let base = spawn_remote(&remote).await;
    (ctx, remote, base)
}

/// Fetches `/notes/{name}` as a search would, and returns its id.
async fn store(ctx: &TestContext, base: &str, name: &str) -> i64 {
    eunha::api::ap::inbox::fetch_remote_status(&ctx.state, &format!("{base}/notes/{name}"))
        .await
        .unwrap()
        .expect("the remote note should be stored")
}

async fn quote_state(ctx: &TestContext, status_id: i64) -> i32 {
    sqlx::query_scalar("SELECT state FROM quotes WHERE status_id = $1")
        .bind(status_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

async fn held(ctx: &TestContext, base: &str, name: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT id FROM statuses WHERE uri = $1")
        .bind(format!("{base}/notes/{name}"))
        .fetch_optional(&ctx.db)
        .await
        .unwrap()
        .is_some()
}

/// Eve's `quoting` quotes fay's `quoted` with the stamp `answer` at
/// `/stamps/1`: the quote's state once eve's post is stored.
async fn verified_with(label: &str, answer: impl FnOnce(&str) -> (StatusCode, Value)) -> i32 {
    let (ctx, remote, base) = context(label).await;
    remote.put("/notes/p", note(&base, "fay", "p", None, None));
    remote.put(
        "/notes/q",
        note(&base, "eve", "q", Some("p"), Some("/stamps/1")),
    );
    let (status, document) = answer(&base);
    remote.answer("/stamps/1", status, document);
    let quoting = store(&ctx, &base, "q").await;
    quote_state(&ctx, quoting).await
}

/// A stamp that says what it should accepts the quote.
#[tokio::test]
async fn test_a_stamp_accepts_the_quote() {
    let state = verified_with("stamp-accepts", |base| {
        (StatusCode::OK, stamp(base, "/stamps/1", "q", "p"))
    })
    .await;
    assert_eq!(state, 1);
}

/// `fetch_resource` reads a stamp only from a `200`: any other success is
/// `nil`, which rejects the quote.
#[tokio::test]
async fn test_a_stamp_not_served_with_200_rejects_the_quote() {
    let state = verified_with("stamp-203", |base| {
        (
            StatusCode::NON_AUTHORITATIVE_INFORMATION,
            stamp(base, "/stamps/1", "q", "p"),
        )
    })
    .await;
    assert_eq!(state, 2);
}

/// A stamp whose author is on another host than the stamp is not one, but
/// does not reject the quote either (`return if non_matching_uri_hosts?`).
#[tokio::test]
async fn test_a_stamp_by_someone_elsewhere_leaves_the_quote_pending() {
    let state = verified_with("stamp-elsewhere", |base| {
        let mut stamp = stamp(base, "/stamps/1", "q", "p");
        stamp["attributedTo"] = json!(base.replace("127.0.0.1", "localhost") + "/users/fay");
        (StatusCode::OK, stamp)
    })
    .await;
    assert_eq!(state, 0);
}

/// A stamp field given as an array is one Mastodon's `value_or_id` raises
/// on: the stamp is never accepted, and the quote stays pending.
#[tokio::test]
async fn test_a_stamp_with_its_author_in_an_array_leaves_the_quote_pending() {
    let state = verified_with("stamp-array", |base| {
        let mut stamp = stamp(base, "/stamps/1", "q", "p");
        stamp["attributedTo"] = json!([stamp["attributedTo"].clone()]);
        (StatusCode::OK, stamp)
    })
    .await;
    assert_eq!(state, 0);
    let state = verified_with("stamp-array-target", |base| {
        let mut stamp = stamp(base, "/stamps/1", "q", "p");
        stamp["interactionTarget"] = json!([stamp["interactionTarget"].clone()]);
        (StatusCode::OK, stamp)
    })
    .await;
    assert_eq!(state, 0);
}

async fn follow(ctx: &TestContext, follower: &str, base: &str, name: &str) {
    let target: i64 = sqlx::query_scalar("SELECT id FROM accounts WHERE uri = $1")
        .bind(format!("{base}/users/{name}"))
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(eunha::snowflake::next_id())
    .bind(follower.parse::<i64>().unwrap())
    .bind(target)
    .execute(&ctx.db)
    .await
    .unwrap();
}

/// The quoted post and the stamp are fetched on behalf of the local account
/// that last followed the quoting one (`followers.local.first`, the newest
/// follow first), and signed with its key.
#[tokio::test]
async fn test_a_quote_is_verified_on_behalf_of_a_local_follower() {
    let (ctx, remote, base) = context("verify-on-behalf").await;
    remote.put("/notes/other", note(&base, "eve", "other", None, None));
    store(&ctx, &base, "other").await;
    for account in [&ctx.alice_id, &ctx.bob_id] {
        let (private_pem, public_pem) = ojak::sig::signature::generate_rsa_keypair(
            &mut rand::rand_core::UnwrapErr(rand::rngs::SysRng),
        )
        .unwrap();
        eunha::federation::keypair::replace_local(
            &ctx.state,
            account.parse().unwrap(),
            &private_pem,
            &public_pem,
        )
        .await
        .unwrap();
    }
    follow(&ctx, &ctx.bob_id, &base, "eve").await;
    follow(&ctx, &ctx.alice_id, &base, "eve").await;
    remote.put("/notes/p", note(&base, "fay", "p", None, None));
    remote.put(
        "/notes/q",
        note(&base, "eve", "q", Some("p"), Some("/stamps/1")),
    );
    remote.put("/stamps/1", stamp(&base, "/stamps/1", "q", "p"));
    let quoting = store(&ctx, &base, "q").await;
    assert_eq!(quote_state(&ctx, quoting).await, 1);
    let alice_key = format!("https://{}/users/alice#main-key", ctx.domain);
    for path in ["/notes/p", "/stamps/1"] {
        let signatures = remote.signatures(path);
        assert!(
            signatures.iter().all(|s| s.contains(&alice_key)),
            "{path}: {signatures:?}"
        );
    }
}

/// `raise RecursionLimitExceededError if @depth > MAX_SYNCHRONOUS_DEPTH`:
/// a post taken in at depth 0 has its quoted posts fetched at depths 1, 2
/// and 3, and the one quoted at depth 3 is not fetched.
#[tokio::test]
async fn test_a_chain_of_quotes_is_fetched_three_deep() {
    let (ctx, remote, base) = context("quote-chain-depth").await;
    for n in 0..5 {
        let quoted = format!("c{}", n + 1);
        remote.put(
            &format!("/notes/c{n}"),
            note(&base, "eve", &format!("c{n}"), Some(&quoted), None),
        );
    }
    store(&ctx, &base, "c0").await;
    for n in 1..=3 {
        assert!(held(&ctx, &base, &format!("c{n}")).await, "c{n}");
    }
    assert!(!held(&ctx, &base, "c4").await);
    assert!(!remote.fetched("/notes/c4"));
}
