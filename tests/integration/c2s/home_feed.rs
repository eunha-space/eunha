//! Home feed regeneration: `HomeFeed#regenerating?`, `User#regenerate_feed!`
//! on a returning user's sign-in, `FollowService#mark_home_feed_as_partial!`
//! and `Vacuum::FeedsVacuum`; and that reading a feed never fills it.

use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::TestContext;

fn home_key(ctx: &TestContext, suffix: &str) -> String {
    ctx.state
        .redis_keys
        .key(format!("feed:home:{}{suffix}", ctx.alice_id))
}

async fn feed_holds(ctx: &TestContext, status_id: &str) -> bool {
    let mut redis = ctx.state.redis.clone();
    let score: Option<f64> = redis::cmd("ZSCORE")
        .arg(home_key(ctx, ""))
        .arg(status_id)
        .query_async(&mut redis)
        .await
        .unwrap();
    score.is_some()
}

async fn feed_exists(ctx: &TestContext) -> bool {
    let mut redis = ctx.state.redis.clone();
    let exists: i64 = redis::cmd("EXISTS")
        .arg(home_key(ctx, ""))
        .query_async(&mut redis)
        .await
        .unwrap();
    exists == 1
}

fn contents(body: &Value) -> Vec<String> {
    body.as_array()
        .unwrap()
        .iter()
        .map(|s| s["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// While the feed regenerates, the home timeline answers `206` with what the
/// feed holds and a `Mastodon-Async-Refresh` header asking for a retry in five
/// seconds, whose id polls as running; once it finishes, `200` and no header.
#[tokio::test]
async fn test_home_timeline_is_partial_while_the_feed_regenerates() {
    let ctx = TestContext::new("home-feed-partial").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    // Build the feed first, as a signed-in user's is.
    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("mastodon-async-refresh").is_none());

    eunha::home_feed::regeneration_in_progress(&ctx.state, alice).await;
    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    let header = resp
        .headers()
        .get("mastodon-async-refresh")
        .expect("a refresh header")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(header.ends_with(", retry=5"), "{header}");
    let id = header
        .strip_prefix("id=\"")
        .and_then(|rest| rest.split_once('"'))
        .map(|(id, _)| id.to_owned())
        .unwrap();
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
    assert_eq!(body["async_refresh"]["status"], "running");

    eunha::home_feed::regeneration_finished(&ctx.state, alice).await;
    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("mastodon-async-refresh").is_none());
}

/// A regeneration key an older Mastodon left as a string reads as running,
/// replaced by a refresh hash (`HomeFeed#upgrade_redis_key!`).
#[tokio::test]
async fn test_a_string_regeneration_key_is_upgraded() {
    let ctx = TestContext::new("home-feed-upgrade").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let key = ctx
        .state
        .redis_keys
        .key(eunha::home_feed::regeneration_key(alice));
    let mut redis = ctx.state.redis_coordination.clone();
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg("true")
        .query_async(&mut redis)
        .await
        .unwrap();
    assert!(eunha::home_feed::regenerating(&ctx.state, alice).await);
    let status: Option<String> = redis::cmd("HGET")
        .arg(&key)
        .arg("status")
        .query_async(&mut redis)
        .await
        .unwrap();
    assert_eq!(status.as_deref(), Some("running"));
}

/// The feeds vacuum removes the feed of a user who has not signed in for a
/// week, the fan-out skips that user, and the user's next request — the
/// first in a day, so `require_user!` records the sign-in — regenerates it
/// with what was missed.
#[tokio::test]
async fn test_a_returning_users_feed_is_regenerated() {
    let ctx = TestContext::new("home-feed-returning").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let first = ctx
        .api
        .post_status(&ctx.bob_token, "before the break", "public")
        .await;
    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(feed_holds(&ctx, first["id"].as_str().unwrap()).await);

    // Alice goes away for eight days.
    sqlx::query(
        "UPDATE users SET current_sign_in_at = now() - interval '8 days',
                          last_sign_in_at = now() - interval '9 days'
         WHERE account_id = $1",
    )
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();
    let missed = ctx
        .api
        .post_status(&ctx.bob_token, "while you were away", "public")
        .await;
    assert!(
        !feed_holds(&ctx, missed["id"].as_str().unwrap()).await,
        "an inactive user's feed is not fanned out to"
    );
    let mut redis = ctx.state.redis.clone();
    eunha::feed::vacuum_inactive_feeds(&mut redis, &ctx.state.redis_keys, &ctx.db)
        .await
        .unwrap();
    assert!(!feed_exists(&ctx).await, "the vacuum removed the feed");

    // Tests run the regeneration in the request, so it has finished by the
    // time the timeline is read.
    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let texts = contents(&body);
    assert!(texts.iter().any(|t| t.contains("while you were away")));
    assert!(texts.iter().any(|t| t.contains("before the break")));
    assert!(feed_exists(&ctx).await);

    let (recent, previous): (bool, bool) = sqlx::query_as(
        "SELECT current_sign_in_at > now() - interval '1 minute',
                last_sign_in_at < now() - interval '7 days'
         FROM users WHERE account_id = $1",
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(recent && previous, "the sign-in was moved along");
    let refresh = eunha::home_feed::async_refresh(&ctx.state, alice).await;
    assert!(refresh.is_finished(), "the regeneration ran and finished");
}

/// The feeds vacuum takes only the feeds of members who have not signed in
/// for a week: an active member's feed, and every post it holds, stays.
#[tokio::test]
async fn test_the_feeds_vacuum_leaves_an_active_members_feed() {
    let ctx = TestContext::new("home-feed-vacuum-active").await;
    let bob: i64 = ctx.bob_id.parse().unwrap();
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let post = ctx
        .api
        .post_status(&ctx.bob_token, "kept through the vacuum", "public")
        .await;
    let post = post["id"].as_str().unwrap();
    assert!(feed_holds(&ctx, post).await);
    let bob_key = ctx.state.redis_keys.key(format!("feed:home:{bob}"));
    let mut redis = ctx.state.redis.clone();
    let held: i64 = redis::cmd("ZCARD")
        .arg(&bob_key)
        .query_async(&mut redis)
        .await
        .unwrap();
    assert!(held > 0, "bob's own post is in his feed");

    sqlx::query(
        "UPDATE users SET current_sign_in_at = now() - interval '8 days' WHERE account_id = $1",
    )
    .bind(bob)
    .execute(&ctx.db)
    .await
    .unwrap();
    eunha::feed::vacuum_inactive_feeds(&mut redis, &ctx.state.redis_keys, &ctx.db)
        .await
        .unwrap();

    assert!(feed_holds(&ctx, post).await, "alice's feed is untouched");
    let exists: i64 = redis::cmd("EXISTS")
        .arg(&bob_key)
        .query_async(&mut redis)
        .await
        .unwrap();
    assert_eq!(exists, 0, "the inactive member's feed is gone");
}

/// A user who signed in within the day is not signed in again by every
/// request, and so is not regenerated by one.
#[tokio::test]
async fn test_a_recent_sign_in_is_not_moved_by_every_request() {
    let ctx = TestContext::new("home-feed-sign-in-daily").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    sqlx::query(
        "UPDATE users SET current_sign_in_at = now() - interval '2 hours',
                          last_sign_in_at = now() - interval '30 days'
         WHERE account_id = $1",
    )
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();
    ctx.api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    let moved: bool = sqlx::query_scalar(
        "SELECT current_sign_in_at > now() - interval '1 hour' FROM users WHERE account_id = $1",
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(!moved);
}

/// An account that follows no one and asks to follow a locked account has a
/// partial home feed until the request is authorized and the followed
/// account's posts are merged in.
#[tokio::test]
async fn test_a_first_follow_leaves_the_feed_partial_until_merged() {
    let ctx = TestContext::new("home-feed-first-follow").await;
    let bob: i64 = ctx.bob_id.parse().unwrap();
    ctx.api
        .post_status(&ctx.bob_token, "locked away", "private")
        .await;
    sqlx::query("UPDATE accounts SET locked = true WHERE id = $1")
        .bind(bob)
        .execute(&ctx.db)
        .await
        .unwrap();
    let relationship = ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    assert_eq!(relationship["requested"], true);

    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
    assert!(resp.headers().get("mastodon-async-refresh").is_some());

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/follow_requests/{}/authorize", ctx.alice_id),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(contents(&body).iter().any(|t| t.contains("locked away")));
}

/// A new account's home feed is empty, and reading it answers `200` with
/// nothing in it: `HomeController#show` regenerates nothing, and only a
/// running regeneration makes the answer partial.
#[tokio::test]
async fn test_an_empty_feed_is_read_as_it_is() {
    let ctx = TestContext::new("home-feed-empty").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("mastodon-async-refresh").is_none());
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, serde_json::json!([]));
    assert!(!feed_exists(&ctx).await, "reading built nothing");
    let refresh = eunha::home_feed::async_refresh(&ctx.state, alice).await;
    assert!(!refresh.is_running() && !refresh.is_finished());
}

/// A feed Redis lost is not rebuilt by reading it: what was in it is gone
/// until a regeneration, and what is posted from then on goes in.
#[tokio::test]
async fn test_a_lost_feed_is_not_rebuilt_by_reading_it() {
    let ctx = TestContext::new("home-feed-lost").await;
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let lost = ctx
        .api
        .post_status(&ctx.bob_token, "lost with the feed", "public")
        .await;
    assert!(feed_holds(&ctx, lost["id"].as_str().unwrap()).await);
    let mut redis = ctx.state.redis.clone();
    let _: () = redis::cmd("DEL")
        .arg(home_key(&ctx, ""))
        .query_async(&mut redis)
        .await
        .unwrap();

    let resp = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, serde_json::json!([]));

    let after = ctx
        .api
        .post_status(&ctx.bob_token, "after the loss", "public")
        .await;
    assert!(feed_holds(&ctx, after["id"].as_str().unwrap()).await);
    let body: Value = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(contents(&body).len(), 1);
    assert!(contents(&body)[0].contains("after the loss"));
}

/// A feed another process built under Mastodon's keys alone is read and fed
/// as eunha's own: there is nothing else to mark it.
#[tokio::test]
async fn test_a_feed_built_elsewhere_is_fed_and_read() {
    let ctx = TestContext::new("home-feed-shared").await;
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let older = ctx
        .api
        .post_status(&ctx.bob_token, "built elsewhere", "public")
        .await;
    let older_id: i64 = older["id"].as_str().unwrap().parse().unwrap();
    // Rebuild the feed as a Mastodon sharing the Redis would leave it.
    let mut redis = ctx.state.redis.clone();
    let _: () = redis::cmd("DEL")
        .arg(home_key(&ctx, ""))
        .query_async(&mut redis)
        .await
        .unwrap();
    let _: () = redis::cmd("ZADD")
        .arg(home_key(&ctx, ""))
        .arg(older_id)
        .arg(older_id)
        .query_async(&mut redis)
        .await
        .unwrap();

    let newer = ctx
        .api
        .post_status(&ctx.bob_token, "fanned out here", "public")
        .await;
    assert!(feed_holds(&ctx, newer["id"].as_str().unwrap()).await);
    let body: Value = ctx
        .api
        .get("/api/v1/timelines/home", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let texts = contents(&body);
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(texts[0].contains("fanned out here"));
    assert!(texts[1].contains("built elsewhere"));
}

/// The fan-out filters as `FeedInsertWorker` does, before anything is
/// written: a muted account's post, a boost from a follow whose boosts are
/// hidden, and a post by a member of an exclusive list never enter the home
/// feed, though the last enters the list's.
#[tokio::test]
async fn test_the_fan_out_filters_before_writing() {
    let ctx = TestContext::new("home-feed-filter").await;
    let (carol_id, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let (dave_id, dave_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "dave", "dave@test.invalid").await;
    let carol_id = carol_id.to_string();
    let dave_id = dave_id.to_string();

    // Bob: followed with boosts hidden. Carol: followed and muted. Dave:
    // followed, in an exclusive list.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({ "reblogs": false }),
        )
        .await;
    ctx.api.follow(&ctx.alice_token, &carol_id).await;
    ctx.api.follow(&ctx.alice_token, &dave_id).await;
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{carol_id}/mute"),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;
    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &serde_json::json!({ "title": "exclusive", "exclusive": true }),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap().to_owned();
    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &serde_json::json!({ "account_ids": [dave_id] }),
        )
        .await;

    let bob_post = ctx
        .api
        .post_status(&ctx.bob_token, "bob's own post", "public")
        .await;
    let carol_post = ctx
        .api
        .post_status(&carol_token, "carol, muted", "public")
        .await;
    let dave_post = ctx
        .api
        .post_status(&dave_token, "dave, listed", "public")
        .await;
    let boost: Value = ctx
        .api
        .post_json(
            &format!(
                "/api/v1/statuses/{}/reblog",
                dave_post["id"].as_str().unwrap()
            ),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(feed_holds(&ctx, bob_post["id"].as_str().unwrap()).await);
    assert!(!feed_holds(&ctx, carol_post["id"].as_str().unwrap()).await);
    assert!(!feed_holds(&ctx, dave_post["id"].as_str().unwrap()).await);
    assert!(!feed_holds(&ctx, boost["id"].as_str().unwrap()).await);

    let mut redis = ctx.state.redis.clone();
    let in_list: Option<f64> = redis::cmd("ZSCORE")
        .arg(ctx.state.redis_keys.key(format!("feed:list:{list_id}")))
        .arg(dave_post["id"].as_str().unwrap())
        .query_async(&mut redis)
        .await
        .unwrap();
    assert!(
        in_list.is_some(),
        "the exclusive list takes its member's post"
    );
}

/// Muting clears the feed as `MuteWorker` does with
/// `FeedManager#clear_from_home`: the muted account's posts, boosts of them
/// and posts mentioning it go, others stay.
#[tokio::test]
async fn test_muting_clears_the_feed_of_the_account() {
    let ctx = TestContext::new("home-feed-clear-mute").await;
    let (carol_id, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let carol_id = carol_id.to_string();
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.alice_token, &carol_id).await;

    let carol_post = ctx
        .api
        .post_status(&carol_token, "carol's post", "public")
        .await;
    let carol_post_id = carol_post["id"].as_str().unwrap().to_owned();
    // Bob mentions Carol, then posts about no one.
    let mention = ctx
        .api
        .post_status(&ctx.bob_token, "hello @carol", "public")
        .await;
    let plain = ctx
        .api
        .post_status(&ctx.bob_token, "nothing about anyone", "public")
        .await;
    for id in [
        &carol_post_id,
        mention["id"].as_str().unwrap(),
        plain["id"].as_str().unwrap(),
    ] {
        assert!(feed_holds(&ctx, id).await, "{id} fanned out");
    }

    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{carol_id}/mute"),
            Some(&ctx.alice_token),
            &serde_json::json!({ "notifications": false }),
        )
        .await;
    assert!(!feed_holds(&ctx, &carol_post_id).await);
    assert!(!feed_holds(&ctx, mention["id"].as_str().unwrap()).await);
    assert!(feed_holds(&ctx, plain["id"].as_str().unwrap()).await);
}

/// Blocking runs `AfterBlockService`: the blocked account's posts and boosts
/// of them leave the feed, and its notifications go.
#[tokio::test]
async fn test_blocking_clears_the_feed_and_notifications() {
    let ctx = TestContext::new("home-feed-clear-block").await;
    let (carol_id, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let carol_id = carol_id.to_string();
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    // Carol, whom Alice does not follow, posts; Bob boosts it into Alice's
    // feed; Carol mentions Alice, which notifies her.
    let carol_post = ctx
        .api
        .post_status(&carol_token, "carol's post", "public")
        .await;
    let boost: Value = ctx
        .api
        .post_json(
            &format!(
                "/api/v1/statuses/{}/reblog",
                carol_post["id"].as_str().unwrap()
            ),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    let boost_id = boost["id"].as_str().unwrap().to_owned();
    assert!(feed_holds(&ctx, &boost_id).await);
    ctx.api
        .post_status(&carol_token, "hello @alice", "public")
        .await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let notifications_from_carol = || {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM notifications WHERE account_id = $1 AND from_account_id = $2",
        )
        .bind(alice)
        .bind(carol_id.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
    };
    assert!(notifications_from_carol().await.unwrap() > 0);

    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{carol_id}/block"),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;
    assert!(!feed_holds(&ctx, &boost_id).await);
    assert_eq!(notifications_from_carol().await.unwrap(), 0);
}

/// Blocking a domain undoes the follows there, which unmerges those
/// accounts' posts as `UnfollowService` does, and does nothing else to the
/// home feed: a boost already in it of a post from there stays, as
/// `AfterBlockDomainFromAccountService` leaves it.
#[tokio::test]
async fn test_a_domain_block_unmerges_only_through_the_follows_it_ends() {
    let ctx = TestContext::new("home-feed-domain-block").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let domain = format!("blocked-{}", ctx.domain);
    let actor_uri = format!("https://{domain}/users/stranger");
    let stranger = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts
             (id, username, domain, display_name, note, url, uri, public_key,
              inbox_url, outbox_url, protocol, created_at, updated_at)
           VALUES ($1, 'stranger', $2, 'stranger', '', $3, $3, 'remote-key',
                   '', '', 1, now(), now())"#,
    )
    .bind(stranger)
    .bind(&domain)
    .bind(&actor_uri)
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, now(), now())",
    )
    .bind(alice)
    .bind(stranger)
    .execute(&ctx.db)
    .await
    .unwrap();
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let mut posts = vec![];
    for n in 1..=2 {
        let post = eunha::snowflake::next_id();
        sqlx::query(
            r#"INSERT INTO statuses (id, account_id, text, visibility, uri, url, local, created_at, updated_at)
               VALUES ($1, $2, 'from elsewhere', 0, $3, $3, false, now(), now())"#,
        )
        .bind(post)
        .bind(stranger)
        .bind(format!("https://{domain}/notes/{n}"))
        .execute(&ctx.db)
        .await
        .unwrap();
        posts.push(post);
    }
    // The first reaches Alice's feed through her follow; the second only as
    // Bob's boost.
    let mut redis = ctx.state.redis.clone();
    eunha::feed::fanout_status(&mut redis, &ctx.state.redis_keys, &ctx.db, posts[0], false).await;
    let boost: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{}/reblog", posts[1]),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    let boost_id = boost["id"].as_str().unwrap().to_owned();
    assert!(feed_holds(&ctx, &posts[0].to_string()).await);
    assert!(feed_holds(&ctx, &boost_id).await);

    ctx.api
        .post_json(
            "/api/v1/domain_blocks",
            Some(&ctx.alice_token),
            &serde_json::json!({ "domain": domain }),
        )
        .await;
    assert!(
        !feed_holds(&ctx, &posts[0].to_string()).await,
        "unmerged with the follow"
    );
    assert!(feed_holds(&ctx, &boost_id).await, "Bob's boost stays");
}

/// An edit runs the fan-out again with `update`: a follower whose filters
/// now keep the post out (here, a follow limited to English, and the post
/// edited into Korean) has it taken out of the feed.
#[tokio::test]
async fn test_an_edit_takes_a_post_out_where_it_is_now_filtered() {
    let ctx = TestContext::new("home-feed-edit").await;
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({ "languages": ["en"] }),
        )
        .await;
    let post: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &serde_json::json!({ "status": "in english", "language": "en" }),
        )
        .await
        .json()
        .await
        .unwrap();
    let id = post["id"].as_str().unwrap().to_owned();
    assert!(feed_holds(&ctx, &id).await);

    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.bob_token),
            &serde_json::json!({ "status": "한국어로", "language": "ko" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!feed_holds(&ctx, &id).await, "the edit took it out");
    // Bob's own feed keeps it: `deliver_to_self!` is not filtered.
    let mut redis = ctx.state.redis.clone();
    let own: Option<f64> = redis::cmd("ZSCORE")
        .arg(
            ctx.state
                .redis_keys
                .key(format!("feed:home:{}", ctx.bob_id)),
        )
        .arg(&id)
        .query_async(&mut redis)
        .await
        .unwrap();
    assert!(own.is_some());
}

/// The bell (`FeedInsertWorker#notify?`): a follower notified of each new
/// post once, but not of a reply to someone else, a boost, or an edit.
#[tokio::test]
async fn test_the_bell_notifies_as_feed_insert_worker_does() {
    let ctx = TestContext::new("home-feed-bell").await;
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({ "notify": true }),
        )
        .await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    let count = || {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM notifications
             WHERE account_id = $1 AND from_account_id = $2 AND type = 'status'",
        )
        .bind(alice)
        .bind(bob)
        .fetch_one(&ctx.db)
    };

    let post = ctx.api.post_status(&ctx.bob_token, "news", "public").await;
    let id = post["id"].as_str().unwrap().to_owned();
    assert_eq!(count().await.unwrap(), 1);

    // An edit does not notify again.
    ctx.api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.bob_token),
            &serde_json::json!({ "status": "news, corrected" }),
        )
        .await;
    assert_eq!(count().await.unwrap(), 1);

    // Nor does a reply to someone else, or a boost.
    let other = ctx
        .api
        .post_status(&ctx.alice_token, "alice's own", "public")
        .await;
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &serde_json::json!({
                "status": "replying",
                "in_reply_to_id": other["id"].as_str().unwrap(),
            }),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{}/reblog", other["id"].as_str().unwrap()),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(count().await.unwrap(), 1);

    // A self-reply does.
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &serde_json::json!({ "status": "and more", "in_reply_to_id": id }),
        )
        .await;
    assert_eq!(count().await.unwrap(), 2);
}
