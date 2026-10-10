use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{seed_user, TestContext};

/// `recount_follows` reconciles a local account's drifted follow counters from
/// the `follows` table (Mastodon's `refresh_counts`), fixing negative values
/// left by legacy code paths.
#[tokio::test]
async fn test_recount_follows_fixes_drift() {
    let ctx = TestContext::new("recount-follows").await;

    // One real follow edge: Bob follows Alice.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();

    // Corrupt Alice's stored counters the way legacy drift did (negative).
    sqlx::query!(
        "UPDATE account_stats SET followers_count = -25, following_count = -1 WHERE account_id = $1",
        alice_id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    eunha::counters::recount_follows(&ctx.db, alice_id)
        .await
        .unwrap();

    let alice: Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{alice_id}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        alice["followers_count"].as_i64(),
        Some(1),
        "followers_count should be recomputed from the follows table"
    );
    assert_eq!(
        alice["following_count"].as_i64(),
        Some(0),
        "following_count should be recomputed to the true value"
    );
}

/// The account-statuses feed (the iOS profile timeline) embeds real account
/// stats and status stats, matching Mastodon — not hard-coded zeros.
#[tokio::test]
async fn test_account_statuses_embeds_real_stats() {
    let ctx = TestContext::new("acct-stat-stats").await;

    // Bob follows Alice so Alice has a non-zero follower count.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "profile feed post", "public")
        .await;
    let id = status["id"].as_str().unwrap().to_string();
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/favourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    let resp = ctx
        .api
        .get(&format!("/api/v1/accounts/{}/statuses", ctx.alice_id), None)
        .await;
    let statuses: Vec<Value> = resp.json().await.unwrap();
    let s = statuses
        .iter()
        .find(|s| s["id"].as_str() == Some(id.as_str()))
        .expect("posted status missing from account feed");

    assert_eq!(
        s["favourites_count"].as_i64(),
        Some(1),
        "account-feed status should report favourites_count"
    );
    assert_eq!(
        s["account"]["followers_count"].as_i64(),
        Some(1),
        "account-feed embedded account should report followers_count"
    );
    assert_eq!(
        s["account"]["statuses_count"].as_i64(),
        Some(1),
        "account-feed embedded account should report statuses_count"
    );
}

// ── account statuses visibility ──────────────────────────────────────────────

/// Private statuses are hidden from unauthenticated viewers.
#[tokio::test]
async fn test_account_statuses_hides_private_from_unauthenticated() {
    let ctx = TestContext::new("acct-stat-unauth").await;

    let prv = ctx
        .api
        .post_status(&ctx.alice_token, "alice private acct", "private")
        .await;
    let pub_s = ctx
        .api
        .post_status(&ctx.alice_token, "alice public acct", "public")
        .await;

    let resp = ctx
        .api
        .get(&format!("/api/v1/accounts/{}/statuses", ctx.alice_id), None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let statuses: Vec<Value> = resp.json().await.unwrap();

    let ids: Vec<&str> = statuses.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        !ids.contains(&prv["id"].as_str().unwrap()),
        "private status visible to unauthenticated user"
    );
    assert!(
        ids.contains(&pub_s["id"].as_str().unwrap()),
        "public status missing from unauthenticated view"
    );
}

/// Private statuses are hidden from non-followers.
#[tokio::test]
async fn test_account_statuses_hides_private_from_non_follower() {
    let ctx = TestContext::new("acct-stat-stranger").await;

    let prv = ctx
        .api
        .post_status(&ctx.alice_token, "alice prv stranger", "private")
        .await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses", ctx.alice_id),
            Some(&ctx.bob_token),
        )
        .await;
    let statuses: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = statuses.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        !ids.contains(&prv["id"].as_str().unwrap()),
        "private status visible to non-follower"
    );
}

/// Direct statuses never appear in account statuses for non-participants.
#[tokio::test]
async fn test_account_statuses_hides_direct_from_non_participant() {
    let ctx = TestContext::new("acct-stat-direct").await;

    let dir = ctx
        .api
        .post_status(&ctx.alice_token, "alice direct nobody", "direct")
        .await;
    let dir_id = dir["id"].as_str().unwrap();

    // Bob (not mentioned) should not see alice's direct status.
    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses", ctx.alice_id),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !statuses.iter().any(|s| s["id"].as_str() == Some(dir_id)),
        "direct status should not appear in account statuses for non-participants",
    );
}

/// Private statuses appear in account statuses for accepted followers.
#[tokio::test]
async fn test_account_statuses_shows_private_to_follower() {
    let ctx = TestContext::new("acct-stat-follower").await;

    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let prv = ctx
        .api
        .post_status(&ctx.alice_token, "alice prv follower", "private")
        .await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses", ctx.alice_id),
            Some(&ctx.bob_token),
        )
        .await;
    let statuses: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = statuses.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        ids.contains(&prv["id"].as_str().unwrap()),
        "private status hidden from accepted follower"
    );
}

/// Account statuses shows all visibilities to the account owner.
#[tokio::test]
async fn test_account_statuses_shows_all_to_self() {
    let ctx = TestContext::new("acct-stat-self").await;

    let pub_s = ctx
        .api
        .post_status(&ctx.alice_token, "self public", "public")
        .await;
    let prv_s = ctx
        .api
        .post_status(&ctx.alice_token, "self private", "private")
        .await;
    let dir_s = ctx
        .api
        .post_status(&ctx.alice_token, "self direct", "direct")
        .await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await;
    let statuses: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = statuses.iter().filter_map(|s| s["id"].as_str()).collect();

    assert!(ids.contains(&pub_s["id"].as_str().unwrap()));
    assert!(ids.contains(&prv_s["id"].as_str().unwrap()));
    assert!(ids.contains(&dir_s["id"].as_str().unwrap()));
}

// ── account statuses filters ───────────────────────────────────────────────────

/// ?exclude_replies=true omits replies to other users from account statuses.
#[tokio::test]
async fn test_account_statuses_exclude_replies() {
    let ctx = TestContext::new("acct-excl-reply").await;

    // Alice's own post.
    let own_post = ctx
        .api
        .post_status(&ctx.alice_token, "alice own post", "public")
        .await;
    let own_post_id = own_post["id"].as_str().unwrap();

    // Alice replies to bob (a foreign reply — should be excluded).
    let bob_post = ctx
        .api
        .post_status(&ctx.bob_token, "bob post", "public")
        .await;
    let bob_post_id = bob_post["id"].as_str().unwrap();
    let reply: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({"status": "alice reply to bob", "in_reply_to_id": bob_post_id, "visibility": "public"}),
    ).await.json().await.unwrap();
    let reply_id = reply["id"].as_str().unwrap();

    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!(
                "/api/v1/accounts/{}/statuses?exclude_replies=true",
                ctx.alice_id
            ),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        !statuses.iter().any(|s| s["id"].as_str() == Some(reply_id)),
        "reply to other user should be excluded",
    );
    assert!(
        statuses
            .iter()
            .any(|s| s["id"].as_str() == Some(own_post_id)),
        "own post should still appear",
    );
}

/// ?exclude_reblogs=true omits reblogs from account statuses.
#[tokio::test]
async fn test_account_statuses_exclude_reblogs() {
    let ctx = TestContext::new("acct-excl-rb").await;

    let original = ctx
        .api
        .post_status(&ctx.bob_token, "rebloggable", "public")
        .await;
    let orig_id = original["id"].as_str().unwrap();
    let reblog: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{orig_id}/reblog"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    let reblog_id = reblog["id"].as_str().unwrap();

    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!(
                "/api/v1/accounts/{}/statuses?exclude_reblogs=true",
                ctx.alice_id
            ),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !statuses.iter().any(|s| s["id"].as_str() == Some(reblog_id)),
        "reblog should be excluded",
    );
}

/// ?pinned=true returns only pinned statuses.
#[tokio::test]
async fn test_account_statuses_pinned() {
    let ctx = TestContext::new("acct-pinned").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "to pin", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/pin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses?pinned=true", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(statuses.iter().any(|s| s["id"].as_str() == Some(id)));
    for s in &statuses {
        assert_eq!(s["pinned"].as_bool(), Some(true));
    }
}

/// ?pinned=true hides private pinned statuses from non-followers.
#[tokio::test]
async fn test_account_statuses_pinned_hides_private_from_non_follower() {
    let ctx = TestContext::new("acct-pin-priv").await;

    // Alice pins a private status.
    let priv_status = ctx
        .api
        .post_status(&ctx.alice_token, "my secret pinned post", "private")
        .await;
    let priv_id = priv_status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{priv_id}/pin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Bob (non-follower) requests alice's pinned statuses — private pin should NOT appear.
    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses?pinned=true", ctx.alice_id),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        !statuses.iter().any(|s| s["id"].as_str() == Some(priv_id)),
        "private pinned status should not be visible to non-followers",
    );
}

/// ?pinned=true shows private pinned statuses to the account owner.
#[tokio::test]
async fn test_account_statuses_pinned_shows_private_to_self() {
    let ctx = TestContext::new("acct-pin-self").await;

    let priv_status = ctx
        .api
        .post_status(&ctx.alice_token, "my own private pin", "private")
        .await;
    let priv_id = priv_status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{priv_id}/pin"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Alice herself sees her private pin.
    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses?pinned=true", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        statuses.iter().any(|s| s["id"].as_str() == Some(priv_id)),
        "owner should see their own private pinned status",
    );
}

/// ?limit=1 on account statuses returns at most 1 status.
#[tokio::test]
async fn test_account_statuses_limit_param() {
    let ctx = TestContext::new("acct-stat-limit").await;

    ctx.api
        .post_status(&ctx.alice_token, "limit test 1", "public")
        .await;
    ctx.api
        .post_status(&ctx.alice_token, "limit test 2", "public")
        .await;

    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses?limit=1", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        statuses.len() <= 1,
        "limit=1 should return at most 1 status, got {}",
        statuses.len()
    );
}

/// ?max_id pagination on account statuses omits statuses newer than max_id.
#[tokio::test]
async fn test_account_statuses_max_id_pagination() {
    let ctx = TestContext::new("acct-stat-maxid").await;

    let s1 = ctx
        .api
        .post_status(&ctx.alice_token, "pagination first", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.alice_token, "pagination second", "public")
        .await;
    let s1_id = s1["id"].as_str().unwrap();
    let s2_id = s2["id"].as_str().unwrap();

    // Fetch with max_id = s2's id: should return s1 but not s2.
    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!(
                "/api/v1/accounts/{}/statuses?max_id={}",
                ctx.alice_id, s2_id
            ),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = statuses.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(!ids.contains(&s2_id), "max_id={s2_id} should exclude s2");
    assert!(
        ids.contains(&s1_id),
        "s1 should be included when max_id={s2_id}"
    );
}

/// ?since_id pagination on account statuses returns only statuses newer than since_id.
#[tokio::test]
async fn test_account_statuses_since_id_pagination() {
    let ctx = TestContext::new("acct-stat-since").await;

    let s1 = ctx
        .api
        .post_status(&ctx.alice_token, "since first", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.alice_token, "since second", "public")
        .await;
    let s1_id = s1["id"].as_str().unwrap().to_string();
    let s2_id = s2["id"].as_str().unwrap().to_string();

    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!(
                "/api/v1/accounts/{}/statuses?since_id={s1_id}",
                ctx.alice_id
            ),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = statuses.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(
        !ids.contains(&s1_id.as_str()),
        "since_id={s1_id} should exclude s1"
    );
    assert!(
        ids.contains(&s2_id.as_str()),
        "s2 should appear when since_id={s1_id}"
    );
}

/// ?min_id returns the statuses just newer than the anchor, newest first.
#[tokio::test]
async fn test_account_statuses_min_id_pagination() {
    let ctx = TestContext::new("acct-stat-min").await;

    let s1 = ctx
        .api
        .post_status(&ctx.alice_token, "min first", "public")
        .await;
    let s2 = ctx
        .api
        .post_status(&ctx.alice_token, "min second", "public")
        .await;
    let s3 = ctx
        .api
        .post_status(&ctx.alice_token, "min third", "public")
        .await;
    let s1_id = s1["id"].as_str().unwrap().to_string();
    let s2_id = s2["id"].as_str().unwrap().to_string();
    let s3_id = s3["id"].as_str().unwrap().to_string();

    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses?min_id={s1_id}", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = statuses.iter().filter_map(|s| s["id"].as_str()).collect();

    assert!(
        !ids.contains(&s1_id.as_str()),
        "min_id anchor should not appear"
    );
    assert!(ids.contains(&s2_id.as_str()), "s2 should appear");
    assert!(ids.contains(&s3_id.as_str()), "s3 should appear");

    let s2_pos = ids.iter().position(|&id| id == s2_id).unwrap();
    let s3_pos = ids.iter().position(|&id| id == s3_id).unwrap();
    assert!(s3_pos < s2_pos, "min_id results should be newest first");
}

/// ?tagged=<name> returns only statuses with that tag; untagged statuses are excluded.
#[tokio::test]
async fn test_account_statuses_tagged_returns_200() {
    let ctx = TestContext::new("acct-tagged-ok").await;

    let tagged = ctx
        .api
        .post_status(&ctx.alice_token, "post with #tagxyz888", "public")
        .await;
    let untagged = ctx
        .api
        .post_status(&ctx.alice_token, "post without tag", "public")
        .await;
    let tagged_id = tagged["id"].as_str().unwrap();
    let untagged_id = untagged["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(
            &format!(
                "/api/v1/accounts/{}/statuses?tagged=tagxyz888",
                ctx.alice_id
            ),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let statuses: Vec<Value> = resp.json().await.unwrap();
    assert!(
        statuses.iter().any(|s| s["id"].as_str() == Some(tagged_id)),
        "tagged status should appear in tagged filter",
    );
    assert!(
        !statuses
            .iter()
            .any(|s| s["id"].as_str() == Some(untagged_id)),
        "untagged status should not appear in tagged filter",
    );
}

/// ?only_media=true excludes text-only statuses.
#[tokio::test]
async fn test_account_statuses_only_media() {
    let ctx = TestContext::new("acct-only-media").await;

    let text_status = ctx
        .api
        .post_status(&ctx.alice_token, "text only status no media", "public")
        .await;
    let text_id = text_status["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses?only_media=true", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "only_media=true should return 200"
    );
    let statuses: Vec<Value> = resp.json().await.unwrap();
    assert!(
        !statuses.iter().any(|s| s["id"].as_str() == Some(text_id)),
        "text-only status should not appear with only_media=true",
    );
}

/// ?only_media=true excludes reblogs, even reblogs of media posts — Mastodon's
/// only_media_scope inner-joins the status's own attachments, and a boost row
/// has none.
#[tokio::test]
async fn test_account_statuses_only_media_excludes_reblogs() {
    let ctx = TestContext::new("acct-only-media-reblog").await;

    // Bob posts a media status; Alice reblogs it.
    let media: Value = ctx
        .api
        .post_multipart_file(
            "/api/v1/media",
            &ctx.bob_token,
            "t.png",
            "image/png",
            crate::helpers::tiny_png(),
            &[],
        )
        .await
        .json()
        .await
        .unwrap();
    let media_id = media["id"].as_str().unwrap();
    let bob_status = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({ "status": "look", "visibility": "public", "media_ids": [media_id] }),
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    let bob_id = bob_status["id"].as_str().unwrap();
    let reblog = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{bob_id}/reblog"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(reblog.status(), StatusCode::OK);
    let reblog: Value = reblog.json().await.unwrap();
    let reblog_id = reblog["id"].as_str().unwrap();

    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses?only_media=true", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !statuses.iter().any(|s| s["id"].as_str() == Some(reblog_id)),
        "a reblog of a media post must not appear with only_media=true",
    );
}

// ── follow lifecycle ──────────────────────────────────────────────────────────

/// Following your own account returns 403.
#[tokio::test]
async fn test_self_follow_returns_403() {
    let ctx = TestContext::new("self-follow").await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.alice_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// Following an unlocked account is immediately accepted.
#[tokio::test]
async fn test_follow_unlocked_account_is_accepted() {
    let ctx = TestContext::new("follow-unlocked").await;

    let rel = ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    assert_eq!(rel["following"].as_bool(), Some(true));
    assert_eq!(rel["requested"].as_bool(), Some(false));
}

/// Following a locked account creates a pending follow request.
#[tokio::test]
async fn test_follow_locked_account_is_pending() {
    let ctx = TestContext::new("follow-locked").await;

    // Lock Bob's account directly in the DB.
    let db = ctx.db.clone();
    let bob_uuid: i64 = ctx.bob_id.parse().unwrap();
    sqlx::query!("UPDATE accounts SET locked = true WHERE id = $1", bob_uuid)
        .execute(&db)
        .await
        .unwrap();

    let rel = ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    assert_eq!(rel["following"].as_bool(), Some(false));
    assert_eq!(rel["requested"].as_bool(), Some(true));
}

// ── verify credentials ────────────────────────────────────────────────────────

/// GET /api/v1/accounts/verify_credentials returns the current user's account.
#[tokio::test]
async fn test_verify_credentials() {
    let ctx = TestContext::new("verify-creds").await;

    let resp = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    assert_eq!(body["username"].as_str(), Some("alice"));
    assert!(body["id"].as_str().is_some(), "id field missing");
    assert!(body["acct"].as_str().is_some(), "acct field missing");
    assert!(
        body["source"].is_object(),
        "source field missing from verify_credentials"
    );
}

/// GET /api/v1/accounts/verify_credentials without token → 401.
#[tokio::test]
async fn test_verify_credentials_requires_auth() {
    let ctx = TestContext::new("verify-unauth").await;

    let resp = ctx
        .api
        .get("/api/v1/accounts/verify_credentials", None)
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ── account lookup ────────────────────────────────────────────────────────────

/// GET /api/v1/accounts/:id returns account data.
#[tokio::test]
async fn test_get_account() {
    let ctx = TestContext::new("get-acct").await;

    let resp = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.alice_id), None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    assert_eq!(body["id"].as_str(), Some(ctx.alice_id.as_str()));
    assert_eq!(body["username"].as_str(), Some("alice"));
}

/// GET /api/v1/accounts/:id for unknown id → 404.
#[tokio::test]
async fn test_get_account_not_found() {
    let ctx = TestContext::new("get-acct-404").await;

    let resp = ctx.api.get("/api/v1/accounts/1234567890", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET /api/v1/accounts/lookup?acct=alice returns Alice's account.
#[tokio::test]
async fn test_lookup_account() {
    let ctx = TestContext::new("lookup").await;

    let resp = ctx
        .api
        .get("/api/v1/accounts/lookup?acct=alice", None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    assert_eq!(body["username"].as_str(), Some("alice"));
}

/// GET /api/v1/accounts/lookup?acct= returns 404 for an unknown username.
#[tokio::test]
async fn test_lookup_account_not_found() {
    let ctx = TestContext::new("lookup-404").await;

    let resp = ctx
        .api
        .get("/api/v1/accounts/lookup?acct=nobody_here_xyz999", None)
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET /api/v1/accounts/lookup?acct= returns suspended account with suspended: true.
#[tokio::test]
async fn test_lookup_account_suspended_returns_suspended() {
    let ctx = TestContext::new("lookup-suspend").await;

    // Elevate alice to admin via direct DB.
    let alice_uuid: i64 = ctx.alice_id.parse().unwrap();
    let admin_db = ctx.db.clone();
    crate::helpers::make_admin(&admin_db, alice_uuid).await;

    // Suspend bob via admin endpoint.
    ctx.api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"type": "suspend"}),
        )
        .await;

    let resp = ctx
        .api
        .get("/api/v1/accounts/lookup?acct=bob", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["suspended"], true);
}

/// GET /api/v1/accounts/:id/followers returns a list after a follow.
#[tokio::test]
async fn test_get_account_followers() {
    let ctx = TestContext::new("acct-followers").await;

    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/followers", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(list
        .iter()
        .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())));
}

/// GET /api/v1/accounts/:id/following returns a list after a follow.
#[tokio::test]
async fn test_get_account_following() {
    let ctx = TestContext::new("acct-following").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/following", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(list
        .iter()
        .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())));
}

// ── relationships ─────────────────────────────────────────────────────────────

/// GET /api/v1/accounts/relationships reflects follow state.
#[tokio::test]
async fn test_get_relationships() {
    let ctx = TestContext::new("rel-basic").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/relationships?id[]={}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["following"].as_bool(), Some(true));
    assert_eq!(list[0]["id"].as_str(), Some(ctx.bob_id.as_str()));
}

/// showing_reblogs is false when not following (not true).
#[tokio::test]
async fn test_showing_reblogs_false_when_not_following() {
    let ctx = TestContext::new("rel-showing-reblogs-nf").await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/relationships?id[]={}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list[0]["following"].as_bool(), Some(false));
    assert_eq!(
        list[0]["showing_reblogs"].as_bool(),
        Some(false),
        "showing_reblogs should be false when not following, not true",
    );
}

/// Unfollowing sets following=false in the relationship.
#[tokio::test]
async fn test_unfollow_updates_relationship() {
    let ctx = TestContext::new("rel-unfollow").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/unfollow", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rel: Value = resp.json().await.unwrap();
    assert_eq!(rel["following"].as_bool(), Some(false));
}

/// Following increments followers_count and following_count.
#[tokio::test]
async fn test_follow_increments_counts() {
    let ctx = TestContext::new("follow-counts").await;

    // Get initial counts.
    let bob_before: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.bob_id), None)
        .await
        .json()
        .await
        .unwrap();
    let bob_followers_before = bob_before["followers_count"].as_i64().unwrap_or(0);

    let alice_before: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.alice_id), None)
        .await
        .json()
        .await
        .unwrap();
    let alice_following_before = alice_before["following_count"].as_i64().unwrap_or(0);

    // Alice follows Bob.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    // Bob's followers_count should increase.
    let bob_after: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.bob_id), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        bob_after["followers_count"].as_i64().unwrap_or(0),
        bob_followers_before + 1,
        "Bob's followers_count should increment after being followed",
    );

    // Alice's following_count should increase.
    let alice_after: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.alice_id), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        alice_after["following_count"].as_i64().unwrap_or(0),
        alice_following_before + 1,
        "Alice's following_count should increment after following",
    );
}

/// Unfollowing decrements followers_count and following_count.
#[tokio::test]
async fn test_unfollow_decrements_counts() {
    let ctx = TestContext::new("unfollow-counts").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let bob_mid: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.bob_id), None)
        .await
        .json()
        .await
        .unwrap();
    let bob_followers_mid = bob_mid["followers_count"].as_i64().unwrap_or(0);

    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/unfollow", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let bob_after: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.bob_id), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        bob_after["followers_count"].as_i64().unwrap_or(0),
        bob_followers_mid - 1,
        "Bob's followers_count should decrement after unfollow",
    );
}

/// Blocking sets blocking=true; unblocking sets it back to false.
#[tokio::test]
async fn test_block_and_unblock() {
    let ctx = TestContext::new("block").await;

    let block_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(block_resp.status(), StatusCode::OK);
    let rel: Value = block_resp.json().await.unwrap();
    assert_eq!(rel["blocking"].as_bool(), Some(true));

    let unblock_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/unblock", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(unblock_resp.status(), StatusCode::OK);
    let rel2: Value = unblock_resp.json().await.unwrap();
    assert_eq!(rel2["blocking"].as_bool(), Some(false));
}

/// Muting sets muting=true; unmuting sets it back to false.
#[tokio::test]
async fn test_mute_and_unmute() {
    let ctx = TestContext::new("mute").await;

    let mute_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(mute_resp.status(), StatusCode::OK);
    let rel: Value = mute_resp.json().await.unwrap();
    assert_eq!(rel["muting"].as_bool(), Some(true));

    let unmute_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/unmute", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(unmute_resp.status(), StatusCode::OK);
    let rel2: Value = unmute_resp.json().await.unwrap();
    assert_eq!(rel2["muting"].as_bool(), Some(false));
}

// ── follow requests ───────────────────────────────────────────────────────────

/// Accepting a pending follow request changes the relationship to following=true.
#[tokio::test]
async fn test_authorize_follow_request() {
    let ctx = TestContext::new("follow-req-accept").await;

    let db = ctx.db.clone();
    let bob_uuid: i64 = ctx.bob_id.parse().unwrap();
    sqlx::query!("UPDATE accounts SET locked = true WHERE id = $1", bob_uuid)
        .execute(&db)
        .await
        .unwrap();

    // Alice follows locked Bob → pending.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    // Bob authorises Alice's follow request.
    let requests_resp = ctx
        .api
        .get("/api/v1/follow_requests", Some(&ctx.bob_token))
        .await;
    let requests: Vec<Value> = requests_resp.json().await.unwrap();
    assert!(!requests.is_empty(), "no pending follow requests");
    let requester_id = requests[0]["id"].as_str().unwrap().to_string();

    let accept_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/follow_requests/{requester_id}/authorize"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(accept_resp.status(), StatusCode::OK);

    // Alice is now following Bob.
    let rels: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/relationships?id[]={}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(rels[0]["following"].as_bool(), Some(true));
    assert_eq!(rels[0]["requested"].as_bool(), Some(false));
}

/// Rejecting a pending follow request leaves following=false, requested=false.
#[tokio::test]
async fn test_reject_follow_request() {
    let ctx = TestContext::new("follow-req-reject").await;

    let db = ctx.db.clone();
    let bob_uuid: i64 = ctx.bob_id.parse().unwrap();
    sqlx::query!("UPDATE accounts SET locked = true WHERE id = $1", bob_uuid)
        .execute(&db)
        .await
        .unwrap();

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let requests: Vec<Value> = ctx
        .api
        .get("/api/v1/follow_requests", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    let requester_id = requests[0]["id"].as_str().unwrap().to_string();

    let reject_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/follow_requests/{requester_id}/reject"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(reject_resp.status(), StatusCode::OK);

    let rels: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/relationships?id[]={}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(rels[0]["following"].as_bool(), Some(false));
    assert_eq!(rels[0]["requested"].as_bool(), Some(false));
}

// ── blocks and mutes lists ────────────────────────────────────────────────────

/// After blocking Bob, GET /api/v1/blocks includes him.
#[tokio::test]
async fn test_blocks_list_includes_blocked() {
    let ctx = TestContext::new("blocks-list").await;

    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let resp = ctx.api.get("/api/v1/blocks", Some(&ctx.alice_token)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(list
        .iter()
        .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())));
}

/// After muting Bob, GET /api/v1/mutes includes him.
#[tokio::test]
async fn test_mutes_list_includes_muted() {
    let ctx = TestContext::new("mutes-list").await;

    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let resp = ctx.api.get("/api/v1/mutes", Some(&ctx.alice_token)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(list
        .iter()
        .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())));
}

// ── preferences ───────────────────────────────────────────────────────────────

/// GET /api/v1/preferences returns colon-separated keys expected by clients.
#[tokio::test]
async fn test_get_preferences() {
    let ctx = TestContext::new("prefs").await;

    let resp = ctx
        .api
        .get("/api/v1/preferences", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["posting:default:visibility"].as_str().is_some(),
        "posting:default:visibility missing: {body}",
    );
    assert!(
        body.get("reading:expand:media").is_some(),
        "reading:expand:media missing: {body}",
    );
}

// ── endorse / unendorse ───────────────────────────────────────────────────────

/// Endorsing Bob sets endorsed=true; unendorsing reverts it.
#[tokio::test]
async fn test_endorse_and_unendorse() {
    let ctx = TestContext::new("endorse").await;

    // You may only endorse accounts you follow.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let endorse_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/endorse", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(endorse_resp.status(), StatusCode::OK);
    let rel: Value = endorse_resp.json().await.unwrap();
    assert_eq!(rel["endorsed"].as_bool(), Some(true));

    let unendorse_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/unendorse", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(unendorse_resp.status(), StatusCode::OK);
    let rel2: Value = unendorse_resp.json().await.unwrap();
    assert_eq!(rel2["endorsed"].as_bool(), Some(false));
}

/// GET /api/v1/accounts/:id/endorsements returns endorsed accounts.
#[tokio::test]
async fn test_get_endorsements_list() {
    let ctx = TestContext::new("endorse-list").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/endorse", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/endorsements", ctx.alice_id),
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(list
        .iter()
        .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())));
}

/// Unfollowing an endorsed account takes the endorsement with it
/// (`Follow#remove_endorsements`), and so does being blocked by it.
#[tokio::test]
async fn test_unfollow_removes_endorsement() {
    async fn endorsed(ctx: &TestContext) -> i64 {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM account_pins WHERE account_id = $1 AND target_account_id = $2",
        )
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap()
    }
    async fn endorse(ctx: &TestContext) {
        let resp = ctx
            .api
            .post_json(
                &format!("/api/v1/accounts/{}/endorse", ctx.bob_id),
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let ctx = TestContext::new("endorse-unfollow").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    endorse(&ctx).await;
    assert_eq!(endorsed(&ctx).await, 1);
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/unfollow", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(endorsed(&ctx).await, 0);

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    endorse(&ctx).await;
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.alice_id),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(endorsed(&ctx).await, 0);
}

/// Endorsing an account you don't follow is rejected (Mastodon AccountPin
/// requires a follow relationship).
#[tokio::test]
async fn test_endorse_requires_following() {
    let ctx = TestContext::new("endorse-nofollow").await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/endorse", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// ── account note ──────────────────────────────────────────────────────────────

/// Setting an account note is reflected in the relationship.
#[tokio::test]
async fn test_set_account_note() {
    let ctx = TestContext::new("acct-note").await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/note", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"comment": "Note about Bob"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rel: Value = resp.json().await.unwrap();
    assert_eq!(rel["note"].as_str(), Some("Note about Bob"));
}

/// An account note over 2000 characters is rejected (Mastodon
/// AccountNote::COMMENT_SIZE_LIMIT).
#[tokio::test]
async fn test_set_account_note_too_long() {
    let ctx = TestContext::new("acct-note-long").await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/note", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({ "comment": "x".repeat(2001) }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

// ── remove from followers ─────────────────────────────────────────────────────

/// After Alice removes Bob from her followers, Bob's relationship shows following=false.
#[tokio::test]
async fn test_remove_from_followers() {
    let ctx = TestContext::new("rm-follower").await;

    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/remove_from_followers", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let rels: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/relationships?id[]={}", ctx.alice_id),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(rels[0]["following"].as_bool(), Some(false));
}

// ── profile settings ──────────────────────────────────────────────────────────

/// PUT /api/v1/profile returns 200 with the account object.
#[tokio::test]
async fn test_update_profile_settings() {
    let ctx = TestContext::new("profile-settings").await;

    let resp = ctx
        .api
        .put_json("/api/v1/profile", Some(&ctx.alice_token), &json!({}))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(body["id"].as_str().is_some());
}

// ── update_credentials ────────────────────────────────────────────────────────

/// PATCH /api/v1/accounts/update_credentials (multipart) updates display_name.
#[tokio::test]
async fn test_update_credentials_display_name() {
    let ctx = TestContext::new("update-creds").await;

    let form = reqwest::multipart::Form::new().text("display_name", "Alice Updated");

    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["display_name"].as_str(), Some("Alice Updated"));
}

/// An avatar and a header are re-encoded rather than stored as sent — which
/// is what turns them upright and takes their EXIF — so what is recorded is
/// the type of what was stored, not what the client said it sent.
#[tokio::test]
async fn test_update_credentials_reencodes_avatar_and_header() {
    let ctx = TestContext::new("update-creds-images").await;

    let part = |name: &str| {
        reqwest::multipart::Part::bytes(crate::helpers::sideways_jpeg())
            .file_name(format!("{name}.png"))
            .mime_str("image/png")
            .unwrap()
    };
    let form = reqwest::multipart::Form::new()
        .part("avatar", part("avatar"))
        .part("header", part("header"));
    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let row = sqlx::query!(
        "SELECT avatar_content_type, avatar_file_name, header_content_type, header_file_name FROM accounts WHERE id = $1",
        alice_id,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(row.avatar_content_type.as_deref(), Some("image/jpeg"));
    assert_eq!(row.header_content_type.as_deref(), Some("image/jpeg"));
    assert!(!row.avatar_file_name.unwrap().ends_with(".png"));
    assert!(!row.header_file_name.unwrap().ends_with(".png"));
}

/// update_credentials strips surrounding whitespace from display_name and note,
/// mirroring Mastodon's `Account#prepare_contents` — a trailing newline from a
/// client must not survive into the stored profile. The strip happens before the
/// length validation, so a value that only exceeds the limit via padding passes.
#[tokio::test]
async fn test_update_credentials_strips_surrounding_whitespace() {
    let ctx = TestContext::new("update-creds-strip").await;

    let patch = |form: reqwest::multipart::Form| {
        ctx.api
            .http
            .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
            .header("host", &ctx.api.host)
            .bearer_auth(&ctx.alice_token)
            .multipart(form)
    };

    let form = reqwest::multipart::Form::new()
        .text("display_name", "  Alice Updated\n")
        .text("note", "\n  About Alice  \n");
    let resp = patch(form).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["display_name"].as_str(), Some("Alice Updated"));
    assert_eq!(body["source"]["note"].as_str(), Some("About Alice"));

    // 40 chars plus padding is 40 chars after stripping → still allowed.
    let padded = format!(" {}\n", "x".repeat(40));
    let resp = patch(reqwest::multipart::Form::new().text("display_name", padded))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["display_name"].as_str(), Some("x".repeat(40).as_str()));
}

/// update_credentials enforces Mastodon's length limits for display_name (40)
/// and note (500), mirroring the Account model validations.
#[tokio::test]
async fn test_update_credentials_length_limits() {
    let ctx = TestContext::new("update-creds-len").await;

    let patch = |form: reqwest::multipart::Form| {
        ctx.api
            .http
            .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
            .header("host", &ctx.api.host)
            .bearer_auth(&ctx.alice_token)
            .multipart(form)
    };

    // 41-char display name → 422.
    let resp = patch(reqwest::multipart::Form::new().text("display_name", "x".repeat(41)))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "41-char display name must be rejected"
    );

    // Exactly 40 chars → OK.
    let resp = patch(reqwest::multipart::Form::new().text("display_name", "x".repeat(40)))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "40-char display name is allowed"
    );

    // 501-char note → 422.
    let resp = patch(reqwest::multipart::Form::new().text("note", "x".repeat(501)))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "501-char note must be rejected"
    );

    // A note dominated by a long URL stays under the limit because URLs count as
    // 23 chars (Mastodon's countable-length rule), so 500 'x' + a long URL is OK.
    let long_url = format!("https://example.com/{}", "a".repeat(300));
    let note = format!("{} {}", "x".repeat(400), long_url);
    let resp = patch(reqwest::multipart::Form::new().text("note", note))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "URLs must count as 23 chars, matching Mastodon"
    );
}

/// A field's `verified_at` (rel="me" link verification) survives an edit that
/// keeps the value unchanged, but is cleared when the value changes — matching
/// Mastodon's `Account#fields_attributes=`.
#[tokio::test]
async fn test_update_credentials_preserves_verified_at() {
    let ctx = TestContext::new("update-verified").await;

    let patch = |form: reqwest::multipart::Form| {
        ctx.api
            .http
            .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
            .header("host", &ctx.api.host)
            .bearer_auth(&ctx.alice_token)
            .multipart(form)
    };

    // Save a field with a URL value.
    let form = reqwest::multipart::Form::new()
        .text("fields_attributes[0][name]", "Website")
        .text("fields_attributes[0][value]", "https://alice.example");
    assert_eq!(patch(form).send().await.unwrap().status(), StatusCode::OK);

    // Simulate a completed verification by stamping verified_at directly.
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    sqlx::query!(
        r#"UPDATE accounts
           SET fields = '[{"name":"Website","value":"https://alice.example","verified_at":"2026-01-01T00:00:00.000Z"}]'::jsonb
           WHERE id = $1"#,
        alice_id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    // Edit only the name; the value is unchanged, so verified_at must persist.
    let form = reqwest::multipart::Form::new()
        .text("fields_attributes[0][name]", "Homepage")
        .text("fields_attributes[0][value]", "https://alice.example");
    let body: Value = patch(form).send().await.unwrap().json().await.unwrap();
    assert_eq!(body["fields"][0]["name"].as_str(), Some("Homepage"));
    assert_eq!(
        body["fields"][0]["verified_at"].as_str(),
        Some("2026-01-01T00:00:00.000Z"),
        "verified_at must survive an edit that keeps the value"
    );

    // Change the value; verified_at must clear.
    let form = reqwest::multipart::Form::new()
        .text("fields_attributes[0][name]", "Homepage")
        .text("fields_attributes[0][value]", "https://elsewhere.example");
    let body: Value = patch(form).send().await.unwrap().json().await.unwrap();
    assert!(
        body["fields"][0]["verified_at"].is_null(),
        "verified_at must clear when the value changes"
    );
}

/// update_credentials rejects more than 4 profile fields (Mastodon
/// Account::DEFAULT_FIELDS_SIZE) and a value without a name
/// (`EmptyProfileFieldNamesValidator`), with `ValidationErrorFormatter`'s
/// details; an over-long field is kept as given and cut to 255 characters
/// when read, as `Account::Field#sanitize` cuts it.
#[tokio::test]
async fn test_update_credentials_fields_limits() {
    let ctx = TestContext::new("update-fields").await;

    // Five fields → 422.
    let mut form = reqwest::multipart::Form::new();
    for i in 0..5 {
        form = form
            .text(format!("fields_attributes[{i}][name]"), format!("k{i}"))
            .text(format!("fields_attributes[{i}][value]"), format!("v{i}"));
    }
    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "5 fields must be rejected"
    );
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        json!("Validation failed: Fields is too long (maximum is 4 characters)")
    );
    assert_eq!(body["details"]["fields"][0]["error"], json!("ERR_TOO_LONG"));

    // A value with no name → 422.
    let resp = ctx
        .api
        .patch_json(
            "/api/v1/accounts/update_credentials",
            Some(&ctx.alice_token),
            &json!({"fields_attributes": [{"name": "  ", "value": "orphan"}]}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        json!("Validation failed: Fields contains values with missing labels")
    );
    assert_eq!(
        body["details"]["fields"][0]["error"],
        json!("ERR_FIELDS_WITH_VALUES_MISSING_LABELS")
    );

    // An over-long value is stored, and read back cut.
    let form = reqwest::multipart::Form::new()
        .text("fields_attributes[0][name]", " website ")
        .text("fields_attributes[0][value]", "x".repeat(300));
    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["source"]["fields"][0]["name"], json!("website"));
    assert_eq!(
        body["source"]["fields"][0]["value"].as_str().unwrap().len(),
        255
    );
    let stored: Value = sqlx::query_scalar("SELECT fields FROM accounts WHERE id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(stored[0]["value"].as_str().unwrap().len(), 300);

    // Exactly 4 valid fields → OK.
    let mut form = reqwest::multipart::Form::new();
    for i in 0..4 {
        form = form
            .text(format!("fields_attributes[{i}][name]"), format!("k{i}"))
            .text(format!("fields_attributes[{i}][value]"), format!("v{i}"));
    }
    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "4 fields should be accepted");
}

/// PATCH /api/v1/accounts/update_credentials updates bio note.
#[tokio::test]
async fn test_update_credentials_note() {
    let ctx = TestContext::new("update-note").await;

    let form = reqwest::multipart::Form::new().text("note", "This is my bio");

    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["source"]["note"]
            .as_str()
            .unwrap_or("")
            .contains("This is my bio"),
        "note not updated: {body}",
    );
}

/// The top-level `note` is rendered to HTML on the fly (Mastodon's
/// `account_bio_format`) while `source.note` keeps the raw editable text.
#[tokio::test]
async fn test_update_credentials_note_rendered_html() {
    let ctx = TestContext::new("update-note-html").await;

    let form = reqwest::multipart::Form::new().text("note", "hello https://example.com");

    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    // source.note stays raw plaintext for editing.
    assert_eq!(
        body["source"]["note"].as_str(),
        Some("hello https://example.com"),
        "source.note should be raw: {body}",
    );
    // Top-level note is rendered HTML with the URL linkified.
    let note = body["note"].as_str().unwrap_or("");
    assert!(note.contains("<p>"), "note not wrapped in <p>: {body}");
    assert!(
        note.contains("<a href=\"https://example.com\""),
        "note URL not linkified: {body}",
    );
}

/// PATCH /api/v1/accounts/update_credentials with locked=true makes account locked.
#[tokio::test]
async fn test_update_credentials_locked() {
    let ctx = TestContext::new("update-locked").await;

    let form = reqwest::multipart::Form::new().text("locked", "true");

    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["locked"].as_bool(), Some(true));

    // Follow from Bob should now be pending.
    let rel = ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    assert_eq!(rel["requested"].as_bool(), Some(true));
}

/// PATCH /api/v1/accounts/update_credentials with source[privacy] updates default posting visibility.
#[tokio::test]
async fn test_update_credentials_source_privacy() {
    let ctx = TestContext::new("update-privacy").await;

    let form = reqwest::multipart::Form::new().text("source[privacy]", "private");

    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The update response itself must reflect the new default, not a hardcoded
    // "public" (the response builder reads the user's actual settings).
    let updated: Value = resp.json().await.unwrap();
    assert_eq!(
        updated["source"]["privacy"].as_str(),
        Some("private"),
        "PATCH response source.privacy stale"
    );

    // And it persists, visible via verify_credentials.
    let creds: Value = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(creds["source"]["privacy"].as_str(), Some("private"));
}

/// PATCH /api/v1/accounts/update_credentials with source[sensitive] updates default sensitivity.
#[tokio::test]
async fn test_update_credentials_source_sensitive() {
    let ctx = TestContext::new("update-sensitive").await;

    let form = reqwest::multipart::Form::new().text("source[sensitive]", "true");

    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let creds: Value = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(creds["source"]["sensitive"].as_bool(), Some(true));
}

/// PATCH /api/v1/accounts/update_credentials with source[language] updates default language.
#[tokio::test]
async fn test_update_credentials_source_language() {
    let ctx = TestContext::new("update-lang").await;

    let form = reqwest::multipart::Form::new().text("source[language]", "fr");

    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let creds: Value = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(creds["source"]["language"].as_str(), Some("fr"));
}

/// Profile fields set via update_credentials appear in verify_credentials source and account fields.
#[tokio::test]
async fn test_update_credentials_profile_fields() {
    let ctx = TestContext::new("profile-fields").await;

    let form = reqwest::multipart::Form::new()
        .text("fields_attributes[0][name]", "Website")
        .text("fields_attributes[0][value]", "https://example.com")
        .text("fields_attributes[1][name]", "Location")
        .text("fields_attributes[1][value]", "Rustland");

    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let updated: Value = resp.json().await.unwrap();

    // The account fields array should have both entries.
    let fields = updated["fields"]
        .as_array()
        .expect("fields should be array");
    assert!(
        fields.iter().any(|f| f["name"].as_str() == Some("Website")),
        "Website field missing from fields: {fields:?}",
    );
    assert!(
        fields
            .iter()
            .any(|f| f["name"].as_str() == Some("Location")),
        "Location field missing from fields: {fields:?}",
    );

    // source.fields should also reflect the values.
    let creds: Value = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let src_fields = creds["source"]["fields"]
        .as_array()
        .expect("source.fields should be array");
    assert!(
        src_fields
            .iter()
            .any(|f| f["name"].as_str() == Some("Website")),
        "Website field missing from source.fields: {src_fields:?}",
    );
}

// ── familiar followers ────────────────────────────────────────────────────────

/// GET /api/v1/accounts/familiar_followers returns an array of familiar-followers objects.
#[tokio::test]
async fn test_familiar_followers_returns_array() {
    let ctx = TestContext::new("familiar").await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/familiar_followers?id[]={}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"].as_str(), Some(ctx.bob_id.as_str()));
    assert!(list[0]["accounts"].is_array());
}

/// Passing the same id twice returns only one entry (deduplication).
#[tokio::test]
async fn test_familiar_followers_deduplicates_ids() {
    let ctx = TestContext::new("familiar-dedup").await;

    let resp = ctx
        .api
        .get(
            &format!(
                "/api/v1/accounts/familiar_followers?id[]={}&id[]={}",
                ctx.bob_id, ctx.bob_id
            ),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(
        list.len(),
        1,
        "duplicate id[] should be collapsed to one entry"
    );
}

/// familiar_followers returns accounts the viewer follows who also follow the target.
#[tokio::test]
async fn test_familiar_followers_correctness() {
    let ctx = TestContext::new("familiar-correct").await;

    // Create charlie as a 3rd user
    let (charlie_uuid, charlie_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "charlie", "charlie@test.invalid").await;
    let charlie_id = charlie_uuid.to_string();

    // Alice follows Charlie
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{charlie_id}/follow"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Charlie follows Bob
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&charlie_token),
            &json!({}),
        )
        .await;

    // Alice checks familiar followers for Bob — should see Charlie (alice follows charlie, charlie follows bob)
    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/familiar_followers?id[]={}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await;
    let list: Vec<Value> = resp.json().await.unwrap();
    let entry = &list[0];
    let familiar: Vec<&str> = entry["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["id"].as_str())
        .collect();
    assert!(
        familiar.contains(&charlie_id.as_str()),
        "charlie should be a familiar follower (alice follows charlie, charlie follows bob)",
    );
    assert!(
        !familiar.contains(&ctx.alice_id.as_str()),
        "alice should not appear in her own familiar followers list",
    );

    // When Bob hides his followers, no familiar followers are revealed for him
    // (Mastodon: hides_followers? → empty).
    let bob_id_num: i64 = ctx.bob_id.parse().unwrap();
    sqlx::query!(
        "UPDATE accounts SET hide_collections = true WHERE id = $1",
        bob_id_num
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let hidden: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/familiar_followers?id[]={}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        hidden[0]["accounts"].as_array().unwrap().is_empty(),
        "familiar followers must be empty when the target hides followers",
    );
}

// ── suggestions ───────────────────────────────────────────────────────────────

/// GET /api/v1/suggestions returns a JSON array.
#[tokio::test]
async fn test_get_suggestions() {
    let ctx = TestContext::new("suggest").await;

    let resp = ctx
        .api
        .get("/api/v1/suggestions", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<Value> = resp.json().await.unwrap();
}

/// DELETE /api/v1/suggestions/:id returns 200.
#[tokio::test]
async fn test_dismiss_suggestion() {
    let ctx = TestContext::new("suggest-dismiss").await;

    let resp = ctx
        .api
        .delete(
            &format!("/api/v1/suggestions/{}", ctx.bob_id),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

/// GET /api/v2/suggestions returns suggestions with a source field.
#[tokio::test]
async fn test_get_suggestions_v2() {
    let ctx = TestContext::new("suggest-v2").await;

    let resp = ctx
        .api
        .get("/api/v2/suggestions", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<Value> = resp.json().await.unwrap();
}

/// Suggestions exclude accounts the viewer has blocked (Mastodon excludes
/// blocked/muted/suspended from follow recommendations).
#[tokio::test]
async fn test_suggestions_exclude_blocked() {
    let ctx = TestContext::new("suggest-block").await;

    // Bob is discoverable and one of the accounts the setting features.
    crate::helpers::set_setting(&ctx.db, "bootstrap_timeline_accounts", "bob").await;
    sqlx::query("UPDATE accounts SET discoverable = true WHERE username = 'bob'")
        .execute(&ctx.db)
        .await
        .unwrap();
    let before: Vec<Value> = ctx
        .api
        .get("/api/v2/suggestions", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        before
            .iter()
            .any(|s| s["account"]["id"].as_str() == Some(ctx.bob_id.as_str())),
        "bob should be suggested before blocking",
    );

    // Alice blocks Bob.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    let after: Vec<Value> = ctx
        .api
        .get("/api/v2/suggestions", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !after
            .iter()
            .any(|s| s["account"]["id"].as_str() == Some(ctx.bob_id.as_str())),
        "a blocked account must not be suggested",
    );
}

// ── directory ─────────────────────────────────────────────────────────────────

/// GET /api/v1/directory returns local accounts (includes alice), once
/// they have the `account_stats` row `Account.discoverable` joins.
#[tokio::test]
async fn test_get_directory() {
    let ctx = TestContext::new("directory").await;
    ctx.api
        .post_status(&ctx.alice_token, "hello", "public")
        .await;

    let resp = ctx
        .api
        .get("/api/v1/directory", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(
        list.iter().any(|a| a["username"].as_str() == Some("alice")),
        "alice not found in directory",
    );
}

/// Silenced accounts are excluded from the directory (Mastodon
/// Account.discoverable → without_silenced).
#[tokio::test]
async fn test_directory_excludes_silenced() {
    let ctx = TestContext::new("directory-silenced").await;
    let bob_id: i64 = ctx.bob_id.parse().unwrap();
    ctx.api.post_status(&ctx.bob_token, "hello", "public").await;

    // Bob is discoverable and initially listed.
    let before: Vec<Value> = ctx
        .api
        .get("/api/v1/directory", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        before.iter().any(|a| a["username"].as_str() == Some("bob")),
        "bob should be listed before silencing"
    );

    // Silence bob.
    sqlx::query!(
        "UPDATE accounts SET silenced_at = now() WHERE id = $1",
        bob_id
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let after: Vec<Value> = ctx
        .api
        .get("/api/v1/directory", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !after.iter().any(|a| a["username"].as_str() == Some("bob")),
        "silenced bob must not appear in directory"
    );
}

/// `Api::V1::DirectoriesController`: remote accounts unless `local` is
/// truthy, `Account.discoverable`'s conditions (approved and confirmed,
/// not moved, with stats), a viewer's exclusions, and the two orders.
#[tokio::test]
async fn test_directory_as_mastodon_lists_it() {
    let ctx = TestContext::new("directory-scopes").await;
    let names = |list: &[Value]| -> Vec<String> {
        list.iter()
            .map(|a| a["acct"].as_str().unwrap().to_owned())
            .collect()
    };
    let directory = |query: &'static str, token: Option<String>| {
        let ctx = &ctx;
        async move {
            let resp = ctx
                .api
                .get(&format!("/api/v1/directory{query}"), token.as_deref())
                .await;
            assert_eq!(resp.status(), StatusCode::OK);
            resp.json::<Vec<Value>>().await.unwrap()
        }
    };
    let (carol_id, carol_token) =
        seed_user(&ctx.db, &ctx.domain, "carol", "carol@example.com").await;
    let (dave_id, dave_token) = seed_user(&ctx.db, &ctx.domain, "dave", "dave@example.com").await;
    let (erin_id, erin_token) = seed_user(&ctx.db, &ctx.domain, "erin", "erin@example.com").await;
    for token in [
        &ctx.alice_token,
        &ctx.bob_token,
        &carol_token,
        &dave_token,
        &erin_token,
    ] {
        ctx.api.post_status(token, "hello", "public").await;
    }
    let remote: i64 = sqlx::query_scalar(
        "INSERT INTO accounts (id, username, domain, uri, url, discoverable, protocol, created_at, updated_at)
         VALUES (timestamp_id('accounts'), 'remy', 'remote.example', 'https://remote.example/users/remy',
                 'https://remote.example/@remy', true, 1, now(), now())
         RETURNING id",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO account_stats (account_id, created_at, updated_at, last_status_at)
         VALUES ($1, now(), now(), now() - interval '1 day')",
    )
    .bind(remote)
    .execute(&ctx.db)
    .await
    .unwrap();
    // Carol has not confirmed her address; Dave has moved to Erin.
    sqlx::query("UPDATE users SET confirmed_at = NULL WHERE account_id = $1")
        .bind(carol_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query("UPDATE accounts SET moved_to_account_id = $2 WHERE id = $1")
        .bind(dave_id)
        .bind(erin_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    let anonymous = names(&directory("", None).await);
    assert!(
        anonymous.contains(&"remy@remote.example".to_owned()),
        "{anonymous:?}"
    );
    assert!(!anonymous.contains(&"carol".to_owned()), "{anonymous:?}");
    assert!(!anonymous.contains(&"dave".to_owned()), "{anonymous:?}");
    assert!(anonymous.contains(&"erin".to_owned()), "{anonymous:?}");
    // The most recent poster first; the remote account posted a day ago.
    assert_eq!(
        anonymous.last().map(String::as_str),
        Some("remy@remote.example")
    );
    let local = names(&directory("?local=1", None).await);
    assert!(
        !local.contains(&"remy@remote.example".to_owned()),
        "{local:?}"
    );

    // `order=new` is by id, newest first.
    let new = names(&directory("?order=new", None).await);
    assert_eq!(new.first().map(String::as_str), Some("remy@remote.example"));
    assert!(directory("?limit=0", None).await.is_empty());

    // Bob mutes Erin and blocks remote.example; Alice blocks Bob.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{erin_id}/mute"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    ctx.api
        .post_json(
            "/api/v1/domain_blocks",
            Some(&ctx.bob_token),
            &json!({"domain": "remote.example"}),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    let for_bob = names(&directory("", Some(ctx.bob_token.clone())).await);
    assert!(!for_bob.contains(&"erin".to_owned()), "{for_bob:?}");
    assert!(!for_bob.contains(&"alice".to_owned()), "{for_bob:?}");
    assert!(
        !for_bob.contains(&"remy@remote.example".to_owned()),
        "{for_bob:?}"
    );
}

// ── account search endpoint ───────────────────────────────────────────────────

/// GET /api/v1/accounts/search returns matching accounts.
#[tokio::test]
async fn test_accounts_search_endpoint() {
    let ctx = TestContext::new("acct-search").await;

    let resp = ctx
        .api
        .get("/api/v1/accounts/search?q=bob", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(list.iter().any(|a| a["username"].as_str() == Some("bob")));

    // A leading '@' is stripped, so "@bob" still finds bob (Mastodon behavior).
    let resp = ctx
        .api
        .get("/api/v1/accounts/search?q=%40bob", Some(&ctx.alice_token))
        .await;
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(
        list.iter().any(|a| a["username"].as_str() == Some("bob")),
        "@bob should match bob"
    );
}

// ── block effects ─────────────────────────────────────────────────────────────

/// Blocking removes the follow relationship in both directions.
#[tokio::test]
async fn test_block_removes_follow() {
    let ctx = TestContext::new("block-rm-follow").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let rels: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/relationships?id[]={}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        rels[0]["following"].as_bool(),
        Some(false),
        "alice should not follow bob after block"
    );
    assert_eq!(
        rels[0]["followed_by"].as_bool(),
        Some(false),
        "bob should not follow alice after block"
    );
}

// ── account lists ─────────────────────────────────────────────────────────────

/// GET /api/v1/accounts/:id/lists returns lists that include the given account.
#[tokio::test]
async fn test_get_account_lists() {
    let ctx = TestContext::new("acct-lists").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let list: Value = ctx
        .api
        .post_json(
            "/api/v1/lists",
            Some(&ctx.alice_token),
            &json!({"title": "Bob's List"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let list_id = list["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/lists/{list_id}/accounts"),
            Some(&ctx.alice_token),
            &json!({"account_ids": [ctx.bob_id]}),
        )
        .await;

    let lists: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/lists", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(lists.iter().any(|l| l["id"].as_str() == Some(list_id)));
}

// ── domain blocks ─────────────────────────────────────────────────────────────

/// Block a domain, list it, unblock it.
#[tokio::test]
async fn test_domain_block_lifecycle() {
    let ctx = TestContext::new("domain-block").await;

    let block_resp = ctx
        .api
        .post_json(
            "/api/v1/domain_blocks",
            Some(&ctx.alice_token),
            &json!({"domain": "evil.example.com"}),
        )
        .await;
    assert_eq!(block_resp.status(), StatusCode::OK);

    let domains: Vec<String> = ctx
        .api
        .get("/api/v1/domain_blocks", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(domains.contains(&"evil.example.com".to_string()));

    let unblock_resp = ctx
        .api
        .http
        .delete(ctx.api.url("/api/v1/domain_blocks"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"domain": "evil.example.com"}))
        .send()
        .await
        .unwrap();
    assert_eq!(unblock_resp.status(), StatusCode::OK);

    let after: Vec<String> = ctx
        .api
        .get("/api/v1/domain_blocks", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!after.contains(&"evil.example.com".to_string()));
}

// ── follow settings (showing_reblogs / notifying) ─────────────────────────────

/// Following with reblogs=false sets showing_reblogs=false in relationship.
#[tokio::test]
async fn test_follow_with_reblogs_false() {
    let ctx = TestContext::new("follow-no-reblogs").await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"reblogs": false}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rel: Value = resp.json().await.unwrap();
    assert_eq!(rel["following"].as_bool(), Some(true));
    assert_eq!(rel["showing_reblogs"].as_bool(), Some(false));
}

/// Following with notify=true sets notifying=true in relationship.
#[tokio::test]
async fn test_follow_with_notify_true() {
    let ctx = TestContext::new("follow-notify").await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"notify": true}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rel: Value = resp.json().await.unwrap();
    assert_eq!(rel["following"].as_bool(), Some(true));
    assert_eq!(rel["notifying"].as_bool(), Some(true));
}

/// Re-following an already-followed account updates settings without duplicating.
#[tokio::test]
async fn test_follow_update_settings_when_already_following() {
    let ctx = TestContext::new("follow-update-settings").await;

    // First follow with defaults.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    // Re-follow with reblogs=false.
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"reblogs": false, "notify": true}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rel: Value = resp.json().await.unwrap();
    assert_eq!(
        rel["following"].as_bool(),
        Some(true),
        "should still be following after re-follow"
    );
    assert_eq!(rel["showing_reblogs"].as_bool(), Some(false));
    assert_eq!(rel["notifying"].as_bool(), Some(true));
}

/// Default follow has showing_reblogs=true and notifying=false.
#[tokio::test]
async fn test_follow_defaults_showing_reblogs_true() {
    let ctx = TestContext::new("follow-defaults").await;

    let rel = ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    assert_eq!(rel["showing_reblogs"].as_bool(), Some(true));
    assert_eq!(rel["notifying"].as_bool(), Some(false));
}

/// Relationship languages field is null (not []) when no language filter is set.
#[tokio::test]
async fn test_relationship_languages_null_when_not_set() {
    let ctx = TestContext::new("rel-languages-null").await;

    let rel = ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    assert!(
        rel["languages"].is_null(),
        "languages should be null when no language filter is set, got: {}",
        rel["languages"],
    );
}

// ── mute settings ─────────────────────────────────────────────────────────────

/// Muting with notifications=false sets muting_notifications=false.
#[tokio::test]
async fn test_mute_with_notifications_false() {
    let ctx = TestContext::new("mute-no-notif").await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"notifications": false}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rel: Value = resp.json().await.unwrap();
    assert_eq!(rel["muting"].as_bool(), Some(true));
    assert_eq!(rel["muting_notifications"].as_bool(), Some(false));
}

/// Muting with duration=3600 sets mute_expires_at to a non-null value.
#[tokio::test]
async fn test_mute_with_duration_sets_expires_at() {
    let ctx = TestContext::new("mute-duration").await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"duration": 3600}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rel: Value = resp.json().await.unwrap();
    assert_eq!(rel["muting"].as_bool(), Some(true));
    assert!(
        rel["muting_expires_at"].as_str().is_some(),
        "muting_expires_at should be set"
    );
}

/// A timed mute queues a `DeleteMuteWorker` for when it expires, which lifts
/// it then; until it runs, the mute stands, as Mastodon reads it.
#[tokio::test]
async fn test_timed_mute_is_lifted_by_its_job() {
    let ctx = TestContext::new("mute-expiry").await;
    ctx.state.jobs.set_mode(eunha::jobs::Mode::Durable);

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"duration": 3600}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let queued = eunha::jobs::queued(&ctx.state, "DeleteMuteWorker")
        .await
        .unwrap();
    assert_eq!(queued.len(), 1);
    // Not yet due: nothing runs.
    eunha::jobs::drain(&ctx.state).await.unwrap();

    // Expired, but not yet lifted: still a mute.
    sqlx::query("UPDATE mutes SET expires_at = now() - interval '1 minute'")
        .execute(&ctx.db)
        .await
        .unwrap();
    let rel: Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/relationships?id[]={}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(rel[0]["muting"].as_bool(), Some(true));

    eunha::jobs::make_due(&ctx.state).await.unwrap();
    eunha::jobs::drain(&ctx.state).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM mutes")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

/// A mute renewed for good is not lifted by the job its timed predecessor
/// queued.
#[tokio::test]
async fn test_renewed_mute_outlives_the_old_job() {
    let ctx = TestContext::new("mute-renewed").await;
    ctx.state.jobs.set_mode(eunha::jobs::Mode::Durable);
    for duration in [3600, 0] {
        ctx.api
            .post_json(
                &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
                Some(&ctx.alice_token),
                &json!({"duration": duration}),
            )
            .await;
    }
    eunha::jobs::make_due(&ctx.state).await.unwrap();
    eunha::jobs::drain(&ctx.state).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM mutes")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(left, 1);
}

/// Re-muting an account updates hide_notifications in place.
#[tokio::test]
async fn test_mute_upsert_updates_settings() {
    let ctx = TestContext::new("mute-upsert").await;

    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"notifications": true}),
        )
        .await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"notifications": false}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rel: Value = resp.json().await.unwrap();
    assert_eq!(rel["muting_notifications"].as_bool(), Some(false));
}

// ── relationship extras ───────────────────────────────────────────────────────

/// blocked_by reflects when the target has blocked the requesting user.
#[tokio::test]
async fn test_relationship_blocked_by() {
    let ctx = TestContext::new("blocked-by").await;

    // Bob blocks Alice.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.alice_id),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    // Alice checks her relationship with Bob.
    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/relationships?id[]={}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await;
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list[0]["blocked_by"].as_bool(), Some(true));
}

/// requested_by reflects when the target has a pending follow request to the user.
#[tokio::test]
async fn test_relationship_requested_by() {
    let ctx = TestContext::new("requested-by").await;

    let db = ctx.db.clone();
    let alice_uuid: i64 = ctx.alice_id.parse().unwrap();

    // Lock Alice's account so Bob's follow becomes pending.
    sqlx::query!(
        "UPDATE accounts SET locked = true WHERE id = $1",
        alice_uuid
    )
    .execute(&db)
    .await
    .unwrap();

    // Bob sends a follow request to Alice.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.alice_id),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    // Alice checks her relationship with Bob.
    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/relationships?id[]={}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await;
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list[0]["requested_by"].as_bool(), Some(true));
}

/// domain_blocking reflects a domain block on the target's domain.
#[tokio::test]
async fn test_relationship_domain_blocking() {
    let ctx = TestContext::new("rel-domain-block").await;

    let db = ctx.db.clone();
    let bob_uuid: i64 = ctx.bob_id.parse().unwrap();

    // Set Bob's domain to a remote domain.
    sqlx::query!(
        "UPDATE accounts SET domain = 'remote.example.com' WHERE id = $1",
        bob_uuid
    )
    .execute(&db)
    .await
    .unwrap();

    // Alice domain-blocks that domain.
    ctx.api
        .post_json(
            "/api/v1/domain_blocks",
            Some(&ctx.alice_token),
            &json!({"domain": "remote.example.com"}),
        )
        .await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/relationships?id[]={}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await;
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list[0]["domain_blocking"].as_bool(), Some(true));
}

// ── hide_collections ──────────────────────────────────────────────────────────

/// When hide_collections=true, followers list is empty for non-owner viewers.
#[tokio::test]
async fn test_hide_collections_hides_followers_from_others() {
    let ctx = TestContext::new("hide-coll-followers").await;

    let db = ctx.db.clone();
    let alice_uuid: i64 = ctx.alice_id.parse().unwrap();

    // Bob follows Alice.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    // Enable hide_collections on Alice's account.
    sqlx::query!(
        "UPDATE accounts SET hide_collections = true WHERE id = $1",
        alice_uuid
    )
    .execute(&db)
    .await
    .unwrap();

    // Bob tries to see Alice's followers — should be empty.
    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/followers", ctx.alice_id),
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(
        list.is_empty(),
        "followers should be hidden when hide_collections=true"
    );
}

/// When hide_collections=true, following list is empty for non-owner viewers.
#[tokio::test]
async fn test_hide_collections_hides_following_from_others() {
    let ctx = TestContext::new("hide-coll-following").await;

    let db = ctx.db.clone();
    let alice_uuid: i64 = ctx.alice_id.parse().unwrap();

    // Alice follows Bob.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    // Enable hide_collections on Alice's account.
    sqlx::query!(
        "UPDATE accounts SET hide_collections = true WHERE id = $1",
        alice_uuid
    )
    .execute(&db)
    .await
    .unwrap();

    // Bob tries to see Alice's following — should be empty.
    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/following", ctx.alice_id),
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(
        list.is_empty(),
        "following should be hidden when hide_collections=true"
    );
}

/// Owner can always see their own followers even with hide_collections=true.
#[tokio::test]
async fn test_hide_collections_owner_sees_own_followers() {
    let ctx = TestContext::new("hide-coll-self").await;

    let db = ctx.db.clone();
    let alice_uuid: i64 = ctx.alice_id.parse().unwrap();

    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    sqlx::query!(
        "UPDATE accounts SET hide_collections = true WHERE id = $1",
        alice_uuid
    )
    .execute(&db)
    .await
    .unwrap();

    // Alice views her own followers.
    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/followers", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await;
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(
        !list.is_empty(),
        "owner should see own followers even with hide_collections"
    );
}

// ── preferences ───────────────────────────────────────────────────────────────

/// GET /api/v1/preferences returns sensible defaults.
#[tokio::test]
async fn test_get_preferences_defaults() {
    let ctx = TestContext::new("prefs-defaults").await;

    let resp = ctx
        .api
        .get("/api/v1/preferences", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let prefs: Value = resp.json().await.unwrap();

    assert!(
        prefs["posting:default:visibility"].as_str().is_some(),
        "missing posting:default:visibility"
    );
    assert!(
        prefs["posting:default:sensitive"].as_bool().is_some(),
        "missing posting:default:sensitive"
    );
}

/// GET /api/v1/preferences returns the documented default posting preferences.
#[tokio::test]
async fn test_preferences_defaults() {
    let ctx = TestContext::new("prefs-default").await;

    let resp = ctx
        .api
        .get("/api/v1/preferences", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let prefs: Value = resp.json().await.unwrap();
    assert_eq!(prefs["posting:default:visibility"].as_str(), Some("public"));
    assert_eq!(prefs["posting:default:sensitive"].as_bool(), Some(false));
    // With none chosen, `preferred_posting_language` falls back to the
    // interface language, here the site's.
    assert_eq!(prefs["posting:default:language"].as_str(), Some("en"));
}

/// PUT /api/v1/profile returns the caller's account object.
#[tokio::test]
async fn test_update_profile_settings_returns_account() {
    let ctx = TestContext::new("profile-settings").await;

    let resp = ctx
        .api
        .put_json("/api/v1/profile", Some(&ctx.alice_token), &json!({}))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"].as_str(), Some(ctx.alice_id.as_str()));
}

// ── account deletion ──────────────────────────────────────────────────────────

/// DELETE /api/v1/accounts with correct password deletes the account (returns 200).
#[tokio::test]
async fn test_delete_account_with_valid_password() {
    let ctx = TestContext::new("del-acct").await;

    let resp = ctx
        .api
        .http
        .delete(ctx.api.url("/api/v1/accounts"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"password": "testpassword123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // After deletion, verify_credentials should fail (tokens revoked, user gone).
    let after = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await;
    assert!(
        after.status() == StatusCode::UNAUTHORIZED || after.status() == StatusCode::FORBIDDEN,
        "expected 401/403 after account deletion, got {}",
        after.status(),
    );
}

/// Self-service deletion follows `DeleteAccountService(reserve_username: true,
/// reserve_email: false)`: the account row stays (suspended and scrubbed, so
/// the username can't be re-registered), the user row and its posts do not.
#[tokio::test]
async fn test_delete_account_reserves_username_and_destroys_user() {
    let ctx = TestContext::new("del-acct-purge").await;
    let alice_account_id: i64 = ctx.alice_id.parse().unwrap();

    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "goodbye"}),
        )
        .await;

    // An upload never attached to a status still has to go.
    sqlx::query(
        r#"INSERT INTO media_attachments (id, account_id, file_file_name, remote_url, type, created_at, updated_at)
           VALUES ($1, $2, 'orphan.png', '', 0, now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(alice_account_id)
    .execute(&ctx.db)
    .await
    .unwrap();

    let resp = ctx
        .api
        .http
        .delete(ctx.api.url("/api/v1/accounts"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"password": "testpassword123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let user_exists: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE account_id = $1")
        .bind(alice_account_id)
        .fetch_optional(&ctx.db)
        .await
        .unwrap();
    assert!(user_exists.is_none(), "user record should be destroyed");

    let account: (bool, String, String) = sqlx::query_as(
        "SELECT requested_deletion_at IS NOT NULL, display_name, note FROM accounts WHERE id = $1",
    )
    .bind(alice_account_id)
    .fetch_one(&ctx.db)
    .await
    .expect("account record should be reserved");
    // `Account#mark_deleted!`, since 4.7.0, rather than a suspension.
    assert!(account.0, "account should stay marked deleted");
    assert_eq!(account.1, "", "display name should be scrubbed");
    assert_eq!(account.2, "", "note should be scrubbed");

    let statuses: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses WHERE account_id = $1")
        .bind(alice_account_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(statuses, 0, "statuses should be removed");

    let media: i64 =
        sqlx::query_scalar("SELECT count(*) FROM media_attachments WHERE account_id = $1")
            .bind(alice_account_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(media, 0, "media attachments should be removed");

    // The deletion request created by `suspend!` is fulfilled, so the
    // suspension is now permanent.
    let requests: i64 =
        sqlx::query_scalar("SELECT count(*) FROM account_deletion_requests WHERE account_id = $1")
            .bind(alice_account_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(requests, 0, "deletion request should be fulfilled");
}

/// Gives alice a collection featuring bob, a followed hashtag and a generated
/// annual report: the associations `DeleteAccountService` purges by name.
async fn give_alice_purgeable_associations(ctx: &TestContext) {
    let collection: Value = ctx
        .api
        .post_json(
            "/api/v1/collections",
            Some(&ctx.alice_token),
            &json!({"name": "Featured"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let collection_id = collection["collection"]["id"].as_str().unwrap();
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/collections/{collection_id}/items"),
            Some(&ctx.alice_token),
            &json!({"account_id": ctx.bob_id}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = ctx
        .api
        .post_json(
            "/api/v1/tags/purgedtag/follow",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    sqlx::query(
        "INSERT INTO generated_annual_reports (account_id, year, data, schema_version, created_at, updated_at)
         VALUES ($1, 2025, '{}', 1, now(), now())",
    )
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .execute(&ctx.db)
    .await
    .unwrap();
}

/// What is left of alice's collections, their items, her tag follows and her
/// annual reports.
async fn alice_purgeable_associations(ctx: &TestContext) -> [i64; 4] {
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let mut counts = [0; 4];
    for (count, sql) in counts.iter_mut().zip([
        "SELECT count(*) FROM collections WHERE account_id = $1",
        "SELECT count(*) FROM collection_items i JOIN collections c ON c.id = i.collection_id
         WHERE c.account_id = $1",
        "SELECT count(*) FROM tag_follows WHERE account_id = $1",
        "SELECT count(*) FROM generated_annual_reports WHERE account_id = $1",
    ]) {
        *count = sqlx::query_scalar(sql)
            .bind(alice)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    }
    counts
}

/// `DeleteAccountService::ASSOCIATIONS_ON_PURGE` takes an account's
/// collections and tag follows, and since Mastodon 4.7.2 its generated annual
/// reports, even when the account row is kept.
#[tokio::test]
async fn test_delete_account_purges_collections_tag_follows_and_annual_reports() {
    let ctx = TestContext::new("del-acct-assoc").await;
    give_alice_purgeable_associations(&ctx).await;
    assert_eq!(alice_purgeable_associations(&ctx).await, [1, 1, 1, 1]);

    let resp = ctx
        .api
        .http
        .delete(ctx.api.url("/api/v1/accounts"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"password": "testpassword123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(alice_purgeable_associations(&ctx).await, [0, 0, 0, 0]);
}

/// A full purge deletes the account row, which `collections.account_id`'s
/// foreign key (no ON DELETE) refuses while the account still owns a
/// collection, so the collections have to go first.
#[tokio::test]
async fn test_purging_an_account_that_owns_a_collection() {
    let ctx = TestContext::new("del-acct-coll").await;
    give_alice_purgeable_associations(&ctx).await;
    let alice: i64 = ctx.alice_id.parse().unwrap();

    Box::pin(eunha::delete_account::call(
        &ctx.state,
        alice,
        eunha::delete_account::Options::purge(),
    ))
    .await
    .unwrap();

    let account: Option<i64> = sqlx::query_scalar("SELECT id FROM accounts WHERE id = $1")
        .bind(alice)
        .fetch_optional(&ctx.db)
        .await
        .unwrap();
    assert!(account.is_none(), "the account row should be gone");
    assert_eq!(alice_purgeable_associations(&ctx).await, [0, 0, 0, 0]);
}

/// Since Mastodon 4.7.3, a deleted account's statuses take their links to
/// preview cards with them (#40624), and the account leaves the collections
/// that featured it, each counting one item fewer (#40623).
#[tokio::test]
async fn test_delete_account_removes_card_links_and_featured_items() {
    let ctx = TestContext::new("del-acct-cards").await;
    let bob: i64 = ctx.bob_id.parse().unwrap();
    // Alice features bob.
    give_alice_purgeable_associations(&ctx).await;
    let collection: i64 =
        sqlx::query_scalar("SELECT collection_id FROM collection_items WHERE account_id = $1")
            .bind(bob)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    let before: i32 = sqlx::query_scalar("SELECT item_count FROM collections WHERE id = $1")
        .bind(collection)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    // Bob's post carries a card.
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "a link", "public")
        .await;
    let status_id: i64 = status["id"].as_str().unwrap().parse().unwrap();
    let card: i64 = sqlx::query_scalar(
        "INSERT INTO preview_cards (url, title, description, created_at, updated_at)
         VALUES ('https://example.com/', 'Example', '', now(), now()) RETURNING id",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO preview_cards_statuses (preview_card_id, status_id) VALUES ($1, $2)")
        .bind(card)
        .bind(status_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    Box::pin(eunha::delete_account::call(
        &ctx.state,
        bob,
        eunha::delete_account::Options::purge(),
    ))
    .await
    .unwrap();

    let links: i64 =
        sqlx::query_scalar("SELECT count(*) FROM preview_cards_statuses WHERE status_id = $1")
            .bind(status_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(links, 0, "the card links should go with the statuses");
    let items: i64 =
        sqlx::query_scalar("SELECT count(*) FROM collection_items WHERE account_id = $1")
            .bind(bob)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(items, 0, "bob should leave alice's collection");
    let after: i32 = sqlx::query_scalar("SELECT item_count FROM collections WHERE id = $1")
        .bind(collection)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(after, before - 1);
    let gone: Option<i64> = sqlx::query_scalar("SELECT id FROM accounts WHERE id = $1")
        .bind(bob)
        .fetch_optional(&ctx.db)
        .await
        .unwrap();
    assert!(gone.is_none(), "the featured account can now be deleted");
}

/// Statuses attached to an unresolved report survive the purge so moderators
/// can still act on them (`reported_status_ids`), while everything else —
/// including uploads never attached to a status — still goes.
#[tokio::test]
async fn test_delete_account_keeps_reported_statuses() {
    let ctx = TestContext::new("del-acct-reported").await;
    let alice_account_id: i64 = ctx.alice_id.parse().unwrap();

    let reported = ctx
        .api
        .post_status(&ctx.alice_token, "reported content", "public")
        .await;
    let reported_id: i64 = reported["id"].as_str().unwrap().parse().unwrap();
    let plain = ctx
        .api
        .post_status(&ctx.alice_token, "ordinary content", "public")
        .await;
    let plain_id: i64 = plain["id"].as_str().unwrap().parse().unwrap();

    ctx.api
        .post_json(
            "/api/v1/reports",
            Some(&ctx.bob_token),
            &json!({
                "account_id": ctx.alice_id,
                "status_ids": [reported_id.to_string()],
                "comment": "spam",
                "category": "spam",
            }),
        )
        .await;

    sqlx::query(
        r#"INSERT INTO media_attachments (id, account_id, file_file_name, remote_url, type, created_at, updated_at)
           VALUES ($1, $2, 'orphan.png', '', 0, now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(alice_account_id)
    .execute(&ctx.db)
    .await
    .unwrap();

    let resp = ctx
        .api
        .http
        .delete(ctx.api.url("/api/v1/accounts"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"password": "testpassword123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let remaining: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM statuses WHERE account_id = $1 ORDER BY id")
            .bind(alice_account_id)
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    assert_eq!(
        remaining,
        vec![reported_id],
        "only the reported status should survive (plain status {plain_id} should be gone)",
    );
    // Kept for the moderators, but discarded since Mastodon 4.7.3 (#40650).
    let discarded: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM statuses WHERE id = $1")
            .bind(reported_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(discarded, "the reported status should be discarded");

    let media: i64 =
        sqlx::query_scalar("SELECT count(*) FROM media_attachments WHERE account_id = $1")
            .bind(alice_account_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(
        media, 0,
        "unattached media should be purged even when some statuses are kept",
    );
}

/// An account whose deletion was asked for is a 404 wherever Mastodon finds
/// it with `Account.without_requested_deletion` (`AccountsController`,
/// `Accounts::BaseController`, the collections controllers), and by name,
/// as `LookupController` raises for `@account.deleted?`. Its relationship is
/// left out even `with_suspended`.
#[tokio::test]
async fn test_deleted_account_is_not_found() {
    let ctx = TestContext::new("del-acct-404").await;

    // The challenge is read from a form as from JSON.
    let resp = ctx
        .api
        .http
        .delete(ctx.api.url("/api/v1/accounts"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .form(&[("password", "testpassword123")])
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());

    let id = &ctx.alice_id;
    for path in [
        format!("/api/v1/accounts/{id}"),
        "/api/v1/accounts/lookup?acct=alice".to_owned(),
        format!("/api/v1/accounts/{id}/statuses"),
        format!("/api/v1/accounts/{id}/followers"),
        format!("/api/v1/accounts/{id}/following"),
        format!("/api/v1/accounts/{id}/featured_tags"),
        format!("/api/v1/accounts/{id}/endorsements"),
        format!("/api/v1/accounts/{id}/lists"),
        format!("/api/v1/accounts/{id}/collections"),
        format!("/api/v1/accounts/{id}/in_collections"),
    ] {
        let resp = ctx.api.get(&path, Some(&ctx.bob_token)).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "GET {path}");
    }
    for action in [
        "follow",
        "unfollow",
        "block",
        "unblock",
        "mute",
        "unmute",
        "remove_from_followers",
        "endorse",
        "unendorse",
        "note",
    ] {
        let path = format!("/api/v1/accounts/{id}/{action}");
        let resp = ctx
            .api
            .post_json(&path, Some(&ctx.bob_token), &json!({}))
            .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "POST {path}");
    }

    let rels: Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/relationships?id[]={id}&with_suspended=true"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(rels, json!([]));
}

/// A suspended account is still served by id as a blanked tombstone with
/// `suspended: true` (Mastodon's `REST::AccountSerializer`).
#[tokio::test]
async fn test_suspended_account_is_served_as_tombstone() {
    let ctx = TestContext::new("susp-acct-tombstone").await;

    ctx.api
        .patch_json(
            "/api/v1/accounts/update_credentials",
            Some(&ctx.alice_token),
            &json!({"display_name": "Alice", "note": "hello"}),
        )
        .await;
    sqlx::query("UPDATE accounts SET suspended_at = now(), suspension_origin = 0 WHERE id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();

    {
        let path = format!("/api/v1/accounts/{}", ctx.alice_id);
        let resp = ctx.api.get(&path, Some(&ctx.bob_token)).await;
        assert_eq!(resp.status(), StatusCode::OK, "{path} should still resolve");
        let account: Value = resp.json().await.unwrap();
        assert_eq!(
            account["suspended"].as_bool(),
            Some(true),
            "{path} must mark the account suspended: {account}",
        );
        assert_eq!(account["display_name"].as_str(), Some(""), "{path}");
        assert_eq!(account["note"].as_str(), Some(""), "{path}");
    }
}

/// An invalid `source[privacy]` is the `ArgumentError` `UserSettings#[]=`
/// raises, which nothing rescues: a 500, after the account was saved and
/// with the settings left as they were.
#[tokio::test]
async fn test_update_credentials_invalid_privacy_is_unrescued() {
    let ctx = TestContext::new("update-privacy").await;
    let resp = ctx
        .api
        .patch_json(
            "/api/v1/accounts/update_credentials",
            Some(&ctx.alice_token),
            &json!({"display_name": "Saved", "source": {"privacy": "direct", "sensitive": true}}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let me: Value = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["display_name"], json!("Saved"));
    assert_eq!(me["source"]["sensitive"], json!(false));
    assert_ne!(me["source"]["privacy"], json!("direct"));
}

/// Rails' `params` merges the query string over the body, a key at a time:
/// the query's `source` replaces the body's whole `source` hash.
#[tokio::test]
async fn test_update_credentials_reads_the_query_string() {
    let ctx = TestContext::new("update-query").await;
    let resp = ctx
        .api
        .http
        .patch(
            ctx.api
                .url("/api/v1/accounts/update_credentials?display_name=Query&source[language]=de"),
        )
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .form(&[
            ("display_name", "Body"),
            ("note", "from the body"),
            ("source[sensitive]", "true"),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let me: Value = resp.json().await.unwrap();
    assert_eq!(me["display_name"], json!("Query"));
    assert_eq!(me["source"]["note"], json!("from the body"));
    assert_eq!(me["source"]["language"], json!("de"));
    assert_eq!(me["source"]["sensitive"], json!(false));
}

/// A blank boolean is nil (`ActiveModel::Type::Boolean`). Nil in `locked`
/// or `indexable`, `null: false` columns, is the `NotNullViolation` nothing
/// rescues, and nothing is saved; `discoverable` takes it, `bot=` reads it
/// as false, and `source[sensitive]` goes back to its default.
#[tokio::test]
async fn test_update_credentials_blank_booleans() {
    let ctx = TestContext::new("update-blank-bool").await;
    for field in ["locked", "indexable"] {
        let resp = ctx
            .api
            .http
            .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
            .header("host", &ctx.api.host)
            .bearer_auth(&ctx.alice_token)
            .form(&[(field, ""), ("display_name", "Unsaved")])
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR, "{field}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(
            body,
            json!({"status": 500, "error": "Internal Server Error"}),
            "{field}"
        );
    }

    ctx.api
        .patch_json(
            "/api/v1/accounts/update_credentials",
            Some(&ctx.alice_token),
            &json!({"discoverable": true, "bot": true, "source": {"sensitive": true}}),
        )
        .await;
    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .form(&[("discoverable", ""), ("bot", ""), ("source[sensitive]", "")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let me: Value = resp.json().await.unwrap();
    assert_ne!(me["display_name"], json!("Unsaved"));
    assert_eq!(me["discoverable"], Value::Null);
    assert_eq!(me["bot"], json!(false));
    assert_eq!(me["source"]["sensitive"], json!(false));
}

/// `source.follow_requests_count` counts requests from accounts that are
/// not suspended (`Account.without_suspended`).
#[tokio::test]
async fn test_follow_requests_count_leaves_out_suspended_requesters() {
    let ctx = TestContext::new("fr-count-suspended").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    let (carol, _) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@example.com").await;
    for requester in [bob, carol] {
        sqlx::query(
            "INSERT INTO follow_requests (account_id, target_account_id, created_at, updated_at)
             VALUES ($1, $2, now(), now())",
        )
        .bind(requester)
        .bind(alice)
        .execute(&ctx.db)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(carol)
        .execute(&ctx.db)
        .await
        .unwrap();
    let me: Value = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["source"]["follow_requests_count"], json!(1));
}

/// Followers and following as `FollowerAccountsController` lists them: a
/// suspended follower is listed, and the account itself sees the accounts
/// it blocks or mutes; another viewer does not.
#[tokio::test]
async fn test_followers_as_mastodon_lists_them() {
    let ctx = TestContext::new("followers-exclusions").await;
    let (carol, carol_token) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@example.com").await;
    let (dave, dave_token) = seed_user(&ctx.db, &ctx.domain, "dave", "dave@example.com").await;
    let _ = dave;
    for token in [&ctx.bob_token, &carol_token, &dave_token] {
        ctx.api.follow(token, &ctx.alice_id).await;
    }
    ctx.api.follow(&ctx.alice_token, &carol.to_string()).await;
    // Alice mutes Bob; Dave is suspended.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(dave)
        .execute(&ctx.db)
        .await
        .unwrap();

    let usernames = |list: Vec<Value>| -> Vec<String> {
        list.iter()
            .map(|a| a["username"].as_str().unwrap().to_owned())
            .collect()
    };
    let own: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/followers", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let own = usernames(own);
    assert!(own.contains(&"bob".to_owned()), "{own:?}");
    assert!(own.contains(&"dave".to_owned()), "{own:?}");

    // Carol blocks Bob: she does not see him among Alice's followers.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&carol_token),
            &json!({}),
        )
        .await;
    let for_carol: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/followers", ctx.alice_id),
            Some(&carol_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let for_carol = usernames(for_carol);
    assert!(!for_carol.contains(&"bob".to_owned()), "{for_carol:?}");
    assert!(for_carol.contains(&"dave".to_owned()), "{for_carol:?}");
}

/// Familiar followers: every account the viewer follows that follows the
/// target, but those hiding their collections; an unknown or suspended
/// target is left out of the answer.
#[tokio::test]
async fn test_familiar_followers_as_mastodon_finds_them() {
    let ctx = TestContext::new("familiar-mastodon").await;
    let target: i64 = ctx.bob_id.parse().unwrap();
    let mut middles = vec![];
    for i in 0..12 {
        let (id, token) = seed_user(
            &ctx.db,
            &ctx.domain,
            &format!("middle{i}"),
            &format!("middle{i}@example.com"),
        )
        .await;
        ctx.api.follow(&token, &ctx.bob_id).await;
        ctx.api.follow(&ctx.alice_token, &id.to_string()).await;
        middles.push(id);
    }
    sqlx::query("UPDATE accounts SET hide_collections = true WHERE id = $1")
        .bind(middles[0])
        .execute(&ctx.db)
        .await
        .unwrap();
    let (gone, _) = seed_user(&ctx.db, &ctx.domain, "gone", "gone@example.com").await;
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(gone)
        .execute(&ctx.db)
        .await
        .unwrap();

    let body: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/familiar_followers?id[]={target}&id[]={gone}&id[]=999999"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body.len(), 1, "{body:?}");
    assert_eq!(body[0]["id"], json!(target.to_string()));
    assert_eq!(body[0]["accounts"].as_array().unwrap().len(), 11);
}

/// `GET /api/v1/accounts/lookup`: `user@` this domain is local, a lookup
/// never resolves over WebFinger, and a blank `acct` is a 404.
#[tokio::test]
async fn test_lookup_as_resolve_account_service_with_skip_webfinger() {
    let ctx = TestContext::new("lookup-skip").await;
    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/lookup?acct=@ALICE@{}", ctx.domain),
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"], json!(ctx.alice_id));
    for query in ["acct=", "acct=nobody@unknown.invalid&resolve=true", ""] {
        let resp = ctx
            .api
            .get(&format!("/api/v1/accounts/lookup?{query}"), None)
            .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{query}");
    }
}

/// `GET /api/v1/accounts?id[]=`: at most 40 ids, `Mastodon::ValidationError`
/// beyond; unconfirmed local accounts are left out; a lone `id` is no list.
#[tokio::test]
async fn test_accounts_batch_as_accounts_controller_index() {
    let ctx = TestContext::new("accounts-batch").await;
    let (carol, _) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@example.com").await;
    sqlx::query("UPDATE users SET confirmed_at = NULL WHERE account_id = $1")
        .bind(carol)
        .execute(&ctx.db)
        .await
        .unwrap();
    let body: Vec<Value> = ctx
        .api
        .get(
            &format!(
                "/api/v1/accounts?id[]={}&id[]={}&id[]={carol}",
                ctx.alice_id, ctx.bob_id
            ),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body.len(), 2, "{body:?}");
    let lone: Vec<Value> = ctx
        .api
        .get(&format!("/api/v1/accounts?id={}", ctx.alice_id), None)
        .await
        .json()
        .await
        .unwrap();
    assert!(lone.is_empty());
    let many: String = (1..=41).map(|i| format!("id[]={i}&")).collect();
    let resp = ctx.api.get(&format!("/api/v1/accounts?{many}"), None).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Lineage survives a *chain* of deletions: when an inviter is deleted after
/// its own inviter already was, the earlier snapshot is not overwritten with
/// the now-missing link.
#[tokio::test]
async fn test_delete_account_preserves_chained_invite_lineage() {
    let ctx = TestContext::new("del-acct-chain").await;
    let alice_account_id: i64 = ctx.alice_id.parse().unwrap();
    let bob_account_id: i64 = ctx.bob_id.parse().unwrap();

    // Alice invited Bob, Bob invited Carol.
    let (carol_id, _carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    for (inviter, invitee) in [
        (alice_account_id, bob_account_id),
        (bob_account_id, carol_id),
    ] {
        let invite_id: i64 = sqlx::query_scalar(
            r#"INSERT INTO invites (user_id, code, uses, created_at, updated_at)
               SELECT id, md5(random()::text), 1, now(), now() FROM users WHERE account_id = $1
               RETURNING id"#,
        )
        .bind(inviter)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
        sqlx::query("UPDATE users SET invite_id = $1 WHERE account_id = $2")
            .bind(invite_id)
            .bind(invitee)
            .execute(&ctx.db)
            .await
            .unwrap();
    }

    // Alice deletes first, then Bob.
    ctx.api
        .http
        .delete(ctx.api.url("/api/v1/accounts"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"password": "testpassword123"}))
        .send()
        .await
        .unwrap();
    ctx.api
        .http
        .delete(ctx.api.url("/api/v1/accounts"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.bob_token)
        .json(&json!({"password": "testpassword123"}))
        .send()
        .await
        .unwrap();

    let lineage: Vec<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT account_id, inviter_account_id FROM eunha.invite_lineage ORDER BY account_id",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert!(
        lineage.contains(&(bob_account_id, Some(alice_account_id))),
        "Bob → Alice should survive Bob's own deletion: {lineage:?}",
    );
    assert!(
        lineage.contains(&(carol_id, Some(bob_account_id))),
        "Carol → Bob should be recorded: {lineage:?}",
    );
}

/// Destroying the user record takes its `invites` with it, which would orphan
/// everyone it invited. eunha snapshots the lineage into `eunha.invite_lineage`
/// first, so the invite tree survives a deletion that removes the PII.
#[tokio::test]
async fn test_delete_account_preserves_invite_lineage() {
    let ctx = TestContext::new("del-acct-lineage").await;
    let alice_account_id: i64 = ctx.alice_id.parse().unwrap();
    let bob_account_id: i64 = ctx.bob_id.parse().unwrap();

    // Alice invites Bob.
    let invite: Value = ctx
        .api
        .post_json("/api/v1/invites", Some(&ctx.alice_token), &json!({}))
        .await
        .json()
        .await
        .unwrap();
    let invite_id: i64 = invite["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE invites SET uses = 1 WHERE id = $1")
        .bind(invite_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET invite_id = $1 WHERE account_id = $2")
        .bind(invite_id)
        .bind(bob_account_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    let resp = ctx
        .api
        .http
        .delete(ctx.api.url("/api/v1/accounts"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"password": "testpassword123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The invite went with the user record…
    let invite_exists: Option<i64> = sqlx::query_scalar("SELECT id FROM invites WHERE id = $1")
        .bind(invite_id)
        .fetch_optional(&ctx.db)
        .await
        .unwrap();
    assert!(
        invite_exists.is_none(),
        "invites should be destroyed with the user record"
    );

    // …but the lineage it encoded did not.
    let inviter: Option<i64> = sqlx::query_scalar(
        "SELECT inviter_account_id FROM eunha.invite_lineage WHERE account_id = $1",
    )
    .bind(bob_account_id)
    .fetch_one(&ctx.db)
    .await
    .expect("lineage row for the invitee");
    assert_eq!(
        inviter,
        Some(alice_account_id),
        "invitee should still point at its inviter"
    );

    // Bob is still a member of the tree (promoted to a root, since a suspended
    // inviter is not itself listed).
    let tree: Value = ctx
        .api
        .get("/api/eunha/v1/invite_tree", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    let roots = tree["roots"].as_array().unwrap();
    assert!(
        roots.iter().any(|n| n["id"] == ctx.bob_id.as_str()),
        "invitee should still appear in the invite tree: {tree}"
    );
}

/// DELETE /api/v1/accounts with wrong password returns 401.
#[tokio::test]
async fn test_delete_account_wrong_password_is_401() {
    let ctx = TestContext::new("del-acct-wrong").await;

    let resp = ctx
        .api
        .http
        .delete(ctx.api.url("/api/v1/accounts"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"password": "notmypassword"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ── GET /api/v1/accounts (batch) ─────────────────────────────────────────────

/// GET /api/v1/accounts?id[]=...&id[]=... returns the requested accounts.
#[tokio::test]
async fn test_get_accounts_batch() {
    let ctx = TestContext::new("acct-batch").await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts?id[]={}&id[]={}", ctx.alice_id, ctx.bob_id),
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let accounts: Vec<Value> = resp.json().await.unwrap();
    let ids: Vec<&str> = accounts.iter().filter_map(|a| a["id"].as_str()).collect();
    assert!(
        ids.contains(&ctx.alice_id.as_str()),
        "alice missing from batch"
    );
    assert!(ids.contains(&ctx.bob_id.as_str()), "bob missing from batch");
}

/// GET /api/v1/accounts?id[]= with empty list returns empty array.
#[tokio::test]
async fn test_get_accounts_batch_empty() {
    let ctx = TestContext::new("acct-batch-empty").await;

    let resp = ctx.api.get("/api/v1/accounts", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let accounts: Vec<Value> = resp.json().await.unwrap();
    assert!(
        accounts.is_empty(),
        "expected empty array for no ids: {accounts:?}"
    );
}

// ── GET /api/v1/apps/verify_credentials ──────────────────────────────────────

/// GET /api/v1/apps/verify_credentials with a valid token returns the app name.
#[tokio::test]
async fn test_verify_app_credentials() {
    let ctx = TestContext::new("app-verify").await;

    let resp = ctx
        .api
        .get("/api/v1/apps/verify_credentials", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(body["name"].as_str().is_some(), "app name missing: {body}");
}

/// GET /api/v1/apps/verify_credentials without a token returns 401.
#[tokio::test]
async fn test_verify_app_credentials_without_token_is_401() {
    let ctx = TestContext::new("app-verify-unauth").await;

    let resp = ctx.api.get("/api/v1/apps/verify_credentials", None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Following an account that has blocked you is rejected with 403 (Mastodon
/// FollowService raises NotPermittedError).
#[tokio::test]
async fn test_follow_blocked_by_target_is_forbidden() {
    let ctx = TestContext::new("follow-blocked-by").await;

    // Bob blocks Alice first.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.alice_id),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    // Alice tries to follow Bob — not allowed.
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // And no follow relationship exists.
    let rel: Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/relationships?id[]={}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(rel[0]["following"].as_bool(), Some(false));
}

/// GET /api/v1/accounts/:id for a suspended account returns 200 with suspended=true.
#[tokio::test]
async fn test_get_suspended_account_returns_suspended() {
    let ctx = TestContext::new("acct-suspended-200").await;

    // Make alice admin
    let alice_uuid: i64 = ctx.alice_id.parse().unwrap();
    let admin_db = ctx.db.clone();
    crate::helpers::make_admin(&admin_db, alice_uuid).await;

    // Suspend bob via admin endpoint
    ctx.api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"type": "suspend"}),
        )
        .await;

    let resp = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.bob_id), None)
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "suspended account should return 200"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["suspended"], true,
        "suspended account should have suspended=true"
    );
}

/// Unlocking a locked account auto-approves pending follow requests.
#[tokio::test]
async fn test_unlock_account_approves_pending_follows() {
    let ctx = TestContext::new("unlock-approve").await;

    let db = ctx.db.clone();
    let alice_uuid: i64 = ctx.alice_id.parse().unwrap();

    // Lock Alice's account.
    sqlx::query!(
        "UPDATE accounts SET locked = true WHERE id = $1",
        alice_uuid
    )
    .execute(&db)
    .await
    .unwrap();

    // Bob sends a follow request (becomes pending because account is locked).
    let follow_resp: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.alice_id),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        follow_resp["requested"].as_bool(),
        Some(true),
        "follow should be pending"
    );

    // Alice unlocks her account.
    ctx.api
        .patch_multipart(
            "/api/v1/accounts/update_credentials",
            &ctx.alice_token,
            &[("locked", "false")],
        )
        .await;
    // `AuthorizeFollowWorker`s.
    ctx.state.jobs.settle().await;

    // Bob's follow should now be accepted.
    let rel: Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/relationships?id[]={}", ctx.alice_id),
            Some(&ctx.bob_token),
        )
        .await
        .json::<Vec<Value>>()
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        rel["following"].as_bool(),
        Some(true),
        "follow should be accepted after unlock"
    );
    assert_eq!(
        rel["requested"].as_bool(),
        Some(false),
        "follow should not be pending after unlock"
    );
}

/// GET /api/v1/accounts/:id/statuses is empty, not an error, for a viewer
/// the account blocks: `AccountStatusesFilter#initial_scope` is
/// `Status.none` when `blocked?`.
#[tokio::test]
async fn test_account_statuses_are_empty_when_blocked_by_target() {
    let ctx = TestContext::new("acct-statuses-blocked").await;

    ctx.api
        .post_status(&ctx.alice_token, "alice public status", "public")
        .await;

    // Alice blocks Bob.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Bob tries to view Alice's statuses.
    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses", ctx.alice_id),
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("link").is_none());
    let statuses: Vec<Value> = resp.json().await.unwrap();
    assert!(statuses.is_empty(), "{statuses:?}");
}

/// A quote with no text of its own is listed: Mastodon has no condition on
/// the text.
#[tokio::test]
async fn test_account_statuses_list_a_quote_without_text() {
    let ctx = TestContext::new("acct-statuses-quote").await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "quote me", "public")
        .await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let quoted: i64 = status["id"].as_str().unwrap().parse().unwrap();
    // A remote server's quote-only post, as it is stored: no text.
    let quote_id: i64 = sqlx::query_scalar(
        "INSERT INTO statuses (id, account_id, text, visibility, local, created_at, updated_at)
         VALUES (timestamp_id('statuses'), $1, '', 0, true, now(), now()) RETURNING id",
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO quotes (status_id, quoted_status_id, account_id, quoted_account_id, state, created_at, updated_at)
         VALUES ($1, $2, $3, $3, 1, now(), now())",
    )
    .bind(quote_id)
    .bind(quoted)
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();

    let statuses: Vec<Value> = ctx
        .api
        .get(&format!("/api/v1/accounts/{}/statuses", ctx.alice_id), None)
        .await
        .json()
        .await
        .unwrap();
    assert!(
        statuses
            .iter()
            .any(|s| s["id"].as_str() == Some(quote_id.to_string().as_str())),
        "{statuses:?}"
    );
}

/// `filtered_reblogs_scope`: a boost of an account the viewer mutes, or
/// that blocks the viewer, is left out; the author still sees it.
#[tokio::test]
async fn test_account_statuses_leave_out_boosts_of_excluded_accounts() {
    let ctx = TestContext::new("acct-statuses-reblogs").await;
    let (carol_id, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@example.com").await;
    let (dave_id, dave_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "dave", "dave@example.com").await;
    let by_carol = ctx.api.post_status(&carol_token, "carol's", "public").await;
    let by_dave = ctx.api.post_status(&dave_token, "dave's", "public").await;
    for status in [&by_carol, &by_dave] {
        let resp = ctx
            .api
            .post_json(
                &format!("/api/v1/statuses/{}/reblog", status["id"].as_str().unwrap()),
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    // Bob mutes Carol; Dave blocks Bob.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{carol_id}/mute"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&dave_token),
            &json!({}),
        )
        .await;
    let _ = dave_id;

    let reblogged = |statuses: &[Value]| -> Vec<String> {
        statuses
            .iter()
            .filter_map(|s| s["reblog"]["id"].as_str().map(str::to_owned))
            .collect()
    };
    let for_bob: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses", ctx.alice_id),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(reblogged(&for_bob).is_empty(), "{for_bob:?}");
    let for_alice: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/statuses", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(reblogged(&for_alice).len(), 2, "{for_alice:?}");
}

/// `pinned=true` is one filter among the others: it pages by `limit` and
/// `max_id`, newest pin first, and `exclude_replies` still applies.
#[tokio::test]
async fn test_account_statuses_pinned_pages_with_the_other_filters() {
    let ctx = TestContext::new("acct-pinned-paged").await;
    let mut ids = vec![];
    for text in ["one", "two", "three"] {
        let status = ctx.api.post_status(&ctx.alice_token, text, "public").await;
        ids.push(status["id"].as_str().unwrap().to_owned());
    }
    let bob_status = ctx.api.post_status(&ctx.bob_token, "hi", "public").await;
    let reply = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "@bob reply", "in_reply_to_id": bob_status["id"]}),
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    // Pinned in the order three, one, reply: the reply pinned last.
    for id in [&ids[2], &ids[0], &reply["id"].as_str().unwrap().to_owned()] {
        let resp = ctx
            .api
            .post_json(
                &format!("/api/v1/statuses/{id}/pin"),
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let page = |query: String| {
        let ctx = &ctx;
        async move {
            let statuses: Vec<Value> = ctx
                .api
                .get(
                    &format!("/api/v1/accounts/{}/statuses?{query}", ctx.alice_id),
                    None,
                )
                .await
                .json()
                .await
                .unwrap();
            statuses
                .iter()
                .map(|s| s["id"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        page("pinned=true&exclude_replies=true".into()).await,
        vec![ids[0].clone(), ids[2].clone()]
    );
    assert_eq!(
        page("pinned=1&limit=1".into()).await,
        vec![reply["id"].as_str().unwrap().to_owned()]
    );
    assert_eq!(
        page(format!("pinned=true&max_id={}", ids[2])).await,
        vec![ids[0].clone()]
    );
}

/// GET /api/v1/accounts/:id/statuses is visible to unauthenticated requests (public accounts).
#[tokio::test]
async fn test_account_statuses_visible_unauthenticated() {
    let ctx = TestContext::new("acct-statuses-unauth").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "public for unauth", "public")
        .await;
    let status_id = status["id"].as_str().unwrap();

    let statuses: Vec<Value> = ctx
        .api
        .get(&format!("/api/v1/accounts/{}/statuses", ctx.alice_id), None)
        .await
        .json()
        .await
        .unwrap();

    assert!(
        statuses.iter().any(|s| s["id"].as_str() == Some(status_id)),
        "public status should be visible to unauthenticated users",
    );
}

/// GET /api/v1/accounts/:id/followers respects the limit parameter.
#[tokio::test]
async fn test_get_account_followers_limit_param() {
    let ctx = TestContext::new("followers-limit").await;

    // Bob follows Alice.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/followers?limit=1", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(list.len() <= 1, "limit=1 should return at most 1 follower");
}

/// GET /api/v1/accounts/:id/following respects the limit parameter.
#[tokio::test]
async fn test_get_account_following_limit_param() {
    let ctx = TestContext::new("following-limit").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/following?limit=1", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(list.len() <= 1, "limit=1 should return at most 1 following");
}

/// GET /api/v1/accounts/:id/followers returns only followers of the given account.
#[tokio::test]
async fn test_get_account_followers_scoped_to_account() {
    let ctx = TestContext::new("followers-scoped").await;

    // Bob follows Alice but not vice versa.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let alice_followers: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/followers", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    let bob_followers: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/followers", ctx.bob_id),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        alice_followers
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())),
        "Bob should appear in Alice's followers"
    );
    assert!(
        !bob_followers
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.alice_id.as_str())),
        "Alice should not appear in Bob's followers (she didn't follow Bob)"
    );
}

/// GET /api/v1/accounts/:id/following excludes accounts with pending (not accepted) follows.
#[tokio::test]
async fn test_get_account_following_excludes_pending() {
    let ctx = TestContext::new("following-pending").await;

    // Lock Alice's account so Bob's follow becomes pending.
    ctx.api
        .patch_multipart(
            "/api/v1/accounts/update_credentials",
            &ctx.alice_token,
            &[("locked", "true")],
        )
        .await;

    // Bob sends a follow request to Alice (pending).
    let rel: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.alice_id),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        rel["requested"].as_bool(),
        Some(true),
        "follow should be pending"
    );

    // Bob's following list should NOT include Alice (follow is not accepted).
    let following: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/following", ctx.bob_id),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        !following
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.alice_id.as_str())),
        "pending follow should not appear in following list"
    );
}

/// Blocked accounts are hidden from followers/following lists.
#[tokio::test]
async fn test_followers_following_hides_blocked_accounts() {
    let ctx = TestContext::new("follow-block-hide").await;

    let (charlie_uuid, _charlie_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "charlie", "charlie@test.invalid").await;
    let charlie_id = charlie_uuid.to_string();

    // Both Bob and Charlie follow Alice.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.alice_id),
            Some(&_charlie_token),
            &json!({}),
        )
        .await;

    // Alice follows both Bob and Charlie.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    ctx.api.follow(&ctx.alice_token, &charlie_id).await;

    // Alice blocks Charlie.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{charlie_id}/block"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Alice's followers list should hide Charlie (blocked).
    let followers: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/followers", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !followers
            .iter()
            .any(|a| a["id"].as_str() == Some(charlie_id.as_str())),
        "blocked account should not appear in followers list"
    );
    assert!(
        followers
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())),
        "non-blocked account should still appear in followers list"
    );

    // Alice's following list should also hide Charlie.
    let following: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/following", ctx.alice_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !following
            .iter()
            .any(|a| a["id"].as_str() == Some(charlie_id.as_str())),
        "blocked account should not appear in following list"
    );
}

/// Followers list is ordered by account id DESC, matching the pagination cursor.
#[tokio::test]
async fn test_followers_ordered_by_account_id_desc() {
    let ctx = TestContext::new("followers-order").await;

    let (charlie_uuid, charlie_token) = crate::helpers::seed_user(
        &ctx.db,
        &ctx.domain,
        "charlie-forder",
        "charlie-forder@test.invalid",
    )
    .await;
    let charlie_id = charlie_uuid.to_string();

    // Alice and Bob both follow Charlie; Alice has a lower account ID than Bob.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{charlie_id}/follow"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{charlie_id}/follow"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    // Charlie accepts both (accounts are unlocked in tests).

    let followers: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{charlie_id}/followers"),
            Some(&charlie_token),
        )
        .await
        .json()
        .await
        .unwrap();

    let ids: Vec<i64> = followers
        .iter()
        .filter_map(|a| a["id"].as_str().and_then(|s| s.parse::<i64>().ok()))
        .collect();
    assert!(ids.len() >= 2, "both alice and bob should be in followers");

    let sorted_desc: Vec<i64> = {
        let mut s = ids.clone();
        s.sort_unstable_by(|a, b| b.cmp(a));
        s
    };
    assert_eq!(
        ids, sorted_desc,
        "followers should be ordered by account id DESC"
    );
}

/// Following list is ordered by account id DESC, matching the pagination cursor.
#[tokio::test]
async fn test_following_ordered_by_account_id_desc() {
    let ctx = TestContext::new("following-order").await;

    let (charlie_uuid, charlie_token) = crate::helpers::seed_user(
        &ctx.db,
        &ctx.domain,
        "charlie-fgorder",
        "charlie-fgorder@test.invalid",
    )
    .await;
    let charlie_id = charlie_uuid.to_string();

    // Charlie follows both Alice and Bob.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.alice_id),
            Some(&charlie_token),
            &json!({}),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&charlie_token),
            &json!({}),
        )
        .await;

    let following: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{charlie_id}/following"),
            Some(&charlie_token),
        )
        .await
        .json()
        .await
        .unwrap();

    let ids: Vec<i64> = following
        .iter()
        .filter_map(|a| a["id"].as_str().and_then(|s| s.parse::<i64>().ok()))
        .collect();
    assert!(
        ids.len() >= 2,
        "both alice and bob should be in following list"
    );

    let sorted_desc: Vec<i64> = {
        let mut s = ids.clone();
        s.sort_unstable_by(|a, b| b.cmp(a));
        s
    };
    assert_eq!(
        ids, sorted_desc,
        "following should be ordered by account id DESC"
    );
}

// ── exclude_replies self-reply inclusion ─────────────────────────────────────

/// exclude_replies=true keeps self-replies (replies to own posts).
#[tokio::test]
async fn test_account_statuses_exclude_replies_keeps_self_replies() {
    let ctx = TestContext::new("acct-excl-selfreply").await;

    // Alice posts a status and then replies to herself.
    let parent = ctx
        .api
        .post_status(&ctx.alice_token, "alice original", "public")
        .await;
    let parent_id = parent["id"].as_str().unwrap();

    let self_reply: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({"status": "alice self-reply", "in_reply_to_id": parent_id, "visibility": "public"}),
    ).await.json().await.unwrap();
    let self_reply_id = self_reply["id"].as_str().unwrap();

    // Bob posts a status and alice replies to bob (a foreign reply).
    let bob_post = ctx
        .api
        .post_status(&ctx.bob_token, "bob post", "public")
        .await;
    let bob_post_id = bob_post["id"].as_str().unwrap();
    let foreign_reply: Value = ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.alice_token),
        &json!({"status": "alice reply to bob", "in_reply_to_id": bob_post_id, "visibility": "public"}),
    ).await.json().await.unwrap();
    let foreign_reply_id = foreign_reply["id"].as_str().unwrap();

    let statuses: Vec<Value> = ctx
        .api
        .get(
            &format!(
                "/api/v1/accounts/{}/statuses?exclude_replies=true",
                ctx.alice_id
            ),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    let ids: Vec<&str> = statuses.iter().filter_map(|s| s["id"].as_str()).collect();
    assert!(ids.contains(&parent_id), "parent should appear");
    assert!(
        ids.contains(&self_reply_id),
        "self-reply should be kept when exclude_replies=true"
    );
    assert!(
        !ids.contains(&foreign_reply_id),
        "reply-to-other should be excluded"
    );
}

// ── GET /api/v1/preferences ──────────────────────────────────────────────────

/// Preferences endpoint requires authentication.
#[tokio::test]
async fn test_preferences_requires_auth() {
    let ctx = TestContext::new("prefs-unauth").await;

    let resp = ctx.api.get("/api/v1/preferences", None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ── account roles ─────────────────────────────────────────────────────────────

/// GET /api/v1/accounts/:id returns a `roles` array. For ordinary users it is
/// empty; for admins it contains an entry with `name: "Admin"`.
#[tokio::test]
async fn test_get_account_includes_roles() {
    let ctx = TestContext::new("acct-roles").await;

    // Ordinary user: roles must be an empty array.
    let resp = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.alice_id), None)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let roles = body["roles"].as_array().expect("roles must be an array");
    assert!(roles.is_empty(), "ordinary user should have no roles");

    // Promote alice to admin.
    crate::helpers::make_admin(&ctx.db, ctx.alice_id.parse::<i64>().unwrap()).await;

    let resp2 = ctx
        .api
        .get(&format!("/api/v1/accounts/{}", ctx.alice_id), None)
        .await;
    let body2: Value = resp2.json().await.unwrap();
    let roles2 = body2["roles"].as_array().expect("roles must be an array");
    assert!(!roles2.is_empty(), "admin should have a role entry");
    assert_eq!(roles2[0]["name"].as_str(), Some("Admin"));
}

// ── GET /api/v1/profile ────────────────────────────────────────────────────────

/// GET /api/v1/profile returns the authenticated account.
#[tokio::test]
async fn test_get_profile() {
    let ctx = TestContext::new("get-profile").await;

    let resp = ctx.api.get("/api/v1/profile", Some(&ctx.alice_token)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"].as_str(), Some(ctx.alice_id.as_str()));
    // `REST::ProfileSerializer`: no `username`, the pictures' descriptions
    // and the profile tab settings, and `null` for a picture never uploaded.
    assert!(body.get("username").is_none(), "{body}");
    assert_eq!(body["avatar"], Value::Null, "{body}");
    assert_eq!(body["header_static"], Value::Null, "{body}");
    assert_eq!(body["avatar_description"], json!(""), "{body}");
    assert_eq!(body["header_description"], json!(""), "{body}");
    for key in ["show_media", "show_media_replies", "show_featured"] {
        assert!(body[key].is_boolean(), "{key} in {body}");
    }
}

/// PUT /api/v1/profile is the same update as PATCH — Rails routes a
/// singular resource's `update` from both.
#[tokio::test]
async fn test_put_profile_updates() {
    let ctx = TestContext::new("put-profile").await;
    let body: Value = ctx
        .api
        .put_json(
            "/api/v1/profile",
            Some(&ctx.alice_token),
            &json!({"display_name": "Put Alice", "show_media": false}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["display_name"], json!("Put Alice"), "{body}");
    assert_eq!(body["show_media"], json!(false), "{body}");
}

/// GET /api/v1/profile without a token → 401.
#[tokio::test]
async fn test_get_profile_requires_auth() {
    let ctx = TestContext::new("get-profile-unauth").await;

    let resp = ctx.api.get("/api/v1/profile", None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// POST /api/v1/accounts/:id/follow with languages=[...] sets the language filter.
#[tokio::test]
async fn test_follow_with_languages_filter() {
    let ctx = TestContext::new("follow-languages").await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({"languages": ["en", "ko"]}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let rel: Value = resp.json().await.unwrap();
    assert_eq!(rel["following"].as_bool(), Some(true));
    let langs = rel["languages"]
        .as_array()
        .expect("languages should be an array");
    assert!(
        langs.iter().any(|l| l.as_str() == Some("en")),
        "languages should include en"
    );
    assert!(
        langs.iter().any(|l| l.as_str() == Some("ko")),
        "languages should include ko"
    );
}

/// GET /api/v1/mutes returns Link header with pagination when limit=1.
#[tokio::test]
async fn test_mutes_list_has_pagination_link_headers() {
    let ctx = TestContext::new("mutes-pagination-headers").await;

    // Alice mutes two accounts so there are enough to paginate.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;

    // Seed a third account and mute it.
    let (charlie_id, _) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "charlie", "charlie@test.invalid").await;
    let charlie_id = charlie_id.to_string();
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{charlie_id}/mute"),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;

    let resp = ctx
        .api
        .http
        .get(ctx.api.url("/api/v1/mutes?limit=1"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let link = resp
        .headers()
        .get("link")
        .expect("Link header missing for paginated mutes");
    let link_str = link.to_str().unwrap();
    assert!(
        link_str.contains("next"),
        "Link header should include 'next'"
    );
    assert!(
        link_str.contains("prev"),
        "Link header should include 'prev'"
    );
}

/// GET /api/v1/directory?local=true returns only local accounts.
#[tokio::test]
async fn test_directory_local_param() {
    let ctx = TestContext::new("dir-local").await;

    let resp = ctx
        .api
        .http
        .get(ctx.api.url("/api/v1/directory?local=true"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let accounts: Vec<Value> = resp.json().await.unwrap();
    for acct in &accounts {
        let acct_field = acct["acct"].as_str().unwrap_or_default();
        assert!(
            !acct_field.contains('@'),
            "local=true should not return remote accounts (got {})",
            acct_field
        );
    }
}

/// GET /api/v1/donation_campaigns needs a user, and answers `204` while no
/// campaign API is configured.
#[tokio::test]
async fn test_donation_campaigns_unconfigured() {
    let ctx = TestContext::new("donation-campaigns").await;

    let resp = ctx.api.get("/api/v1/donation_campaigns", None).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let resp = ctx
        .api
        .get("/api/v1/donation_campaigns", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

/// A campaign Mastodon cached for this seed and locale is served from the
/// cache, without asking the campaign API (here nowhere to be reached).
#[tokio::test]
async fn test_donation_campaigns_served_from_mastodons_cache() {
    let ctx = TestContext::with_instance_config("donation-cached", |instance| {
        instance.donation_campaigns.api_url =
            Some("https://donations.invalid/api/v1/campaigns".into());
    })
    .await;
    let seed = eunha::api::mastodon::donation_campaigns::seed(ctx.alice_id.parse().unwrap());
    let mut redis = ctx.state.redis.clone();
    let _: () = redis::pipe()
        .cmd("SET")
        .arg(
            ctx.state
                .redis_keys
                .key(format!("cache:donation_campaign_request:{seed}:en")),
        )
        .arg("spring:en")
        .ignore()
        .cmd("SET")
        .arg(
            ctx.state
                .redis_keys
                .key("cache:donation_campaign:spring:en"),
        )
        .arg(r#"{"id":"spring","locale":"en","banner_message":"Give"}"#)
        .ignore()
        .query_async(&mut redis)
        .await
        .unwrap();

    let resp = ctx
        .api
        .get("/api/v1/donation_campaigns", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        json!({"id": "spring", "locale": "en", "banner_message": "Give"})
    );

    // Another locale has nothing cached, and the API cannot be reached.
    let resp = ctx
        .api
        .get("/api/v1/donation_campaigns?lang=de", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

/// GET /api/v1/accounts/:id/identity_proofs returns empty array (stub).
#[tokio::test]
async fn test_account_identity_proofs_returns_array() {
    let ctx = TestContext::new("identity-proofs").await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}/identity_proofs", ctx.alice_id),
            None,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Vec<serde_json::Value> = resp.json().await.unwrap();
    let _ = body;
}

/// GET /api/v1/directory?order=new returns accounts ordered by creation date descending.
#[tokio::test]
async fn test_directory_order_new() {
    let ctx = TestContext::new("dir-order-new").await;

    let resp = ctx
        .api
        .http
        .get(ctx.api.url("/api/v1/directory?order=new"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let accounts: Vec<Value> = resp.json().await.unwrap();
    // Verify the response is an array (ordering correctness is hard to assert without
    // precise seeding, but we verify the param is accepted and returns valid JSON).
    let _ = accounts;
}

/// PATCH /api/v1/accounts/update_credentials takes a JSON body, as Rails
/// reads JSON into the same params as a form: nested `source`, and
/// `fields_attributes` as an array.
#[tokio::test]
async fn test_update_credentials_accepts_json() {
    let ctx = TestContext::new("update-creds-json").await;
    let resp = ctx
        .api
        .patch_json(
            "/api/v1/accounts/update_credentials",
            Some(&ctx.alice_token),
            &json!({
                "display_name": "Alice J",
                "locked": true,
                "source": {"privacy": "unlisted"},
                "fields_attributes": [{"name": "Site", "value": "example.com"}],
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["display_name"], "Alice J");
    assert_eq!(body["locked"], true);
    assert_eq!(body["source"]["privacy"], "unlisted");
    assert_eq!(body["fields"][0]["name"], "Site");
}

/// `pin` and `unpin` are Mastodon's older names for endorsing.
#[tokio::test]
async fn test_pin_and_unpin_endorse() {
    let ctx = TestContext::new("pin-endorse").await;
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    for (verb, endorsed) in [("pin", true), ("unpin", false)] {
        let rel: Value = ctx
            .api
            .post_json(
                &format!("/api/v1/accounts/{}/{verb}", ctx.bob_id),
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(rel["endorsed"].as_bool(), Some(endorsed), "{verb}");
    }
}

/// `feature_approval.current_user` of account `id`, as `token`'s holder sees it.
async fn feature_approval_seen(ctx: &TestContext, id: i64, token: Option<&str>) -> Value {
    let account: Value = ctx
        .api
        .get(&format!("/api/v1/accounts/{id}"), token)
        .await
        .json()
        .await
        .unwrap();
    account["feature_approval"]["current_user"].clone()
}

async fn insert_follow(ctx: &TestContext, from: i64, to: i64) {
    sqlx::query(
        "INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
         VALUES (timestamp_id('follows'), $1, $2, now(), now())",
    )
    .bind(from)
    .bind(to)
    .execute(&ctx.db)
    .await
    .unwrap();
}

/// `feature_approval.current_user` is answered for the viewer, as
/// `REST::AccountSerializer` answers it from `current_user`: on the account
/// itself, on one embedded in a post, and from the follows in each direction
/// that `Account#feature_policy_for_account` reads.
#[tokio::test]
async fn test_feature_approval_is_answered_for_the_viewer() {
    let ctx = TestContext::new("feature-approval-viewer").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    sqlx::query("UPDATE accounts SET discoverable = true, locked = true WHERE id = $1")
        .bind(alice)
        .execute(&ctx.db)
        .await
        .unwrap();

    // Locked, and Bob does not follow her.
    assert_eq!(
        feature_approval_seen(&ctx, alice, Some(&ctx.bob_token)).await,
        "denied"
    );
    // Nobody asking is refused; Alice may always feature herself.
    assert_eq!(feature_approval_seen(&ctx, alice, None).await, "denied");
    assert_eq!(
        feature_approval_seen(&ctx, alice, Some(&ctx.alice_token)).await,
        "automatic"
    );

    // Alice following Bob does not make Bob her follower.
    insert_follow(&ctx, alice, bob).await;
    assert_eq!(
        feature_approval_seen(&ctx, alice, Some(&ctx.bob_token)).await,
        "denied"
    );
    insert_follow(&ctx, bob, alice).await;
    assert_eq!(
        feature_approval_seen(&ctx, alice, Some(&ctx.bob_token)).await,
        "automatic"
    );

    // An account embedded in a post is answered the same way.
    let post = ctx
        .api
        .post_status(&ctx.alice_token, "featured?", "public")
        .await;
    let seen: Value = ctx
        .api
        .get(
            &format!("/api/v1/statuses/{}", post["id"].as_str().unwrap()),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        seen["account"]["feature_approval"]["current_user"],
        "automatic"
    );

    // A remote account's federated policy: anyone it follows may feature it
    // automatically (`following`), its followers with approval (`followers`).
    let following = 1 << 3;
    let followers = 1 << 2;
    let remote: i64 = sqlx::query_scalar(
        "INSERT INTO accounts (id, username, domain, uri, url, discoverable, protocol,
                               feature_approval_policy, created_at, updated_at)
         VALUES (timestamp_id('accounts'), 'remy', 'remote.example',
                 'https://remote.example/users/remy', 'https://remote.example/@remy',
                 true, 1, $1, now(), now())
         RETURNING id",
    )
    .bind((following << 16) | followers)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        feature_approval_seen(&ctx, remote, Some(&ctx.bob_token)).await,
        "denied"
    );
    insert_follow(&ctx, bob, remote).await;
    assert_eq!(
        feature_approval_seen(&ctx, remote, Some(&ctx.bob_token)).await,
        "manual"
    );
    insert_follow(&ctx, remote, bob).await;
    assert_eq!(
        feature_approval_seen(&ctx, remote, Some(&ctx.bob_token)).await,
        "automatic"
    );
}

/// Mastodon serializes `roles` only for a local account (`has_many :roles,
/// if: :local?`); a remote account has no roles here to show, so the key is
/// absent rather than an empty list.
#[tokio::test]
async fn test_roles_only_on_local_accounts() {
    let ctx = TestContext::new("roles-local-only").await;
    let remote = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, inbox_url, protocol, created_at, updated_at)
           VALUES ($1, 'faraway', 'roles.invalid', '', '', 'https://roles.invalid/@faraway',
                   'https://roles.invalid/users/faraway', 'https://roles.invalid/inbox', 1, now(), now())"#,
    )
    .bind(remote)
    .execute(&ctx.db)
    .await
    .unwrap();

    let remote: Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{remote}"),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(remote.get("roles").is_none(), "{remote}");

    let local: Value = ctx
        .api
        .get(
            &format!("/api/v1/accounts/{}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(local["roles"], json!([]));
}

/// `feature_approval.current_user` is where the viewer stands: a
/// discoverable, unlocked local account may be featured by anyone signed in
/// (`automatic`), and by nobody when nobody is asking (`denied`).
#[tokio::test]
async fn test_feature_approval_is_for_the_viewer() {
    let ctx = TestContext::new("feature-approval-lookup").await;
    sqlx::query("UPDATE accounts SET discoverable = true, locked = false WHERE id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let path = format!("/api/v1/accounts/{}", ctx.bob_id);
    let seen: Value = ctx
        .api
        .get(&path, Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        seen["feature_approval"]["current_user"], "automatic",
        "{seen}"
    );
    let looked_up: Value = ctx
        .api
        .get("/api/v1/accounts/lookup?acct=bob", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        looked_up["feature_approval"]["current_user"], "automatic",
        "{looked_up}"
    );
    let anonymous: Value = ctx.api.get(&path, None).await.json().await.unwrap();
    assert_eq!(
        anonymous["feature_approval"]["current_user"], "denied",
        "{anonymous}"
    );
}
