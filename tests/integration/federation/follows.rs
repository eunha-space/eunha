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

/// A Follow repeated while its request is pending only gives the request its
/// new id, before anything else: no second notification, and no `Reject`
/// even once the local account has blocked the requester.
#[tokio::test]
async fn test_a_repeated_follow_only_renames_a_pending_request() {
    let ctx = TestContext::new("follows-repeat-request").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    give_key(&ctx, alice).await;
    sqlx::query("UPDATE accounts SET locked = true WHERE id = $1")
        .bind(alice)
        .execute(&ctx.db)
        .await
        .unwrap();
    let alice_uri = format!("https://{}/users/alice", ctx.domain);
    let (rita, rita_uri) = seed_remote(&ctx, "rita").await;
    let follow_activity = |id: &str| {
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": id,
            "type": "Follow",
            "actor": rita_uri,
            "object": alice_uri,
        })
    };
    let notifications = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM notifications WHERE account_id = $1 AND type = 'follow_request'",
        )
        .bind(alice)
        .fetch_one(&ctx.db)
        .await
        .unwrap()
    };

    receive(&ctx, follow_activity("https://rita.invalid/follows/1")).await;
    assert!(exists(&ctx, "follow_requests", rita, alice).await);
    assert_eq!(notifications().await, 1);

    sqlx::query(
        "INSERT INTO blocks (account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, now(), now())",
    )
    .bind(alice)
    .bind(rita)
    .execute(&ctx.db)
    .await
    .unwrap();
    receive(&ctx, follow_activity("https://rita.invalid/follows/2")).await;
    let uri: Option<String> = sqlx::query_scalar(
        "SELECT uri FROM follow_requests WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(rita)
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(uri.as_deref(), Some("https://rita.invalid/follows/2"));
    assert_eq!(notifications().await, 1);
    assert!(queued(&ctx, "Reject").await.is_empty());
}

/// Unlocking an account authorizes its requests as `AuthorizeFollowWorker`
/// does (`AuthorizeFollowService`, `FollowRequest#authorize!`): each follow
/// keeps its request's options and id, a remote requester is sent an
/// `Accept` of it, a list membership that waited on a request now holds the
/// follow, and no notification of a new follower is made.
#[tokio::test]
async fn test_unlocking_authorizes_requests_as_authorize_follow_service_does() {
    let ctx = TestContext::new("follows-unlock").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    give_key(&ctx, alice).await;
    let alice_uri = format!("https://{}/users/alice", ctx.domain);
    sqlx::query("UPDATE accounts SET locked = true WHERE id = $1")
        .bind(alice)
        .execute(&ctx.db)
        .await
        .unwrap();
    let (rita, rita_uri) = seed_remote(&ctx, "rita").await;
    let ritas_request = request(&ctx, rita, alice, "https://rita.invalid/follows/1").await;
    sqlx::query(
        "UPDATE follow_requests SET show_reblogs = false, notify = true, languages = '{en}'
         WHERE id = $1",
    )
    .bind(ritas_request)
    .execute(&ctx.db)
    .await
    .unwrap();
    let bobs_request = request(&ctx, bob, alice, &format!("https://{}/bobs", ctx.domain)).await;
    let list_id: i64 = sqlx::query_scalar(
        "INSERT INTO lists (account_id, title, created_at, updated_at)
         VALUES ($1, 'friends', now(), now()) RETURNING id",
    )
    .bind(bob)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO list_accounts (list_id, account_id, follow_request_id)
         VALUES ($1, $2, $3)",
    )
    .bind(list_id)
    .bind(alice)
    .bind(bobs_request)
    .execute(&ctx.db)
    .await
    .unwrap();

    ctx.api
        .patch_json(
            "/api/v1/accounts/update_credentials",
            Some(&ctx.alice_token),
            &json!({"locked": false}),
        )
        .await;
    ctx.state.jobs.settle().await;

    let (uri, show_reblogs, notify, languages): (Option<String>, bool, bool, Option<Vec<String>>) =
        sqlx::query_as(
            "SELECT uri, show_reblogs, notify, languages FROM follows
             WHERE account_id = $1 AND target_account_id = $2",
        )
        .bind(rita)
        .bind(alice)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(uri.as_deref(), Some("https://rita.invalid/follows/1"));
    assert!(!show_reblogs);
    assert!(notify);
    assert_eq!(languages, Some(vec!["en".to_owned()]));
    let accepts = queued(&ctx, "Accept").await;
    assert_eq!(accepts.len(), 1, "{accepts:?}");
    let (accept, inbox) = &accepts[0];
    assert_eq!(inbox, &format!("{rita_uri}/inbox"));
    assert_eq!(
        accept["id"],
        format!("{alice_uri}#accepts/follows/{ritas_request}")
    );
    assert_eq!(accept["object"]["id"], "https://rita.invalid/follows/1");

    let follow_id: Option<i64> =
        sqlx::query_scalar("SELECT follow_id FROM list_accounts WHERE list_id = $1")
            .bind(list_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(follow_id.is_some(), "the membership holds the follow");
    let follows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE account_id = $1 AND type = 'follow'",
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(follows, 0);
}

/// Whether `uri` is what `generate_uri_for` makes: a UUID under the root.
fn generated(ctx: &TestContext, uri: Option<&str>) -> bool {
    uri.and_then(|uri| uri.strip_prefix(&format!("https://{}/", ctx.domain)))
        .is_some_and(|rest| uuid::Uuid::parse_str(rest).is_ok())
}

async fn uri_of(ctx: &TestContext, table: &str, from: i64, to: i64) -> Option<String> {
    sqlx::query_scalar(&format!(
        "SELECT uri FROM {table} WHERE account_id = $1 AND target_account_id = $2"
    ))
    .bind(from)
    .bind(to)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

/// Follows, requests and blocks made here get an id of their own
/// (`before_validation :set_uri, on: :create`), which the activities about
/// them carry: the `Follow` sent is the request's id, the `Block` the
/// block's, and the `Undo` of a block names it, as the serializers do.
#[tokio::test]
async fn test_local_rows_get_a_uri_the_activities_carry() {
    let ctx = TestContext::new("follows-local-uris").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    give_key(&ctx, alice).await;
    let alice_uri = format!("https://{}/users/alice", ctx.domain);
    let (rita, rita_uri) = seed_remote(&ctx, "rita").await;

    // A local follow.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{bob}/follow"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert!(generated(
        &ctx,
        uri_of(&ctx, "follows", alice, bob).await.as_deref()
    ));

    // A remote one is a request whose id the Follow carries.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{rita}/follow"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    let request_uri = uri_of(&ctx, "follow_requests", alice, rita).await;
    assert!(generated(&ctx, request_uri.as_deref()));
    let follows = queued(&ctx, "Follow").await;
    assert_eq!(follows.len(), 1, "{follows:?}");
    assert_eq!(follows[0].0["id"], request_uri.as_deref().unwrap());

    // The Block carries the block's id. `BlockService` asks only whether
    // the blocker follows, so its own request is left, as upstream leaves it.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{rita}/block"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    let block_uri = uri_of(&ctx, "blocks", alice, rita).await;
    assert!(generated(&ctx, block_uri.as_deref()));
    assert!(exists(&ctx, "follow_requests", alice, rita).await);
    let blocks = queued(&ctx, "Block").await;
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    assert_eq!(blocks[0].0["id"], block_uri.as_deref().unwrap());
    assert_eq!(blocks[0].0["object"], rita_uri.as_str());
    let block_id: i64 = sqlx::query_scalar(
        "SELECT id FROM blocks WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(alice)
    .bind(rita)
    .fetch_one(&ctx.db)
    .await
    .unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{rita}/unblock"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    let undos = queued(&ctx, "Undo").await;
    assert_eq!(undos.len(), 1, "{undos:?}");
    assert_eq!(
        undos[0].0["id"],
        format!("{alice_uri}#blocks/{block_id}/undo")
    );
    assert_eq!(undos[0].0["object"]["id"], block_uri.as_deref().unwrap());
}

/// A Follow of an unlocked account is held as a request and authorized
/// (`AuthorizeFollowService`), so the `Accept` is named after the request.
#[tokio::test]
async fn test_an_inbound_follow_is_accepted_by_its_request() {
    let ctx = TestContext::new("follows-inbound-accept").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    give_key(&ctx, alice).await;
    let alice_uri = format!("https://{}/users/alice", ctx.domain);
    let (rita, rita_uri) = seed_remote(&ctx, "rita").await;
    receive(
        &ctx,
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": "https://rita.invalid/follows/1",
            "type": "Follow",
            "actor": rita_uri,
            "object": alice_uri,
        }),
    )
    .await;
    assert_eq!(
        uri_of(&ctx, "follows", rita, alice).await.as_deref(),
        Some("https://rita.invalid/follows/1")
    );
    let accepts = queued(&ctx, "Accept").await;
    assert_eq!(accepts.len(), 1, "{accepts:?}");
    let id = accepts[0].0["id"].as_str().unwrap();
    let request_id: i64 = id
        .strip_prefix(&format!("{alice_uri}#accepts/follows/"))
        .and_then(|id| id.parse().ok())
        .unwrap_or_else(|| panic!("{id}"));
    assert!(request_id > 0);
    assert_eq!(
        accepts[0].0["object"]["id"],
        "https://rita.invalid/follows/1"
    );
}

/// `UnfollowService` runs under `with_redis_lock("relationship:<a>:<b>")`,
/// the two ids smaller first: while another holds it, an unfollow is a
/// `RaceConditionError`, a 503, and leaves the follow.
#[tokio::test]
async fn test_an_unfollow_waits_for_the_relationship_lock() {
    let ctx = TestContext::new("follows-unfollow-lock").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    follow(&ctx, alice, bob, &format!("https://{}/f", ctx.domain)).await;

    let name = format!("lock:relationship:{}:{}", alice.min(bob), alice.max(bob));
    let held = eunha::redis_lock::try_acquire(&ctx.state, &name, 60_000)
        .await
        .expect("the lock is free");
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{bob}/unfollow"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), 503);
    assert!(exists(&ctx, "follows", alice, bob).await);

    drop(held);
    // The guard releases in a task of its own.
    let mut released = false;
    for _ in 0..50 {
        if let Some(lock) = eunha::redis_lock::try_acquire(&ctx.state, &name, 60_000).await {
            drop(lock);
            released = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(released);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{bob}/unfollow"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), 200);
    assert!(!exists(&ctx, "follows", alice, bob).await);
}
