//! Linked Data Signatures (`RsaSignature2017`), as Mastodon makes and checks
//! them: on what a relay or a forwarding server passes on, so that the
//! servers it reaches can still tell who wrote it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::helpers::TestContext;

/// A remote account with a key, known here as Mastodon would know it.
async fn seed_remote(ctx: &TestContext, username: &str, domain: &str) -> (i64, String, String) {
    let (priv_pem, pub_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $5, $4 || '/inbox', $4 || '/outbox', now(), now())"#,
    )
    .bind(id)
    .bind(username)
    .bind(domain)
    .bind(&uri)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();
    (id, uri, priv_pem)
}

/// Give alice a key and a remote follower, so that what she posts is queued.
async fn alice_with_a_follower(ctx: &TestContext) -> (i64, String) {
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let (priv_pem, pub_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    sqlx::query("UPDATE accounts SET private_key = $2, public_key = $3 WHERE id = $1")
        .bind(alice_id)
        .bind(&priv_pem)
        .bind(&pub_pem)
        .execute(&ctx.db)
        .await
        .unwrap();
    let (nina_id, _, _) = seed_remote(ctx, "nina", "nina.invalid").await;
    sqlx::query(
        "INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(eunha::snowflake::next_id())
    .bind(nina_id)
    .bind(alice_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    (alice_id, pub_pem)
}

/// The activities queued for delivery, newest last.
async fn queued(ctx: &TestContext, kind: &str) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT payload->'activity' FROM eunha.ojak_queue
         WHERE queue IN ('delivery', 'delivery-priority') AND payload->'activity'->>'type' = $1
         ORDER BY id",
    )
    .bind(kind)
    .fetch_all(&ctx.db)
    .await
    .unwrap()
}

fn verify(activity: &Value, public_key: &str) -> Result<(), ojak::sig::Error> {
    ojak::sig::linked_data::verify(
        &ojak_jsonld::Registry::bundled(),
        activity,
        public_key,
        chrono::Utc::now().timestamp(),
    )
}

/// A public post goes with alice's Linked Data Signature, which verifies
/// against the key she publishes; a followers-only one goes without, as
/// `Status#sign?` is `distributable?`.
#[tokio::test]
async fn test_public_posts_go_signed_and_private_ones_do_not() {
    let ctx = TestContext::new("ldsig-out").await;
    let (_, public_key) = alice_with_a_follower(&ctx).await;

    ctx.api
        .post_status(&ctx.alice_token, "for everyone", "public")
        .await;
    let creates = queued(&ctx, "Create").await;
    let create = creates.last().expect("a Create was queued");
    let signature = &create["signature"];
    assert_eq!(signature["type"], "RsaSignature2017");
    let actor = create["actor"].as_str().unwrap();
    assert_eq!(signature["creator"], format!("{actor}#main-key"));
    assert!(
        create["@context"]
            .as_array()
            .is_some_and(|context| context.contains(&json!("https://w3id.org/security/v1"))),
        "the security context travels with the signature: {}",
        create["@context"]
    );
    verify(create, &public_key).expect("alice's signature verifies");
    let mut changed = create.clone();
    changed["object"]["content"] = json!("<p>something else</p>");
    assert!(verify(&changed, &public_key).is_err());

    ctx.api
        .post_status(&ctx.alice_token, "for followers", "private")
        .await;
    let creates = queued(&ctx, "Create").await;
    assert_eq!(creates.len(), 2);
    assert!(
        creates[1].get("signature").is_none(),
        "a followers-only post is not signed"
    );

    // Her profile goes signed too (`Account#sign?`).
    eunha::accounts::update_profiles(
        &ctx.state,
        &eunha::accounts::Selection::All,
        &Default::default(),
        false,
    )
    .await
    .unwrap();
    let update = queued(&ctx, "Update").await.pop().expect("an Update");
    verify(&update, &public_key).expect("the profile Update verifies");
}

/// In authorized fetch mode Mastodon signs only what it must
/// (`always_sign`): a post goes unsigned, its deletion signed.
#[tokio::test]
async fn test_authorized_fetch_signs_only_deletions() {
    let ctx = TestContext::new("ldsig-secure").await;
    crate::helpers::set_setting(&ctx.db, "authorized_fetch", "true").await;
    let (_, public_key) = alice_with_a_follower(&ctx).await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "for everyone, fetched signed", "public")
        .await;
    let create = queued(&ctx, "Create").await.pop().expect("a Create");
    assert!(create.get("signature").is_none());

    let id = status["id"].as_str().unwrap();
    let resp = ctx
        .api
        .delete(&format!("/api/v1/statuses/{id}"), &ctx.alice_token)
        .await;
    assert!(resp.status().is_success(), "{}", resp.status());
    let delete = queued(&ctx, "Delete").await.pop().expect("a Delete");
    verify(&delete, &public_key).expect("the deletion is signed whatever the mode");
}

/// A Create signed by bob, delivered by a relay this server subscribes to:
/// taken on bob's signature, though nobody here follows him. Changed on the
/// way, it is dropped.
#[tokio::test]
async fn test_a_relayed_post_is_taken_on_its_authors_signature() {
    let ctx = TestContext::new("ldsig-in").await;
    let (_, bob, bob_key) = seed_remote(&ctx, "bob", "bob.invalid").await;
    let (_, relay, relay_key) = seed_remote(&ctx, "relay", "relay.invalid").await;
    sqlx::query(
        "INSERT INTO relays (inbox_url, state, created_at, updated_at) VALUES ($1, 2, now(), now())",
    )
    .bind(format!("{relay}/inbox"))
    .execute(&ctx.db)
    .await
    .unwrap();

    let create = |n: u32| {
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
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
                "content": "<p>passed on by a relay</p>",
                "published": "2026-10-01T12:00:00Z",
            },
        })
    };
    let sign = |activity: &Value| {
        let now = chrono::Utc::now().timestamp();
        ojak::sig::linked_data::sign(
            &ojak_jsonld::Registry::bundled(),
            activity,
            &format!("{bob}#main-key"),
            &ojak::sig::PrivateKey::from_pem(&bob_key).unwrap(),
            now,
            now + ojak::sig::linked_data::DEFAULT_LIFETIME_SECONDS,
        )
        .unwrap()
    };
    let stored = |n: u32| {
        let uri = format!("{bob}/statuses/{n}");
        let db = ctx.db.clone();
        async move {
            sqlx::query_as::<_, (String, i32)>(
                "SELECT text, visibility FROM statuses WHERE uri = $1",
            )
            .bind(uri)
            .fetch_optional(&db)
            .await
            .unwrap()
        }
    };
    let relay_key_id = format!("{relay}#main-key");

    let signed = sign(&create(1));
    let resp = ctx
        .api
        .post_signed("/inbox", &signed, &relay_key_id, &relay_key)
        .await;
    assert_eq!(resp.status(), 202);
    let (text, visibility) = stored(1).await.expect("the relayed post was taken");
    assert!(text.contains("passed on by a relay"), "{text}");
    assert_eq!(visibility, 0, "public");

    let mut changed = sign(&create(2));
    changed["object"]["content"] = json!("<p>changed by the relay</p>");
    let resp = ctx
        .api
        .post_signed("/inbox", &changed, &relay_key_id, &relay_key)
        .await;
    assert_eq!(resp.status(), 202, "dropped, as Mastodon drops it");
    assert!(stored(2).await.is_none(), "a changed post is not taken");

    // Naming a context ojak does not ship, it is fetched to check the
    // signature, as Mastodon fetches it (tests/linked_data_contexts.rs);
    // one that defines nothing would leave the signature holding. But one
    // on a private address is refused, as Mastodon's `Request` refuses it,
    // without a request being made, and the post is not taken.
    let requests = Arc::new(AtomicUsize::new(0));
    let counted = requests.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let private = format!("http://{}/empty", listener.local_addr().unwrap());
    let app = axum::Router::new().fallback(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        async {
            (
                [("content-type", "application/ld+json")],
                r#"{"@context": {}}"#,
            )
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for context in [private.as_str(), "https://contexts.invalid/empty"] {
        let mut elsewhere = sign(&create(4));
        elsewhere["@context"] = json!([
            "https://www.w3.org/ns/activitystreams",
            "https://w3id.org/security/v1",
            context
        ]);
        let resp = ctx
            .api
            .post_signed("/inbox", &elsewhere, &relay_key_id, &relay_key)
            .await;
        assert_eq!(resp.status(), 202);
        assert!(stored(4).await.is_none(), "{context} is not fetched");
    }
    assert_eq!(requests.load(Ordering::SeqCst), 0);

    // Unsigned, from a relay: bob's server cannot be asked, so it is not
    // taken either.
    let resp = ctx
        .api
        .post_signed("/inbox", &create(3), &relay_key_id, &relay_key)
        .await;
    assert_eq!(resp.status(), 202);
    assert!(stored(3).await.is_none());
}

/// Signed by bob and passed on by a server that is not a relay this server
/// subscribes to, a post nobody here follows him for is authenticated, and
/// still not taken: `requested_through_relay?` is false.
#[tokio::test]
async fn test_a_signed_post_from_elsewhere_needs_a_reason_to_be_taken() {
    let ctx = TestContext::new("ldsig-norelay").await;
    let (_, bob, bob_key) = seed_remote(&ctx, "bob", "bob.invalid").await;
    let (_, carol, carol_key) = seed_remote(&ctx, "carol", "carol.invalid").await;
    let activity = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{bob}/statuses/1/activity"),
        "type": "Create",
        "actor": bob,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("{bob}/statuses/1"),
            "type": "Note",
            "attributedTo": bob,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "content": "<p>forwarded</p>",
        },
    });
    let now = chrono::Utc::now().timestamp();
    let signed = ojak::sig::linked_data::sign(
        &ojak_jsonld::Registry::bundled(),
        &activity,
        &format!("{bob}#main-key"),
        &ojak::sig::PrivateKey::from_pem(&bob_key).unwrap(),
        now,
        now + 60,
    )
    .unwrap();
    // A sender cannot say it came through a relay.
    let mut claimed = signed.clone();
    claimed[eunha_through_relay()] = json!(true);
    for activity in [&signed, &claimed] {
        let resp = ctx
            .api
            .post_signed("/inbox", activity, &format!("{carol}#main-key"), &carol_key)
            .await;
        assert_eq!(resp.status(), 202);
    }
    let taken: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses WHERE uri = $1")
        .bind(format!("{bob}/statuses/1"))
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(taken, 0);
}

/// The key eunha marks a relayed activity with.
fn eunha_through_relay() -> &'static str {
    "eunha:requestedThroughRelay"
}

/// A relayed post signed by an author whose key `ProcessAccountService`
/// moved into `keypairs`, leaving `accounts.public_key` blank as Mastodon
/// 4.7 leaves it, is checked against that stored key.
#[tokio::test]
async fn test_a_relayed_post_is_checked_against_the_authors_keypair() {
    let ctx = TestContext::new("ldsig-keypair").await;
    let (bob_id, bob, bob_key) = seed_remote(&ctx, "bob", "bob.invalid").await;
    let (_, relay, relay_key) = seed_remote(&ctx, "relay", "relay.invalid").await;
    sqlx::query(
        "INSERT INTO keypairs (account_id, uri, type, public_key, created_at, updated_at)
         SELECT id, $2, 0, public_key, now(), now() FROM accounts WHERE id = $1",
    )
    .bind(bob_id)
    .bind(format!("{bob}#main-key"))
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query("UPDATE accounts SET public_key = '' WHERE id = $1")
        .bind(bob_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO relays (inbox_url, state, created_at, updated_at) VALUES ($1, 2, now(), now())",
    )
    .bind(format!("{relay}/inbox"))
    .execute(&ctx.db)
    .await
    .unwrap();

    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{bob}/statuses/1/activity"),
        "type": "Create",
        "actor": bob,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "cc": [format!("{bob}/followers")],
        "object": {
            "id": format!("{bob}/statuses/1"),
            "type": "Note",
            "attributedTo": bob,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [format!("{bob}/followers")],
            "content": "<p>signed with a stored keypair</p>",
            "published": "2026-10-01T12:00:00Z",
        },
    });
    let now = chrono::Utc::now().timestamp();
    let signed = ojak::sig::linked_data::sign(
        &ojak_jsonld::Registry::bundled(),
        &create,
        &format!("{bob}#main-key"),
        &ojak::sig::PrivateKey::from_pem(&bob_key).unwrap(),
        now,
        now + ojak::sig::linked_data::DEFAULT_LIFETIME_SECONDS,
    )
    .unwrap();
    let resp = ctx
        .api
        .post_signed("/inbox", &signed, &format!("{relay}#main-key"), &relay_key)
        .await;
    assert_eq!(resp.status(), 202);
    let taken: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses WHERE uri = $1")
        .bind(format!("{bob}/statuses/1"))
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(taken, 1, "bob's stored keypair verified the signature");
}
