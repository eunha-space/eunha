//! What eunha's counters count, against what Mastodon's count.
//!
//! A count is read by everyone who sees the status or account, so counting the
//! wrong things is not only wrong but can say something the viewer should not
//! learn. Mastodon's rules are in `Status#increment_counter_caches` and
//! `Account::Counters`; the ones checked here are the conditional ones, since
//! an unconditional counter is hard to get wrong.

use crate::helpers::TestContext;

async fn replies_count(ctx: &TestContext, id: &str, token: &str) -> i64 {
    let status: serde_json::Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}"), Some(token))
        .await
        .json()
        .await
        .unwrap();
    status["replies_count"].as_i64().unwrap_or(-1)
}

/// Only replies everyone can see are counted.
///
/// Mastodon increments `replies_count` `if in_reply_to_id.present? &&
/// distributable?`, and `distributable?` is public or unlisted. A
/// followers-only or direct reply therefore leaves the count alone — otherwise
/// the number visible to everyone would announce that someone had replied
/// privately, which is exactly what the visibility was chosen to avoid.
#[tokio::test]
async fn test_only_distributable_replies_are_counted() {
    let ctx = TestContext::new("counters-replies").await;

    let parent = ctx
        .api
        .post_status(&ctx.alice_token, "a public post", "public")
        .await;
    let parent_id = parent["id"].as_str().unwrap().to_string();
    assert_eq!(replies_count(&ctx, &parent_id, &ctx.alice_token).await, 0);

    // Public and unlisted replies count.
    for (visibility, expected) in [("public", 1), ("unlisted", 2)] {
        let response = ctx
            .api
            .post_json(
                "/api/v1/statuses",
                Some(&ctx.bob_token),
                &serde_json::json!({
                    "status": format!("a {visibility} reply"),
                    "in_reply_to_id": parent_id,
                    "visibility": visibility,
                }),
            )
            .await;
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            replies_count(&ctx, &parent_id, &ctx.alice_token).await,
            expected,
            "a {visibility} reply should be counted"
        );
    }

    // Followers-only and direct replies do not.
    for visibility in ["private", "direct"] {
        let response = ctx
            .api
            .post_json(
                "/api/v1/statuses",
                Some(&ctx.bob_token),
                &serde_json::json!({
                    "status": format!("a {visibility} reply"),
                    "in_reply_to_id": parent_id,
                    "visibility": visibility,
                }),
            )
            .await;
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            replies_count(&ctx, &parent_id, &ctx.alice_token).await,
            2,
            "a {visibility} reply must not change a count everyone can read"
        );
    }
}

/// Deleting a reply that was never counted must not decrement the count.
///
/// The mirror of the rule above: if a private reply did not add to the count,
/// removing it must not subtract, or the count drifts down every time someone
/// deletes one.
#[tokio::test]
async fn test_deleting_an_uncounted_reply_leaves_the_count_alone() {
    let ctx = TestContext::new("counters-replies-delete").await;

    let parent = ctx
        .api
        .post_status(&ctx.alice_token, "a public post", "public")
        .await;
    let parent_id = parent["id"].as_str().unwrap().to_string();

    let public_reply = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &serde_json::json!({
                "status": "a public reply",
                "in_reply_to_id": parent_id,
                "visibility": "public",
            }),
        )
        .await;
    assert_eq!(public_reply.status().as_u16(), 200);
    let _: serde_json::Value = public_reply.json().await.unwrap();

    let private_reply: serde_json::Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &serde_json::json!({
                "status": "a private reply",
                "in_reply_to_id": parent_id,
                "visibility": "private",
            }),
        )
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(replies_count(&ctx, &parent_id, &ctx.alice_token).await, 1);

    let deleted = ctx
        .api
        .delete(
            &format!("/api/v1/statuses/{}", private_reply["id"].as_str().unwrap()),
            &ctx.bob_token,
        )
        .await;
    assert_eq!(deleted.status().as_u16(), 200);

    assert_eq!(
        replies_count(&ctx, &parent_id, &ctx.alice_token).await,
        1,
        "deleting a reply that was never counted must not decrement"
    );
}

/// A reply that arrives over ActivityPub counts like one posted here.
///
/// Mastodon's counter callbacks live on the `Status` model, so they run
/// whenever a status is created — whether a local client posted it or it
/// arrived in the inbox. A local post replied to from across the network should
/// show those replies, and eunha counted only the ones posted through its own
/// API, so a widely federated post read as having none.
#[tokio::test]
async fn test_a_federated_reply_is_counted() {
    let ctx = TestContext::new("counters-federated-reply").await;

    let parent = ctx
        .api
        .post_status(&ctx.alice_token, "a post the network can see", "public")
        .await;
    let parent_id = parent["id"].as_str().unwrap().to_string();
    let parent_uri: String = sqlx::query_scalar!(
        "SELECT uri FROM statuses WHERE id = $1",
        parent_id.parse::<i64>().unwrap()
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap()
    .unwrap();

    let domain = "counters-remote.invalid";
    let actor_uri = format!("https://{domain}/users/mallory");
    sqlx::query!(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'mallory', $2, 'mallory', '', $3::text, $3::text, 'remote-key',
                   $3::text||'/inbox', $3::text||'/outbox', now(), now())"#,
        eunha::snowflake::next_id(),
        domain,
        actor_uri,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let create = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("https://{domain}/activities/reply-1"),
        "type": "Create",
        "actor": actor_uri,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": {
            "id": format!("https://{domain}/notes/reply-1"),
            "type": "Note",
            "attributedTo": actor_uri,
            "content": "<p>a reply from elsewhere</p>",
            "inReplyTo": parent_uri,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "published": "2026-01-01T00:00:00Z",
        },
    });

    eunha::api::ap::inbox::queue_activity(&ctx.db, &create)
        .await
        .unwrap();

    let processed = eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .expect("draining must not error");
    assert_eq!(processed, 1, "the queued Create must be claimed");

    let stored: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!" FROM statuses WHERE in_reply_to_id = $1"#,
        parent_id.parse::<i64>().unwrap()
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(stored, 1, "the reply itself must have been stored");

    assert_eq!(
        replies_count(&ctx, &parent_id, &ctx.alice_token).await,
        1,
        "a public reply from another instance must be counted"
    );
}

/// A federated reply that is deleted stops being counted.
///
/// The mirror of counting it on arrival: without this the count only ever rises,
/// and a thread whose replies were all withdrawn still claims to have them.
#[tokio::test]
async fn test_a_deleted_federated_reply_is_uncounted() {
    let ctx = TestContext::new("counters-federated-delete").await;

    let parent = ctx
        .api
        .post_status(&ctx.alice_token, "a post the network can see", "public")
        .await;
    let parent_id = parent["id"].as_str().unwrap().to_string();
    let parent_uri: String = sqlx::query_scalar!(
        "SELECT uri FROM statuses WHERE id = $1",
        parent_id.parse::<i64>().unwrap()
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap()
    .unwrap();

    let domain = "counters-del-remote.invalid";
    let actor_uri = format!("https://{domain}/users/mallory");
    sqlx::query!(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'mallory', $2, 'mallory', '', $3::text, $3::text, 'remote-key',
                   $3::text||'/inbox', $3::text||'/outbox', now(), now())"#,
        eunha::snowflake::next_id(),
        domain,
        actor_uri,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let note_uri = format!("https://{domain}/notes/reply-1");
    let enqueue = |activity: serde_json::Value, _kind: &'static str| {
        let db = ctx.db.clone();
        async move {
            eunha::api::ap::inbox::queue_activity(&db, &activity)
                .await
                .unwrap();
        }
    };

    enqueue(
        serde_json::json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("https://{domain}/activities/reply-1"),
            "type": "Create",
            "actor": actor_uri,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "object": {
                "id": note_uri,
                "type": "Note",
                "attributedTo": actor_uri,
                "content": "<p>a reply from elsewhere</p>",
                "inReplyTo": parent_uri,
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
                "published": "2026-01-01T00:00:00Z",
            },
        }),
        "Create",
    )
    .await;
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();
    assert_eq!(replies_count(&ctx, &parent_id, &ctx.alice_token).await, 1);

    enqueue(
        serde_json::json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("https://{domain}/activities/delete-1"),
            "type": "Delete",
            "actor": actor_uri,
            "object": {"id": note_uri, "type": "Tombstone"},
        }),
        "Delete",
    )
    .await;
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();

    assert_eq!(
        replies_count(&ctx, &parent_id, &ctx.alice_token).await,
        0,
        "a withdrawn reply must stop being counted"
    );
}

async fn statuses_count(ctx: &TestContext, account_id: &str) -> i64 {
    let account: serde_json::Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{account_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    account["statuses_count"].as_i64().unwrap_or(-1)
}

/// A direct message does not raise the public post count.
///
/// `Status#increment_counter_caches` opens with `return if direct_visibility?`,
/// so none of the counters move for a DM. The count is on a profile anyone can
/// read, and a number that climbs whenever an account sends a private message
/// reports that it sent one.
#[tokio::test]
async fn test_a_direct_message_is_not_counted() {
    let ctx = TestContext::new("counters-direct").await;

    let before = statuses_count(&ctx, &ctx.alice_id).await;

    let public = ctx
        .api
        .post_status(&ctx.alice_token, "everyone can see this", "public")
        .await;
    assert_eq!(public["visibility"].as_str(), Some("public"));
    assert_eq!(
        statuses_count(&ctx, &ctx.alice_id).await,
        before + 1,
        "a public post counts"
    );

    let response = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &serde_json::json!({"status": "just between us", "visibility": "direct"}),
        )
        .await;
    assert_eq!(response.status().as_u16(), 200);

    assert_eq!(
        statuses_count(&ctx, &ctx.alice_id).await,
        before + 1,
        "a direct message must not change a count anyone can read"
    );
}

/// Deleting a direct message does not lower the count either.
#[tokio::test]
async fn test_deleting_a_direct_message_leaves_the_count_alone() {
    let ctx = TestContext::new("counters-direct-delete").await;

    ctx.api
        .post_status(&ctx.alice_token, "everyone can see this", "public")
        .await;
    let before = statuses_count(&ctx, &ctx.alice_id).await;

    let dm: serde_json::Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &serde_json::json!({"status": "just between us", "visibility": "direct"}),
        )
        .await
        .json()
        .await
        .unwrap();

    let deleted = ctx
        .api
        .delete(
            &format!("/api/v1/statuses/{}", dm["id"].as_str().unwrap()),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(deleted.status().as_u16(), 200);

    assert_eq!(
        statuses_count(&ctx, &ctx.alice_id).await,
        before,
        "deleting a message that was never counted must not decrement"
    );
}

/// A remote account's post count and last-post time follow what arrives.
///
/// `increment_counter_caches` runs on the Status model, so it fires for a
/// status that arrived in the inbox just as for one posted here. Without it a
/// remote profile shows a post count frozen at whatever it was when the account
/// was first seen, however much that account goes on to post.
#[tokio::test]
async fn test_a_federated_status_counts_for_its_author() {
    let ctx = TestContext::new("counters-remote-statuses").await;

    let domain = "counters-author.invalid";
    let actor_uri = format!("https://{domain}/users/mallory");
    let account_id = eunha::snowflake::next_id();
    sqlx::query!(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'mallory', $2, 'mallory', '', $3::text, $3::text, 'remote-key',
                   $3::text||'/inbox', $3::text||'/outbox', now(), now())"#,
        account_id,
        domain,
        actor_uri,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    // An inbox drops a status from an account nobody here follows, so give it a
    // local follower to make the Create relevant.
    sqlx::query!(
        r#"INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())"#,
        eunha::snowflake::next_id(),
        ctx.alice_id.parse::<i64>().unwrap(),
        account_id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    for (n, visibility_to) in [
        (1, "https://www.w3.org/ns/activitystreams#Public"),
        (2, "https://www.w3.org/ns/activitystreams#Public"),
    ] {
        let create = serde_json::json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("https://{domain}/activities/{n}"),
            "type": "Create",
            "actor": actor_uri,
            "to": [visibility_to],
            "object": {
                "id": format!("https://{domain}/notes/{n}"),
                "type": "Note",
                "attributedTo": actor_uri,
                "content": format!("<p>post {n}</p>"),
                "to": [visibility_to],
                "published": "2026-01-01T00:00:00Z",
            },
        });
        eunha::api::ap::inbox::queue_activity(&ctx.db, &create)
            .await
            .unwrap();
        eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
            .await
            .unwrap();
    }

    let ingested: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!" FROM statuses WHERE account_id = $1"#,
        account_id
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        ingested, 2,
        "both posts must have been stored to be counted"
    );

    assert_eq!(
        statuses_count(&ctx, &account_id.to_string()).await,
        2,
        "both federated posts should count for their author"
    );

    let last: Option<chrono::NaiveDateTime> = sqlx::query_scalar!(
        "SELECT last_status_at FROM account_stats WHERE account_id = $1",
        account_id
    )
    .fetch_optional(&ctx.db)
    .await
    .unwrap()
    .flatten();
    assert!(last.is_some(), "last_status_at should follow what arrives");
}

/// A follow from another instance raises the local account's follower count.
///
/// `Follow`'s counter callbacks are unconditional, so this should hold however
/// the follow arrived. Kept as coverage rather than as a fix: eunha routes both
/// paths through one `counters` module, and this is what says so.
#[tokio::test]
async fn test_a_federated_follow_counts() {
    let ctx = TestContext::new("counters-federated-follow").await;

    let domain = "counters-follow.invalid";
    let actor_uri = format!("https://{domain}/users/eve");
    sqlx::query!(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'eve', $2, 'eve', '', $3::text, $3::text, 'remote-key',
                   $3::text||'/inbox', $3::text||'/outbox', now(), now())"#,
        eunha::snowflake::next_id(),
        domain,
        actor_uri,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let follow = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("https://{domain}/activities/follow-1"),
        "type": "Follow",
        "actor": actor_uri,
        "object": format!("https://{}/users/alice", ctx.domain),
    });
    eunha::api::ap::inbox::queue_activity(&ctx.db, &follow)
        .await
        .unwrap();
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();

    let account: serde_json::Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        account["followers_count"].as_i64(),
        Some(1),
        "a follow from another instance should be counted"
    );

    // Redelivery must not inflate it: federation repeats.
    eunha::api::ap::inbox::queue_activity(&ctx.db, &follow)
        .await
        .unwrap();
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();
    let account: serde_json::Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        account["followers_count"].as_i64(),
        Some(1),
        "the same Follow delivered twice is still one follower"
    );

    // And undoing it brings the count back down.
    let undo = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("https://{domain}/activities/undo-1"),
        "type": "Undo",
        "actor": actor_uri,
        "object": {
            "id": format!("https://{domain}/activities/follow-1"),
            "type": "Follow",
            "actor": actor_uri,
            "object": format!("https://{}/users/alice", ctx.domain),
        },
    });
    eunha::api::ap::inbox::queue_activity(&ctx.db, &undo)
        .await
        .unwrap();
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();

    let account: serde_json::Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        account["followers_count"].as_i64(),
        Some(0),
        "an unfollow from another instance should bring the count back down"
    );
}

/// A boost from another instance counts as one of the booster's posts.
///
/// A reblog is a Status in Mastodon, so the same callback that counts an
/// original counts a boost, and undoing it takes the count back down. eunha
/// counted the boost against the boosted post but not against the booster.
#[tokio::test]
async fn test_a_federated_boost_counts_for_the_booster() {
    let ctx = TestContext::new("counters-federated-boost").await;

    let original = ctx
        .api
        .post_status(&ctx.alice_token, "worth boosting", "public")
        .await;
    let original_uri: String = sqlx::query_scalar!(
        "SELECT uri FROM statuses WHERE id = $1",
        original["id"].as_str().unwrap().parse::<i64>().unwrap()
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap()
    .unwrap();

    let domain = "counters-boost.invalid";
    let actor_uri = format!("https://{domain}/users/booster");
    let booster_id = eunha::snowflake::next_id();
    sqlx::query!(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'booster', $2, 'booster', '', $3::text, $3::text, 'remote-key',
                   $3::text||'/inbox', $3::text||'/outbox', now(), now())"#,
        booster_id,
        domain,
        actor_uri,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let announce_uri = format!("https://{domain}/activities/announce-1");
    let announce = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": announce_uri,
        "type": "Announce",
        "actor": actor_uri,
        "object": original_uri,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
    });
    let enqueue = |activity: serde_json::Value, _kind: &'static str| {
        let db = ctx.db.clone();
        async move {
            eunha::api::ap::inbox::queue_activity(&db, &activity)
                .await
                .unwrap();
        }
    };

    enqueue(announce.clone(), "Announce").await;
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();

    assert_eq!(
        statuses_count(&ctx, &booster_id.to_string()).await,
        1,
        "a boost is one of the booster's statuses"
    );

    // Redelivery must not count twice.
    enqueue(announce, "Announce").await;
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();
    assert_eq!(
        statuses_count(&ctx, &booster_id.to_string()).await,
        1,
        "the same Announce delivered twice is still one boost"
    );

    enqueue(
        serde_json::json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("https://{domain}/activities/undo-announce-1"),
            "type": "Undo",
            "actor": actor_uri,
            "object": {"id": announce_uri, "type": "Announce", "actor": actor_uri},
        }),
        "Undo",
    )
    .await;
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();

    assert_eq!(
        statuses_count(&ctx, &booster_id.to_string()).await,
        0,
        "undoing a boost takes it back off the booster's total"
    );
}

/// A scheduled post is counted under the same rules as an immediate one.
///
/// The path that publishes a schedule kept its own copy of the counter logic,
/// and that copy never got the direct-message rule — so a scheduled DM raised
/// the public post count after the same bug had been fixed for ordinary posts.
/// It goes through `counters` now, which is the point of the module: a rule
/// fixed once is fixed everywhere it applies.
#[tokio::test]
async fn test_a_scheduled_direct_message_is_not_counted() {
    let ctx = TestContext::new("counters-scheduled-dm").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();

    ctx.api
        .post_status(&ctx.alice_token, "an ordinary post", "public")
        .await;
    let before = statuses_count(&ctx, &ctx.alice_id).await;

    // A public one first, so the test proves the publisher ran at all: without
    // this, a path that silently did nothing would pass the assertion below.
    let public_id = eunha::snowflake::next_id();
    sqlx::query!(
        r#"INSERT INTO scheduled_statuses (id, account_id, scheduled_at, params)
           VALUES ($1, $2, now() - interval '1 minute', $3)"#,
        public_id,
        alice_id,
        serde_json::json!({"text": "scheduled and public", "visibility": "public", "sensitive": false}),
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    eunha::background::publish_due_statuses(&ctx.state)
        .await
        .expect("publishing must not error");
    assert_eq!(
        statuses_count(&ctx, &ctx.alice_id).await,
        before + 1,
        "a scheduled public post should be counted"
    );
    let before = statuses_count(&ctx, &ctx.alice_id).await;

    // A schedule due in the past, so draining publishes it immediately.
    let scheduled_id = eunha::snowflake::next_id();
    sqlx::query!(
        r#"INSERT INTO scheduled_statuses (id, account_id, scheduled_at, params)
           VALUES ($1, $2, now() - interval '1 minute', $3)"#,
        scheduled_id,
        alice_id,
        serde_json::json!({
            "text": "scheduled and private",
            "visibility": "direct",
            "sensitive": false,
        }),
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    eunha::background::publish_due_statuses(&ctx.state)
        .await
        .expect("publishing must not error");

    assert_eq!(
        statuses_count(&ctx, &ctx.alice_id).await,
        before,
        "a scheduled direct message must not raise the public post count"
    );
}

/// `Announce#perform` runs under `with_redis_lock("announce:<object>")`:
/// held by another, the activity fails, to be tried again, and stores
/// nothing; released, it goes through.
#[tokio::test]
async fn test_a_federated_boost_takes_the_announce_lock() {
    let ctx = TestContext::new("counters-announce-lock").await;
    let original = ctx
        .api
        .post_status(&ctx.alice_token, "worth boosting", "public")
        .await;
    let original_uri: String = sqlx::query_scalar!(
        "SELECT uri FROM statuses WHERE id = $1",
        original["id"].as_str().unwrap().parse::<i64>().unwrap()
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap()
    .unwrap();
    let domain = "announce-lock.invalid";
    let actor_uri = format!("https://{domain}/users/booster");
    let booster_id = eunha::snowflake::next_id();
    sqlx::query!(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, 'booster', $2, 'booster', '', $3::text, $3::text, 'remote-key',
                   $3::text||'/inbox', $3::text||'/outbox', now(), now())"#,
        booster_id,
        domain,
        actor_uri,
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let announce = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("https://{domain}/activities/announce-1"),
        "type": "Announce",
        "actor": actor_uri,
        "object": original_uri,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
    });

    let held = eunha::redis_lock::try_acquire(
        &ctx.state,
        &format!("lock:announce:{original_uri}"),
        60_000,
    )
    .await
    .unwrap();
    // Failed inline, it is queued to be tried again.
    eunha::api::ap::inbox::received(&ctx.state, announce)
        .await
        .unwrap();
    assert_eq!(
        statuses_count(&ctx, &booster_id.to_string()).await,
        0,
        "stored while another held the lock"
    );
    held.release().await;
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();
    assert_eq!(statuses_count(&ctx, &booster_id.to_string()).await, 1);
}
