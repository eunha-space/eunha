//! The ingress queue that carries inbound activities off the request path:
//! `ActivityPub::ProcessingWorker` jobs in the job queue.
//!
//! `TestContext` puts the inbox in sync mode so the rest of the suite can
//! assert on an activity right after POSTing it, which means the queued path
//! needs its own coverage. These tests drive the worker directly: queue a job
//! the way the handler would, drain the ingress jobs, and check both the
//! activity's effect and the job's bookkeeping.

use serde_json::json;

use crate::helpers::TestContext;

/// Insert a remote account so handlers resolve its actor without a network
/// fetch (the test domains are unreachable by design).
async fn seed_remote_actor(ctx: &TestContext, username: &str, domain: &str) -> String {
    let uri = format!("https://{domain}/users/{username}");
    sqlx::query!(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, protocol, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4::text, $4::text, 'remote-key',
                   $4::text||'/inbox', $4::text||'/outbox', 1, now(), now())"#,
        eunha::snowflake::next_id(),
        username,
        domain,
        uri,
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    uri
}

/// A queued activity is claimed, dispatched to its handler, and the job row is
/// removed once it succeeds.
#[tokio::test]
async fn test_queued_activity_is_processed_and_job_removed() {
    let ctx = TestContext::new("ingress-follow").await;
    let actor_uri = seed_remote_actor(&ctx, "eve", "ingress-remote.invalid").await;

    let follow = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://ingress-remote.invalid/activities/1",
        "type": "Follow",
        "actor": actor_uri,
        "object": format!("https://{}/users/alice", ctx.domain),
    });

    eunha::api::ap::inbox::queue_activity(&ctx.db, &follow)
        .await
        .unwrap();

    let processed = eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .expect("draining the ingress queue must not error");
    assert_eq!(processed, 1, "the queued Follow must be claimed");

    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let follows: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM follows f
           JOIN accounts a ON a.id = f.account_id
           WHERE a.uri = $1 AND f.target_account_id = $2"#,
        actor_uri,
        alice_id,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(follows, 1, "the queued Follow must take effect");

    let remaining: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM eunha.jobs
               WHERE kind = 'ActivityPub::ProcessingWorker'"#
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(remaining, 0, "a succeeded job must be deleted");
}

/// Draining an empty queue is a no-op, and a job scheduled for the future is
/// left alone rather than claimed early.
#[tokio::test]
async fn test_queue_respects_run_at() {
    let ctx = TestContext::new("ingress-runat").await;
    let actor_uri = seed_remote_actor(&ctx, "mallory", "ingress-later.invalid").await;

    assert_eq!(
        eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
            .await
            .unwrap(),
        0,
        "an empty queue must drain to nothing"
    );

    let follow = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://ingress-later.invalid/activities/1",
        "type": "Follow",
        "actor": actor_uri,
        "object": format!("https://{}/users/alice", ctx.domain),
    });
    eunha::api::ap::inbox::queue_activity(&ctx.db, &follow)
        .await
        .unwrap();
    // As a retry backed off for an hour is.
    sqlx::query!("UPDATE eunha.jobs SET run_at = now() + interval '1 hour'")
        .execute(&ctx.db)
        .await
        .unwrap();

    assert_eq!(
        eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
            .await
            .unwrap(),
        0,
        "a backed-off job must not be claimed before its run_at"
    );

    let remaining: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM eunha.jobs
               WHERE kind = 'ActivityPub::ProcessingWorker'"#
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(remaining, 1, "the future job must still be queued");
}

/// A Delete that arrives before its Create is remembered, so the late Create is
/// not honoured.
///
/// Federation does not promise order, and without this, deleting a post can be
/// undone by the network redelivering the post that created it.
///
/// Two mechanisms answer this and only one is exercised here. For an actor this
/// instance knows, the Delete writes a tombstone and the Create is refused
/// against it — that is what this test covers, and it had none before. For an
/// actor it does not know there is no row to hang a tombstone on, and the Delete
/// is remembered in Redis instead (`delete_later`). That second path is still
/// uncovered; it was noticed when a refactor removed the call and every test
/// still passed.
#[tokio::test]
async fn test_a_delete_that_arrives_first_suppresses_a_late_create() {
    let ctx = TestContext::new("ingress-delete-first").await;
    let actor_uri = seed_remote_actor(&ctx, "mallory", "ingress-order.invalid").await;
    let note_uri = "https://ingress-order.invalid/notes/raced";

    // An inbox drops a status from an account nobody here follows, so without a
    // follower this test would pass for the wrong reason — the control at the
    // end is what proves it does not.
    let actor_id: i64 = sqlx::query_scalar!("SELECT id FROM accounts WHERE uri = $1", actor_uri)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    sqlx::query!(
        r#"INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())"#,
        eunha::snowflake::next_id(),
        ctx.alice_id.parse::<i64>().unwrap(),
        actor_id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    // The Delete arrives for a status this instance has never seen.
    let delete = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://ingress-order.invalid/activities/delete-1",
        "type": "Delete",
        "actor": actor_uri,
        "object": {"id": note_uri, "type": "Tombstone"},
    });
    eunha::api::ap::inbox::queue_activity(&ctx.db, &delete)
        .await
        .unwrap();
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();

    // The Create for the same object turns up afterwards.
    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://ingress-order.invalid/activities/create-1",
        "type": "Create",
        "actor": actor_uri,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": actor_uri,
            "content": "<p>already withdrawn</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2026-01-01T00:00:00Z",
        },
    });
    eunha::api::ap::inbox::queue_activity(&ctx.db, &create)
        .await
        .unwrap();
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();

    let live: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!" FROM statuses WHERE uri = $1 AND deleted_at IS NULL"#,
        note_uri,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        live, 0,
        "a Create arriving after its Delete must not bring the status back"
    );

    // The same Create, with no Delete before it, must be stored — otherwise the
    // assertion above holds for a reason that has nothing to do with ordering.
    let control_uri = "https://ingress-order.invalid/notes/control";
    let control = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": "https://ingress-order.invalid/activities/create-2",
        "type": "Create",
        "actor": actor_uri,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": control_uri,
            "type": "Note",
            "attributedTo": actor_uri,
            "content": "<p>never withdrawn</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2026-01-01T00:00:00Z",
        },
    });
    eunha::api::ap::inbox::queue_activity(&ctx.db, &control)
        .await
        .unwrap();
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();

    let control_live: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!" FROM statuses WHERE uri = $1 AND deleted_at IS NULL"#,
        control_uri,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        control_live, 1,
        "an ordinary Create from this actor must be stored, or the test above proves nothing"
    );
}

/// The Redis note answers a Delete-before-Create when no tombstone can.
///
/// The other test covers the tombstone, which needs an `accounts` row to hang
/// itself on. When the Delete is the first thing this instance has ever heard
/// about an actor there is no such row, and the Delete is remembered in Redis
/// instead. Only that mechanism can suppress the late Create, so the tombstone
/// is removed here to leave it holding the question alone — without which this
/// test would pass on the tombstone's account and prove nothing, as an earlier
/// version of the test above in fact did.
#[tokio::test]
async fn test_a_remembered_delete_suppresses_a_create_without_a_tombstone() {
    let ctx = TestContext::new("ingress-delete-redis").await;
    // The suite shares one Redis, and the key this exercises is
    // `delete_upon_arrival:{actor}:{uri}` — both fixed strings would mean a key
    // written by the previous run answers this one, and the test passes whether
    // or not the code still writes it. A unique domain per run avoids that.
    let domain = format!("ingress-redis-{}.invalid", eunha::snowflake::next_id());
    let actor_uri = seed_remote_actor(&ctx, "mallory", &domain).await;
    let note_uri = format!("https://{domain}/notes/remembered");
    let note_uri = note_uri.as_str();

    let actor_id: i64 = sqlx::query_scalar!("SELECT id FROM accounts WHERE uri = $1", actor_uri)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    sqlx::query!(
        r#"INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())"#,
        eunha::snowflake::next_id(),
        ctx.alice_id.parse::<i64>().unwrap(),
        actor_id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let delete = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("https://{domain}/activities/delete-1"),
        "type": "Delete",
        "actor": actor_uri,
        "object": {"id": note_uri, "type": "Tombstone"},
    });
    eunha::api::ap::inbox::queue_activity(&ctx.db, &delete)
        .await
        .unwrap();
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();

    // Take away the persistent answer, leaving only the remembered one.
    let removed = sqlx::query!("DELETE FROM tombstones WHERE uri = $1", note_uri)
        .execute(&ctx.db)
        .await
        .unwrap();
    assert_eq!(
        removed.rows_affected(),
        1,
        "the Delete should have written a tombstone for this known actor"
    );

    let create = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("https://{domain}/activities/create-1"),
        "type": "Create",
        "actor": actor_uri,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": note_uri,
            "type": "Note",
            "attributedTo": actor_uri,
            "content": "<p>already withdrawn</p>",
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2026-01-01T00:00:00Z",
        },
    });
    eunha::api::ap::inbox::queue_activity(&ctx.db, &create)
        .await
        .unwrap();
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();

    let live: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!" FROM statuses WHERE uri = $1 AND deleted_at IS NULL"#,
        note_uri,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        live, 0,
        "the remembered Delete must suppress the Create with no tombstone to help"
    );
}
