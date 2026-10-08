//! When Mastodon does not notify, and whether eunha agrees.
//!
//! `NotifyService::DropCondition#drop?` is a list of reasons to say nothing.
//! The ones tested here are the unconditional ones — not the account's
//! notification policy, which is a preference, but the reasons that hold
//! regardless: a block, a mute, a domain block. Getting one wrong means a
//! notification from someone the recipient has explicitly shut out.

use crate::helpers::TestContext;

/// Seed a remote account and return its id and actor uri.
async fn remote_actor(ctx: &TestContext, username: &str, domain: &str) -> (i64, String) {
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query!(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4::text, $4::text, 'remote-key',
                   $4::text||'/inbox', $4::text||'/outbox', now(), now())"#,
        id,
        username,
        domain,
        uri,
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    (id, uri)
}

async fn notification_count(ctx: &TestContext, account_id: i64) -> i64 {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!" FROM notifications WHERE account_id = $1"#,
        account_id
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

/// An account on a domain the recipient has blocked cannot notify them.
///
/// `domain_blocking?` is `@recipient.domain_blocking?(@sender.domain) &&
/// not_following?`: blocking a domain is a statement about wanting nothing from
/// it, and a mention arriving as a notification is the thing being asked to
/// stop. Following someone there is the exception, since that is a deliberate
/// choice to keep hearing from them.
#[tokio::test]
async fn test_a_domain_block_stops_notifications() {
    let ctx = TestContext::new("notify-domain-block").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();

    let domain = "notify-blocked.invalid";
    let (_sender_id, actor_uri) = remote_actor(&ctx, "stranger", domain).await;

    sqlx::query!(
        r#"INSERT INTO account_domain_blocks (account_id, domain, created_at, updated_at)
           VALUES ($1, $2, now(), now())"#,
        alice_id,
        domain,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let before = notification_count(&ctx, alice_id).await;

    let alice_uri = format!("https://{}/users/alice", ctx.domain);
    let create = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("https://{domain}/activities/mention-1"),
        "type": "Create",
        "actor": actor_uri,
        "to": [alice_uri],
        "object": {
            "id": format!("https://{domain}/notes/mention-1"),
            "type": "Note",
            "attributedTo": actor_uri,
            "content": "<p>hello there</p>",
            "to": [alice_uri],
            "tag": [{"type": "Mention", "href": alice_uri, "name": "@alice"}],
            "published": chrono::Utc::now().to_rfc3339(),
        },
    });
    eunha::api::ap::inbox::queue_activity(&ctx.db, &create)
        .await
        .unwrap();
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();

    assert_eq!(
        notification_count(&ctx, alice_id).await,
        before,
        "an account on a blocked domain must not reach the recipient's notifications"
    );
}

/// Following someone on a blocked domain still notifies.
///
/// The domain block is `&& not_following?`. Blocking a domain is a statement
/// about the domain; following one account there is a deliberate exception to
/// it, and silently swallowing that account's mentions would make the follow
/// useless without saying so.
#[tokio::test]
async fn test_following_through_a_domain_block_still_notifies() {
    let ctx = TestContext::new("notify-domain-block-followed").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();

    let domain = "notify-followed.invalid";
    let (sender_id, actor_uri) = remote_actor(&ctx, "friend", domain).await;

    sqlx::query!(
        r#"INSERT INTO account_domain_blocks (account_id, domain, created_at, updated_at)
           VALUES ($1, $2, now(), now())"#,
        alice_id,
        domain,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    // Alice follows this one account despite blocking its domain.
    sqlx::query!(
        r#"INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())"#,
        eunha::snowflake::next_id(),
        alice_id,
        sender_id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let before = notification_count(&ctx, alice_id).await;

    let alice_uri = format!("https://{}/users/alice", ctx.domain);
    let create = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("https://{domain}/activities/mention-2"),
        "type": "Create",
        "actor": actor_uri,
        "to": [alice_uri],
        "object": {
            "id": format!("https://{domain}/notes/mention-2"),
            "type": "Note",
            "attributedTo": actor_uri,
            "content": "<p>still here</p>",
            "to": [alice_uri],
            "tag": [{"type": "Mention", "href": alice_uri, "name": "@alice"}],
            "published": chrono::Utc::now().to_rfc3339(),
        },
    });
    eunha::api::ap::inbox::queue_activity(&ctx.db, &create)
        .await
        .unwrap();
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();

    assert_eq!(
        notification_count(&ctx, alice_id).await,
        before + 1,
        "an account followed through a domain block should still notify"
    );
}

/// A mention that also mentions someone the recipient blocked is not delivered.
///
/// `FeedManager#filter_from_mentions?` gathers the status's mentions and, for a
/// reply, the account replied to, and drops the notification if the recipient
/// blocks or mutes any of them. The sender need not be blocked: the point is
/// not to be pulled into a conversation with someone deliberately shut out.
#[tokio::test]
async fn test_a_mention_alongside_a_blocked_account_is_not_delivered() {
    let ctx = TestContext::new("notify-blocked-co-mention").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let bob_id: i64 = ctx.bob_id.parse().unwrap();

    // Alice blocks Bob. A third account then mentions them both.
    sqlx::query!(
        r#"INSERT INTO blocks (id, account_id, target_account_id, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())"#,
        eunha::snowflake::next_id(),
        alice_id,
        bob_id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let domain = "notify-comention.invalid";
    let (_id, actor_uri) = remote_actor(&ctx, "stranger", domain).await;

    let before = notification_count(&ctx, alice_id).await;

    let alice_uri = format!("https://{}/users/alice", ctx.domain);
    let bob_uri = format!("https://{}/users/bob", ctx.domain);
    let create = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("https://{domain}/activities/comention-1"),
        "type": "Create",
        "actor": actor_uri,
        "to": [alice_uri, bob_uri],
        "object": {
            "id": format!("https://{domain}/notes/comention-1"),
            "type": "Note",
            "attributedTo": actor_uri,
            "content": "<p>you two should talk</p>",
            "to": [alice_uri, bob_uri],
            "tag": [
                {"type": "Mention", "href": alice_uri, "name": "@alice"},
                {"type": "Mention", "href": bob_uri, "name": "@bob"},
            ],
            "published": chrono::Utc::now().to_rfc3339(),
        },
    });
    eunha::api::ap::inbox::queue_activity(&ctx.db, &create)
        .await
        .unwrap();
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();

    assert_eq!(
        notification_count(&ctx, alice_id).await,
        before,
        "a mention that drags in a blocked account must not notify"
    );
}

/// A mention alongside someone the recipient has no quarrel with still arrives.
///
/// The guard against being dragged into a thread with a blocked account must
/// not swallow ordinary group conversations, which are the common case.
#[tokio::test]
async fn test_an_ordinary_co_mention_still_notifies() {
    let ctx = TestContext::new("notify-comention-ok").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();

    let domain = "notify-comention-ok.invalid";
    let (_id, actor_uri) = remote_actor(&ctx, "stranger", domain).await;

    let before = notification_count(&ctx, alice_id).await;

    let alice_uri = format!("https://{}/users/alice", ctx.domain);
    let bob_uri = format!("https://{}/users/bob", ctx.domain);
    let create = serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("https://{domain}/activities/comention-ok"),
        "type": "Create",
        "actor": actor_uri,
        "to": [alice_uri, bob_uri],
        "object": {
            "id": format!("https://{domain}/notes/comention-ok"),
            "type": "Note",
            "attributedTo": actor_uri,
            "content": "<p>you two should meet</p>",
            "to": [alice_uri, bob_uri],
            "tag": [
                {"type": "Mention", "href": alice_uri, "name": "@alice"},
                {"type": "Mention", "href": bob_uri, "name": "@bob"},
            ],
            "published": chrono::Utc::now().to_rfc3339(),
        },
    });
    eunha::api::ap::inbox::queue_activity(&ctx.db, &create)
        .await
        .unwrap();
    eunha::api::ap::inbox::drain_inbox_queue(&ctx.state)
        .await
        .unwrap();

    assert_eq!(
        notification_count(&ctx, alice_id).await,
        before + 1,
        "a group mention with nobody blocked should notify as usual"
    );
    let bob_id: i64 = ctx.bob_id.parse().unwrap();
    assert!(
        notification_count(&ctx, bob_id).await > 0,
        "and everyone else mentioned should hear about it too"
    );
}

/// A group does not reach back for ever.
///
/// Mastodon's `MAXIMUM_GROUP_SPAN_HOURS` is 12: a notification joins the
/// previous group unless that group already began more than twelve hours ago,
/// in which case it starts a new one. eunha's key had no time in it at all, so
/// every favourite of a status joined one group however far apart they were —
/// a post favourited today and again next week read as a single "2 people".
///
/// Checked against a running Mastodon 4.7.0, which puts two favourites twenty
/// hours apart into different groups.
#[tokio::test]
async fn test_a_group_key_carries_an_hour_bucket() {
    let ctx = TestContext::new("notify-group-span").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "favourite me", "public")
        .await;
    let sid = status["id"].as_str().unwrap();

    let favourited = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{sid}/favourite"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(favourited.status().as_u16(), 200);

    let stored: Option<String> = sqlx::query_scalar!(
        r#"SELECT group_key FROM notifications
           WHERE account_id = $1 AND type = 'favourite' ORDER BY id DESC LIMIT 1"#,
        ctx.alice_id.parse::<i64>().unwrap(),
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();

    let key = stored.expect("a favourite is groupable, so it is given a key");
    let prefix = format!("favourite-{sid}-");
    assert!(
        key.starts_with(&prefix),
        "the key names what is grouped and when: {key}"
    );

    let bucket: i64 = key[prefix.len()..]
        .parse()
        .unwrap_or_else(|_| panic!("the bucket should be an hour number: {key}"));
    let now_bucket = chrono::Utc::now().timestamp() / 3600;
    assert!(
        (bucket - now_bucket).abs() <= 1,
        "the bucket should be this hour, got {bucket} against {now_bucket}"
    );
}

/// A type Mastodon does not group is not given a key.
///
/// `GROUPABLE_NOTIFICATION_TYPES` is favourite, reblog, follow and admin.sign_up
/// — a mention is not among them, and gets no group key at all.
#[tokio::test]
async fn test_an_ungroupable_type_has_no_key() {
    let ctx = TestContext::new("notify-group-ungroupable").await;

    let response = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &serde_json::json!({"status": "@alice hello", "visibility": "public"}),
        )
        .await;
    assert_eq!(response.status().as_u16(), 200);

    let stored: Option<Option<String>> = sqlx::query_scalar!(
        r#"SELECT group_key FROM notifications
           WHERE account_id = $1 AND type = 'mention' ORDER BY id DESC LIMIT 1"#,
        ctx.alice_id.parse::<i64>().unwrap(),
    )
    .fetch_optional(&ctx.db)
    .await
    .unwrap();

    if let Some(key) = stored {
        assert!(
            key.is_none(),
            "a mention is not groupable and should carry no key, got {key:?}"
        );
    }
}
