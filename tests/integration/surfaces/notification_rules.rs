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

/// The group key a notification was stored with, the newest of its type.
async fn stored_group_key(ctx: &TestContext, account_id: i64, kind: &str) -> Option<String> {
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT group_key FROM notifications
         WHERE account_id = $1 AND type = $2 ORDER BY id DESC LIMIT 1",
    )
    .bind(account_id)
    .bind(kind)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

/// A filtered notification is given no group key.
///
/// `set_group_key!` returns early when the notification is `filtered?`: the
/// key stays NULL, so the notification reads as `ungrouped-<id>` even after
/// it is let through, and the running bucket in Redis is not touched.
#[tokio::test]
async fn test_a_filtered_notification_has_no_group_key() {
    let ctx = TestContext::new("notify-group-filtered").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    sqlx::query(
        "INSERT INTO notification_policies
           (account_id, for_not_following, created_at, updated_at)
         VALUES ($1, 1, now(), now())
         ON CONFLICT (account_id) DO UPDATE SET for_not_following = 1",
    )
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();

    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let filtered: bool = sqlx::query_scalar(
        "SELECT filtered FROM notifications WHERE account_id = $1 AND type = 'follow'",
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(filtered, "a follow from someone alice does not follow");
    assert_eq!(stored_group_key(&ctx, alice, "follow").await, None);

    let mut redis = ctx.state.redis_coordination.clone();
    let bucket: Option<i64> = redis::cmd("GET")
        .arg(
            ctx.state
                .redis_keys
                .key(format!("notif-group/{alice}/follow")),
        )
        .query_async(&mut redis)
        .await
        .unwrap();
    assert_eq!(bucket, None, "the running bucket is left alone");
}

/// `admin.sign_up` is groupable, so it is given `admin.sign_up-<bucket>`,
/// the bucket being the hour the new account was made in.
#[tokio::test]
async fn test_a_sign_up_notification_is_grouped() {
    let ctx = TestContext::new("notify-group-sign-up").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    let made_at: i64 = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM created_at)::bigint FROM accounts WHERE id = $1",
    )
    .bind(bob)
    .fetch_one(&ctx.db)
    .await
    .unwrap();

    eunha::push::notify_local(&ctx.state, alice, "admin.sign_up", "Account", bob, bob).await;

    assert_eq!(
        stored_group_key(&ctx, alice, "admin.sign_up").await,
        Some(format!("admin.sign_up-{}", made_at / 3600)),
    );
}

/// `muting_notifications?` asks only whether a mute hiding notifications is
/// there. One that has expired, but has not yet been removed, still drops a
/// staff notification from the muted account.
#[tokio::test]
async fn test_an_expired_mute_still_hides_a_staff_notification() {
    let ctx = TestContext::new("notify-expired-mute").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    sqlx::query(
        "INSERT INTO mutes (account_id, target_account_id, hide_notifications,
                            expires_at, created_at, updated_at)
         VALUES ($1, $2, true, now() - interval '1 hour', now(), now())",
    )
    .bind(alice)
    .bind(bob)
    .execute(&ctx.db)
    .await
    .unwrap();

    eunha::push::notify_local(&ctx.state, alice, "admin.sign_up", "Account", bob, bob).await;

    assert_eq!(notification_count(&ctx, alice).await, 0);
}

/// The activity a notification of `kind` to `account_id` points at.
async fn stored_activity(ctx: &TestContext, account_id: i64, kind: &str) -> Option<(String, i64)> {
    sqlx::query_as::<_, (String, i64)>(
        "SELECT activity_type, activity_id FROM notifications
         WHERE account_id = $1 AND type = $2 ORDER BY id DESC LIMIT 1",
    )
    .bind(account_id)
    .bind(kind)
    .fetch_optional(&ctx.db)
    .await
    .unwrap()
}

/// `FavouriteService` notifies of the `Favourite`, which takes its
/// notification with it when it is undone (`has_one :notification,
/// dependent: :destroy`); the notification still shows the post.
#[tokio::test]
async fn test_a_favourite_notification_is_about_the_favourite() {
    let ctx = TestContext::new("notify-activity-favourite").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "favourite me", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    let path = |verb: &str| format!("/api/v1/statuses/{sid}/{verb}");
    let resp = ctx
        .api
        .post_json(
            &path("favourite"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status().as_u16(), 200);

    let favourite_id: i64 = sqlx::query_scalar("SELECT id FROM favourites WHERE status_id = $1")
        .bind(sid.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(
        stored_activity(&ctx, alice, "favourite").await,
        Some(("Favourite".to_owned(), favourite_id))
    );
    let listed: Vec<serde_json::Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed[0]["status"]["id"].as_str(), Some(sid));

    let resp = ctx
        .api
        .post_json(
            &path("unfavourite"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status().as_u16(), 200);
    assert_eq!(stored_activity(&ctx, alice, "favourite").await, None);
}

/// `ReblogService` notifies of the boost, a status of the booster's; the
/// notification shows the post boosted (`status&.reblog`), and goes with
/// the boost.
#[tokio::test]
async fn test_a_reblog_notification_is_about_the_boost() {
    let ctx = TestContext::new("notify-activity-reblog").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "boost me", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    let boost: serde_json::Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{sid}/reblog"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    let boost_id: i64 = boost["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        stored_activity(&ctx, alice, "reblog").await,
        Some(("Status".to_owned(), boost_id))
    );
    let listed: Vec<serde_json::Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed[0]["type"], "reblog");
    assert_eq!(listed[0]["status"]["id"].as_str(), Some(sid));

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{sid}/unreblog"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status().as_u16(), 200);
    ctx.state.jobs.settle().await;
    assert_eq!(stored_activity(&ctx, alice, "reblog").await, None);
}

/// A follow notification is about the sender's `Follow` of the recipient,
/// even when the recipient follows the sender too.
#[tokio::test]
async fn test_a_follow_notification_is_about_the_senders_follow() {
    let ctx = TestContext::new("notify-activity-follow").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    let bobs_follow: i64 = sqlx::query_scalar(
        "SELECT id FROM follows WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(bob)
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        stored_activity(&ctx, alice, "follow").await,
        Some(("Follow".to_owned(), bobs_follow))
    );
}

/// Migration 032 points the notifications eunha wrote before it at
/// Mastodon's activities, where they still exist, and leaves the rest.
#[tokio::test]
async fn test_migration_032_points_notifications_at_mastodons_activities() {
    let ctx = TestContext::new("notify-activity-migration").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    let post = ctx
        .api
        .post_status(&ctx.alice_token, "old notifications", "public")
        .await;
    let sid: i64 = post["id"].as_str().unwrap().parse().unwrap();
    let gone = ctx
        .api
        .post_status(&ctx.alice_token, "unfavourited since", "public")
        .await;
    let gone_id: i64 = gone["id"].as_str().unwrap().parse().unwrap();
    for verb in ["favourite", "reblog"] {
        ctx.api
            .post_json(
                &format!("/api/v1/statuses/{sid}/{verb}"),
                Some(&ctx.bob_token),
                &serde_json::json!({}),
            )
            .await;
    }
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    let (favourite_id, boost_id, bobs_follow, alices_follow): (i64, i64, i64, i64) =
        sqlx::query_as(
            "SELECT (SELECT id FROM favourites WHERE account_id = $2),
                    (SELECT id FROM statuses WHERE account_id = $2 AND reblog_of_id = $3),
                    (SELECT id FROM follows WHERE account_id = $2 AND target_account_id = $1),
                    (SELECT id FROM follows WHERE account_id = $1 AND target_account_id = $2)",
        )
        .bind(alice)
        .bind(bob)
        .bind(sid)
        .fetch_one(&ctx.db)
        .await
        .unwrap();

    // As eunha wrote them: about the post, and the wrong follow.
    for (kind, activity_type, activity_id) in [
        ("favourite", "Status", sid),
        ("reblog", "Status", sid),
        ("follow", "Follow", alices_follow),
    ] {
        sqlx::query(
            "UPDATE notifications SET activity_type = $3, activity_id = $4
             WHERE account_id = $1 AND type = $2",
        )
        .bind(alice)
        .bind(kind)
        .bind(activity_type)
        .bind(activity_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO notifications (account_id, from_account_id, type, activity_type,
                                    activity_id, created_at, updated_at)
         VALUES ($1, $2, 'favourite', 'Status', $3, now(), now())",
    )
    .bind(alice)
    .bind(bob)
    .bind(gone_id)
    .execute(&ctx.db)
    .await
    .unwrap();

    sqlx::raw_sql(include_str!(
        "../../../migrations/032_notification_activities.sql"
    ))
    .execute(&ctx.db)
    .await
    .unwrap();

    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT type, activity_type, activity_id FROM notifications
         WHERE account_id = $1 ORDER BY type, activity_type, activity_id",
    )
    .bind(alice)
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("favourite".to_owned(), "Favourite".to_owned(), favourite_id),
            ("favourite".to_owned(), "Status".to_owned(), gone_id),
            ("follow".to_owned(), "Follow".to_owned(), bobs_follow),
            ("reblog".to_owned(), "Status".to_owned(), boost_id),
        ]
    );
}
