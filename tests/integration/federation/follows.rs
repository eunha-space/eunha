//! Follows across servers: an `Accept` or `Reject` of a local account's
//! follow (`ActivityPub::Activity::Accept` and `::Reject`), found among what
//! was asked of the sender alone.

use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn seed_remote(ctx: &TestContext, username: &str) -> (i64, String) {
    let domain = format!("{username}.invalid");
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, '', $4 || '/inbox', $4 || '/outbox', now(), now())"#,
    )
    .bind(id)
    .bind(username)
    .bind(&domain)
    .bind(&uri)
    .execute(&ctx.db)
    .await
    .unwrap();
    (id, uri)
}

/// Gives a local account a key, so that what it sends is signed and queued.
async fn give_key(ctx: &TestContext, id: i64) {
    let (private, public) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    sqlx::query("UPDATE accounts SET private_key = $1, public_key = $2 WHERE id = $3")
        .bind(&private)
        .bind(&public)
        .bind(id)
        .execute(&ctx.db)
        .await
        .unwrap();
}

async fn request(ctx: &TestContext, from: i64, to: i64, uri: &str) -> i64 {
    sqlx::query_scalar(
        r#"INSERT INTO follow_requests (account_id, target_account_id, uri, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now()) RETURNING id"#,
    )
    .bind(from)
    .bind(to)
    .bind(uri)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

async fn follow(ctx: &TestContext, from: i64, to: i64, uri: &str) {
    sqlx::query(
        r#"INSERT INTO follows (account_id, target_account_id, uri, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())"#,
    )
    .bind(from)
    .bind(to)
    .bind(uri)
    .execute(&ctx.db)
    .await
    .unwrap();
}

async fn exists(ctx: &TestContext, table: &str, from: i64, to: i64) -> bool {
    sqlx::query_scalar::<_, bool>(&format!(
        "SELECT EXISTS (SELECT 1 FROM {table} WHERE account_id = $1 AND target_account_id = $2)"
    ))
    .bind(from)
    .bind(to)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

async fn receive(ctx: &TestContext, activity: Value) {
    eunha::api::ap::inbox::received(&ctx.state, activity)
        .await
        .unwrap();
}

fn answer(kind: &str, actor: &str, object: Value) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{actor}#answers/{}", eunha::snowflake::next_id()),
        "type": kind,
        "actor": actor,
        "object": object,
    })
}

/// Queued deliveries of activities of a type: each activity and its inbox.
async fn queued(ctx: &TestContext, kind: &str) -> Vec<(Value, String)> {
    sqlx::query_as(
        r#"SELECT payload->'activity', payload->>'inbox' FROM eunha.ojak_queue
           WHERE queue IN ('delivery', 'delivery-priority')
             AND payload->'activity'->>'type' = $1
           ORDER BY id"#,
    )
    .bind(kind)
    .fetch_all(&ctx.db)
    .await
    .unwrap()
}

/// Only the account asked can answer a follow request: an `Accept` or a
/// `Reject` by someone else, naming the request's id, leaves it pending
/// (`FollowRequest.find_by(target_account: @account, uri:)`).
#[tokio::test]
async fn test_only_the_followed_account_answers_a_follow_request() {
    let ctx = TestContext::new("follows-answer-scope").await;
    ctx.state.jobs.set_mode(eunha::jobs::Mode::Durable);
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let (rita, rita_uri) = seed_remote(&ctx, "rita").await;
    let (_mallory, mallory_uri) = seed_remote(&ctx, "mallory").await;
    let follow_uri = format!("https://{}/some-follow", ctx.domain);
    request(&ctx, alice, rita, &follow_uri).await;

    receive(&ctx, answer("Reject", &mallory_uri, json!(follow_uri))).await;
    receive(&ctx, answer("Accept", &mallory_uri, json!(follow_uri))).await;
    assert!(exists(&ctx, "follow_requests", alice, rita).await);
    assert!(!exists(&ctx, "follows", alice, rita).await);

    // The account asked accepts: the request becomes a follow, which carries
    // the request's id, and the account, now followed from here for the first
    // time, is fetched again.
    receive(&ctx, answer("Accept", &rita_uri, json!(follow_uri))).await;
    assert!(!exists(&ctx, "follow_requests", alice, rita).await);
    let uri: Option<String> = sqlx::query_scalar(
        "SELECT uri FROM follows WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(alice)
    .bind(rita)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(uri.as_deref(), Some(follow_uri.as_str()));
    let refreshes = eunha::jobs::queued(&ctx.state, "RemoteAccountRefreshWorker")
        .await
        .unwrap();
    assert_eq!(refreshes.len(), 1);
    assert_eq!(refreshes[0].args, json!({"account_id": rita}));
}

/// A second local follower does not fetch the account again: only the first
/// does (`is_first_follow`).
#[tokio::test]
async fn test_an_accept_refreshes_the_account_only_for_its_first_follower() {
    let ctx = TestContext::new("follows-accept-refresh").await;
    ctx.state.jobs.set_mode(eunha::jobs::Mode::Durable);
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    let (rita, rita_uri) = seed_remote(&ctx, "rita").await;
    follow(
        &ctx,
        bob,
        rita,
        &format!("https://{}/bobs-follow", ctx.domain),
    )
    .await;
    let follow_uri = format!("https://{}/alices-follow", ctx.domain);
    request(&ctx, alice, rita, &follow_uri).await;

    receive(&ctx, answer("Accept", &rita_uri, json!(follow_uri))).await;
    assert!(exists(&ctx, "follows", alice, rita).await);
    assert!(
        eunha::jobs::queued(&ctx.state, "RemoteAccountRefreshWorker")
            .await
            .unwrap()
            .is_empty()
    );
}

/// A `Reject` of a follow already accepted removes the follower, as
/// Mastodon's "remove follower" sends it: `UnfollowService`, which tells the
/// sender with an `Undo` of the follow. Someone else's `Reject` does nothing.
#[tokio::test]
async fn test_a_reject_of_an_accepted_follow_unfollows() {
    let ctx = TestContext::new("follows-reject-follow").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    give_key(&ctx, alice).await;
    let (rita, rita_uri) = seed_remote(&ctx, "rita").await;
    let (_mallory, mallory_uri) = seed_remote(&ctx, "mallory").await;
    let follow_uri = format!("https://{}/a-follow", ctx.domain);
    follow(&ctx, alice, rita, &follow_uri).await;

    receive(&ctx, answer("Reject", &mallory_uri, json!(follow_uri))).await;
    assert!(exists(&ctx, "follows", alice, rita).await);

    receive(&ctx, answer("Reject", &rita_uri, json!(follow_uri))).await;
    assert!(!exists(&ctx, "follows", alice, rita).await);
    let undos = queued(&ctx, "Undo").await;
    assert_eq!(undos.len(), 1, "{undos:?}");
    assert_eq!(undos[0].0["object"]["id"], follow_uri.as_str());
    assert_eq!(undos[0].0["object"]["object"], rita_uri.as_str());
}

/// A Follow whose id is not known here is found by who asked whom: an
/// embedded Follow from a local account to the sender (`accept_embedded_follow`
/// and `reject_embedded_follow`).
#[tokio::test]
async fn test_an_embedded_follow_is_answered_by_its_accounts() {
    let ctx = TestContext::new("follows-embedded").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    let alice_uri = format!("https://{}/users/alice", ctx.domain);
    let bob_uri = format!("https://{}/users/bob", ctx.domain);
    let (rita, rita_uri) = seed_remote(&ctx, "rita").await;
    request(&ctx, alice, rita, &format!("https://{}/one", ctx.domain)).await;
    request(&ctx, bob, rita, &format!("https://{}/two", ctx.domain)).await;

    let embedded = |actor: &str| {
        json!({
            "id": "https://rita.invalid/an-id-we-never-sent",
            "type": "Follow",
            "actor": actor,
            "object": rita_uri,
        })
    };
    receive(&ctx, answer("Accept", &rita_uri, embedded(&alice_uri))).await;
    assert!(exists(&ctx, "follows", alice, rita).await);
    assert!(exists(&ctx, "follow_requests", bob, rita).await);

    // A Reject withdraws a request, and ends a follow.
    receive(&ctx, answer("Reject", &rita_uri, embedded(&bob_uri))).await;
    assert!(!exists(&ctx, "follow_requests", bob, rita).await);
    receive(&ctx, answer("Reject", &rita_uri, embedded(&alice_uri))).await;
    assert!(!exists(&ctx, "follows", alice, rita).await);
}

async fn block_uri(ctx: &TestContext, from: i64, to: i64) -> Option<String> {
    sqlx::query_scalar("SELECT uri FROM blocks WHERE account_id = $1 AND target_account_id = $2")
        .bind(from)
        .bind(to)
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

/// An inbound `Block` of a local account, named by its path as a local
/// account Mastodon made has no `uri`: the follows between them end as
/// `UnfollowService` ends them, telling the blocker (a `Reject` of its
/// follow, an `Undo` of ours), the local account's request to follow it is
/// withdrawn, and the block keeps the activity's id. Delivered again, it only
/// takes the new id.
#[tokio::test]
async fn test_an_inbound_block_ends_follows_as_unfollow_service_does() {
    let ctx = TestContext::new("follows-inbound-block").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    give_key(&ctx, alice).await;
    let alice_uri = format!("https://{}/users/alice", ctx.domain);
    let bob_uri = format!("https://{}/users/bob", ctx.domain);
    let (rita, rita_uri) = seed_remote(&ctx, "rita").await;
    let ritas_follow = "https://rita.invalid/follows/1";
    let alices_follow = format!("https://{}/alices-follow", ctx.domain);
    follow(&ctx, rita, alice, ritas_follow).await;
    follow(&ctx, alice, rita, &alices_follow).await;
    let ritas_follow_id: i64 = sqlx::query_scalar(
        "SELECT id FROM follows WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(rita)
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    request(
        &ctx,
        bob,
        rita,
        &format!("https://{}/bobs-request", ctx.domain),
    )
    .await;

    let block = |id: &str, object: &str| {
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": id,
            "type": "Block",
            "actor": rita_uri,
            "object": object,
        })
    };
    receive(&ctx, block("https://rita.invalid/blocks/1", &alice_uri)).await;
    assert!(!exists(&ctx, "follows", rita, alice).await);
    assert!(!exists(&ctx, "follows", alice, rita).await);
    assert_eq!(
        block_uri(&ctx, rita, alice).await.as_deref(),
        Some("https://rita.invalid/blocks/1")
    );

    let rejects = queued(&ctx, "Reject").await;
    assert_eq!(rejects.len(), 1, "{rejects:?}");
    let (reject, inbox) = &rejects[0];
    assert_eq!(inbox, &format!("{rita_uri}/inbox"));
    assert_eq!(
        reject["id"],
        format!("{alice_uri}#rejects/follows/{ritas_follow_id}")
    );
    assert_eq!(reject["object"]["id"], ritas_follow);
    let undos = queued(&ctx, "Undo").await;
    assert_eq!(undos.len(), 1, "{undos:?}");
    assert_eq!(undos[0].0["object"]["id"], alices_follow.as_str());

    receive(&ctx, block("https://rita.invalid/blocks/2", &bob_uri)).await;
    assert!(!exists(&ctx, "follow_requests", bob, rita).await);
    assert!(exists(&ctx, "blocks", rita, bob).await);

    // Delivered again under another id, the block only takes it.
    follow(&ctx, alice, rita, &alices_follow).await;
    receive(&ctx, block("https://rita.invalid/blocks/3", &alice_uri)).await;
    assert!(exists(&ctx, "follows", alice, rita).await);
    assert_eq!(
        block_uri(&ctx, rita, alice).await.as_deref(),
        Some("https://rita.invalid/blocks/3")
    );
}
