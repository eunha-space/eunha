//! How far a mute reaches, as Mastodon's does.
//!
//! `FeedManager#filter_from_home` drops every status whose author the viewer
//! mutes, or that boosts or mentions someone they mute, and
//! `NotifyService::DropCondition` drops every notification from them once
//! `hide_notifications` is set — a mention of the viewer and a favourite of
//! the viewer's own post among them — and a mention that drags in someone the
//! viewer mutes so (`blocked_mention?`). `favourited_by` and `reblogged_by`
//! leave them out (`not_excluded_by_account`).

use crate::helpers::TestContext;

async fn home_ids(ctx: &TestContext, token: &str) -> Vec<String> {
    ctx.api
        .home_timeline(token)
        .await
        .iter()
        .filter_map(|s| s["id"].as_str().map(str::to_owned))
        .collect()
}

/// Mute Bob, hiding his notifications too — the strongest form the API offers.
async fn alice_mutes_bob(ctx: &TestContext) {
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({"notifications": true}),
        )
        .await;
    assert_eq!(resp.status().as_u16(), 200);
}

async fn notifications_from_bob(ctx: &TestContext, kind: &str) -> i64 {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "c!" FROM notifications
           WHERE account_id = $1 AND from_account_id = $2 AND type = $3"#,
        ctx.alice_id.parse::<i64>().unwrap(),
        ctx.bob_id.parse::<i64>().unwrap(),
        kind,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

// ── what is addressed to me ───────────────────────────────────────────────

/// A muted account's post stays out of home, one that mentions me included.
#[tokio::test]
async fn test_a_muted_post_mentioning_me_stays_out_of_home() {
    let ctx = TestContext::new("mute-reach-mention-home").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    alice_mutes_bob(&ctx).await;

    let ordinary = ctx
        .api
        .post_status(&ctx.bob_token, "bob talking to nobody", "public")
        .await;
    let addressed = ctx
        .api
        .post_status(&ctx.bob_token, "@alice what do you think?", "public")
        .await;

    let ids = home_ids(&ctx, &ctx.alice_token).await;
    assert!(
        !ids.contains(&ordinary["id"].as_str().unwrap().to_string()),
        "a muted account's ordinary post must stay out of home"
    );
    assert!(
        !ids.contains(&addressed["id"].as_str().unwrap().to_string()),
        "a muted account's post mentioning me stays out of home too"
    );
}

/// A muted account's boost stays out of home, a boost of my own post
/// included.
#[tokio::test]
async fn test_a_muted_boost_of_my_post_stays_out_of_home() {
    let ctx = TestContext::new("mute-reach-boost-home").await;
    // A boost of a post already among the newest in the feed is grouped
    // away (`aggregate_reblogs`); this is about the mute, so see every boost.
    ctx.api
        .patch_json(
            "/api/eunha/v1/preferences",
            Some(&ctx.alice_token),
            &serde_json::json!({ "aggregate_reblogs": false }),
        )
        .await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    alice_mutes_bob(&ctx).await;

    let mine = ctx
        .api
        .post_status(&ctx.alice_token, "alice says something", "public")
        .await;
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{}/reblog", mine["id"].as_str().unwrap()),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status().as_u16(), 200);
    let boost: serde_json::Value = resp.json().await.unwrap();

    let ids = home_ids(&ctx, &ctx.alice_token).await;
    assert!(
        !ids.contains(&boost["id"].as_str().unwrap().to_string()),
        "a muted account's boost of my own post stays out of home"
    );
}

/// `hide_notifications` drops a mention of me.
#[tokio::test]
async fn test_a_mute_drops_a_mention() {
    let ctx = TestContext::new("mute-reach-mention-notify").await;

    alice_mutes_bob(&ctx).await;
    ctx.api
        .post_status(&ctx.bob_token, "@alice are you there?", "public")
        .await;

    assert_eq!(
        notifications_from_bob(&ctx, "mention").await,
        0,
        "a mention from an account muted with its notifications is dropped"
    );
}

/// A mute that leaves notifications on still lets a mention through.
#[tokio::test]
async fn test_a_mute_keeping_notifications_notifies_a_mention() {
    let ctx = TestContext::new("mute-keep-notifications").await;
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({"notifications": false}),
        )
        .await;
    assert_eq!(resp.status().as_u16(), 200);
    ctx.api
        .post_status(&ctx.bob_token, "@alice are you there?", "public")
        .await;
    assert_eq!(notifications_from_bob(&ctx, "mention").await, 1);
}

/// `blocked_mention?`: a mention that also mentions someone I mute with their
/// notifications is dropped, whoever sent it.
#[tokio::test]
async fn test_a_mention_dragging_in_a_muted_account_is_dropped() {
    let ctx = TestContext::new("mute-third-party-mention").await;
    let (carol_id, carol_token) =
        crate::helpers::seed_account_and_token(&ctx.db, &ctx.domain, "carol", "carol@example.test")
            .await;
    alice_mutes_bob(&ctx).await;
    ctx.api
        .post_status(&carol_token, "@alice @bob both of you", "public")
        .await;
    let from_carol: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE account_id = $1 AND from_account_id = $2",
    )
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .bind(carol_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(from_carol, 0);
}

/// So is a favourite of a post of mine.
#[tokio::test]
async fn test_a_mute_drops_a_favourite_of_my_post() {
    let ctx = TestContext::new("mute-reach-favourite-notify").await;

    alice_mutes_bob(&ctx).await;
    let mine = ctx
        .api
        .post_status(&ctx.alice_token, "alice says something", "public")
        .await;
    let resp = ctx
        .api
        .post_json(
            &format!(
                "/api/v1/statuses/{}/favourite",
                mine["id"].as_str().unwrap()
            ),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status().as_u16(), 200);

    assert_eq!(
        notifications_from_bob(&ctx, "favourite").await,
        0,
        "a favourite from a muted account is dropped"
    );
}

/// And a boost of one.
#[tokio::test]
async fn test_a_mute_drops_a_boost_of_my_post() {
    let ctx = TestContext::new("mute-reach-boost-notify").await;

    alice_mutes_bob(&ctx).await;
    let mine = ctx
        .api
        .post_status(&ctx.alice_token, "alice says something", "public")
        .await;
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{}/reblog", mine["id"].as_str().unwrap()),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status().as_u16(), 200);

    assert_eq!(
        notifications_from_bob(&ctx, "reblog").await,
        0,
        "a boost from a muted account is dropped"
    );
}

/// `not_excluded_by_account`: who favourited a post leaves out the accounts I
/// mute, on a post of my own too.
#[tokio::test]
async fn test_favourited_by_leaves_out_a_muted_account() {
    let ctx = TestContext::new("mute-reach-favourited-by").await;

    alice_mutes_bob(&ctx).await;
    let mine = ctx
        .api
        .post_status(&ctx.alice_token, "alice says something", "public")
        .await;
    let id = mine["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/favourite"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;

    let body: Vec<serde_json::Value> = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{id}/favourited_by"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = body.iter().filter_map(|a| a["id"].as_str()).collect();
    assert!(
        !ids.contains(&ctx.bob_id.as_str()),
        "a muted account is left out of favourited_by"
    );
}

// ── what is not ───────────────────────────────────────────────────────────

/// A muted account's post that mentions somebody else stays hidden, in the
/// public timeline as in home.
#[tokio::test]
async fn test_a_muted_post_mentioning_someone_else_stays_hidden() {
    let ctx = TestContext::new("mute-reach-third-party").await;

    let (_carol_id, carol_token) =
        crate::helpers::seed_account_and_token(&ctx.db, &ctx.domain, "carol", "carol@example.test")
            .await;

    alice_mutes_bob(&ctx).await;
    let elsewhere = ctx
        .api
        .post_status(&ctx.bob_token, "@carol how are you?", "public")
        .await;
    // A control, so a timeline that returned nothing at all cannot pass this.
    let carols = ctx
        .api
        .post_status(&carol_token, "carol says something", "public")
        .await;

    let body: Vec<serde_json::Value> = ctx
        .api
        .get(
            "/api/v1/timelines/public?local=true",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = body.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        ids.contains(&carols["id"].as_str().unwrap()),
        "an unmuted account's post should be in the public timeline"
    );
    assert!(
        !ids.contains(&elsewhere["id"].as_str().unwrap()),
        "a muted account's post to a third party must stay hidden"
    );
}

/// `hide_notifications` silences a follow.
#[tokio::test]
async fn test_a_mute_still_hides_a_follow_notification() {
    let ctx = TestContext::new("mute-reach-not-follows").await;

    alice_mutes_bob(&ctx).await;
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    assert_eq!(
        notifications_from_bob(&ctx, "follow").await,
        0,
        "a follow from a muted account must stay silenced"
    );
}

/// And a post of their own I subscribed to.
#[tokio::test]
async fn test_a_mute_still_hides_an_ordinary_post_from_notifications() {
    let ctx = TestContext::new("mute-reach-not-ordinary").await;

    // Alice subscribes to Bob's posts — the "bell" — which is what produces a
    // `status` notification.
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({"notify": true}),
        )
        .await;
    assert_eq!(resp.status().as_u16(), 200);

    ctx.api
        .post_status(&ctx.bob_token, "bob before the mute", "public")
        .await;
    assert_eq!(
        notifications_from_bob(&ctx, "status").await,
        1,
        "the bell should notify before any mute"
    );

    alice_mutes_bob(&ctx).await;
    ctx.api
        .post_status(&ctx.bob_token, "bob talking to nobody", "public")
        .await;

    assert_eq!(
        notifications_from_bob(&ctx, "status").await,
        1,
        "a muted account's own post must not notify me"
    );
}
