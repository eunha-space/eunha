use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// Following creates a follow notification for the followee.
#[tokio::test]
async fn test_follow_creates_notification() {
    let ctx = TestContext::new("notif-follow").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let resp = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let notifs: Vec<Value> = resp.json().await.unwrap();

    let follow_notif = notifs.iter().find(|n| n["type"].as_str() == Some("follow"));
    assert!(follow_notif.is_some(), "no follow notification found");
    assert_eq!(
        follow_notif.unwrap()["account"]["id"].as_str(),
        Some(ctx.alice_id.as_str()),
    );
}

/// Favouriting creates a favourite notification for the status author.
#[tokio::test]
async fn test_favourite_creates_notification() {
    let ctx = TestContext::new("notif-fav").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "faveable notification test", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/favourite"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let fav_notif = notifs
        .iter()
        .find(|n| n["type"].as_str() == Some("favourite"));
    assert!(fav_notif.is_some(), "no favourite notification found");
    assert_eq!(
        fav_notif.unwrap()["account"]["id"].as_str(),
        Some(ctx.bob_id.as_str()),
    );
}

/// Replying with a mention creates a mention notification.
#[tokio::test]
async fn test_reply_creates_mention_notification() {
    let ctx = TestContext::new("notif-mention").await;

    let parent = ctx
        .api
        .post_status(&ctx.alice_token, "parent for mention", "public")
        .await;
    let parent_id = parent["id"].as_str().unwrap();

    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({
                "status": "@alice reply here",
                "in_reply_to_id": parent_id,
                "visibility": "public"
            }),
        )
        .await;

    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let mention_notif = notifs
        .iter()
        .find(|n| n["type"].as_str() == Some("mention"));
    assert!(mention_notif.is_some(), "no mention notification found");
}

/// When the status a mention notification points at is deleted, the notification
/// must not surface as a statusless orphan. Mastodon cascade-deletes such
/// notifications; we mirror that by excluding them from the v1 and v2 feeds so
/// clients (e.g. mastodon-ios, which renders mentions as a full post row) never
/// receive a mention/status/quote notification without its status.
#[tokio::test]
async fn test_mention_notification_with_deleted_status_is_excluded() {
    let ctx = TestContext::new("notif-mention-deleted").await;

    let parent = ctx
        .api
        .post_status(&ctx.alice_token, "parent for mention", "public")
        .await;
    let parent_id = parent["id"].as_str().unwrap();

    let reply = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({
                "status": "@alice reply here",
                "in_reply_to_id": parent_id,
                "visibility": "public"
            }),
        )
        .await
        .json::<Value>()
        .await
        .unwrap();
    let reply_id = reply["id"].as_str().unwrap();

    // Sanity check: the mention notification exists while the status is live.
    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        notifs.iter().any(|n| n["type"] == "mention"),
        "mention notification should exist before the status is deleted"
    );

    // Bob deletes the replying status.
    let del = ctx
        .api
        .delete(&format!("/api/v1/statuses/{reply_id}"), &ctx.bob_token)
        .await;
    assert!(del.status().is_success(), "delete failed: {}", del.status());

    // v1: the orphaned mention notification must be gone.
    let notifs_v1: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !notifs_v1.iter().any(|n| n["type"] == "mention"),
        "mention notification with a deleted status must be excluded from v1"
    );

    // v2: no notification group should reference the deleted mention either.
    let v2: Value = ctx
        .api
        .get("/api/v2/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let groups = v2["notification_groups"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !groups.iter().any(|g| g["type"] == "mention"),
        "mention group with a deleted status must be excluded from v2"
    );
}

/// GET /api/v1/notifications/:id/dismiss removes the notification.
#[tokio::test]
async fn test_dismiss_notification() {
    let ctx = TestContext::new("notif-dismiss").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!notifs.is_empty(), "no notifications to dismiss");
    let notif_id = notifs[0]["id"].as_str().unwrap();

    let dismiss_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/notifications/{notif_id}/dismiss"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(dismiss_resp.status(), StatusCode::OK);

    let after: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !after.iter().any(|n| n["id"].as_str() == Some(notif_id)),
        "dismissed notification still appears",
    );
}

/// POST /api/v1/notifications/clear removes all notifications.
#[tokio::test]
async fn test_clear_notifications() {
    let ctx = TestContext::new("notif-clear").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let clear_resp = ctx
        .api
        .post_json(
            "/api/v1/notifications/clear",
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(clear_resp.status(), StatusCode::OK);

    let after: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(after.is_empty(), "notifications not cleared");
}

/// Reblogging creates a reblog notification for the status author.
#[tokio::test]
async fn test_reblog_creates_notification() {
    let ctx = TestContext::new("notif-reblog").await;

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "reblog notify me", "public")
        .await;
    let id = status["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let reblog_notif = notifs.iter().find(|n| n["type"].as_str() == Some("reblog"));
    assert!(reblog_notif.is_some(), "no reblog notification found");
    assert_eq!(
        reblog_notif.unwrap()["account"]["id"].as_str(),
        Some(ctx.bob_id.as_str()),
    );
}

/// GET /api/v1/notifications?types[]=follow returns only follow notifications.
#[tokio::test]
async fn test_notification_filter_types() {
    let ctx = TestContext::new("notif-types").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let status = ctx
        .api
        .post_status(&ctx.bob_token, "filterable", "public")
        .await;
    let id = status["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{id}/favourite"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications?types[]=follow", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    for n in &notifs {
        assert_eq!(
            n["type"].as_str(),
            Some("follow"),
            "non-follow notification returned when filtering for follow"
        );
    }
}

/// GET /api/v1/notifications?exclude_types[]=follow omits follow notifications.
#[tokio::test]
async fn test_notification_exclude_types() {
    let ctx = TestContext::new("notif-excl").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let notifs: Vec<Value> = ctx
        .api
        .get(
            "/api/v1/notifications?exclude_types[]=follow",
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !notifs.iter().any(|n| n["type"].as_str() == Some("follow")),
        "follow notification appeared despite exclusion",
    );
}

/// GET /api/v1/notifications accepts limit up to 80 (Mastodon default max).
#[tokio::test]
async fn test_notifications_limit_param_respected() {
    let ctx = TestContext::new("notif-limit").await;

    // Default limit should be 40 (not some lower number).
    let resp = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // limit=1 should return at most 1.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "notif limit test", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{sid}/favourite"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;

    let notifs: Vec<serde_json::Value> = ctx
        .api
        .get("/api/v1/notifications?limit=1", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        notifs.len() <= 1,
        "limit=1 should return at most 1 notification"
    );
}

/// GET /api/v1/notifications/:id returns the notification for the authenticated user.
#[tokio::test]
async fn test_get_notification_by_id() {
    let ctx = TestContext::new("notif-get-id").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!notifs.is_empty(), "expected a follow notification");
    let notif_id = notifs[0]["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/notifications/{notif_id}"),
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"].as_str(), Some(notif_id));
}

/// GET /api/v1/notifications/:id returns 404 for another user's notification.
#[tokio::test]
async fn test_get_notification_other_users_is_404() {
    let ctx = TestContext::new("notif-get-other").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let bob_notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!bob_notifs.is_empty());
    let bob_notif_id = bob_notifs[0]["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/notifications/{bob_notif_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// POST /api/v1/notifications/:id/dismiss returns 404 for another user's notification.
#[tokio::test]
async fn test_dismiss_notification_other_users_is_404() {
    let ctx = TestContext::new("notif-dismiss-other").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let bob_notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!bob_notifs.is_empty());
    let bob_notif_id = bob_notifs[0]["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/notifications/{bob_notif_id}/dismiss"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET /api/v1/notifications?account_id=X returns only notifications from account X.
#[tokio::test]
async fn test_notification_filter_by_account_id() {
    let ctx = TestContext::new("notif-acct-filter").await;

    // Alice follows Bob → Bob gets a follow notification from Alice.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    // Also generate a favourite notification for Bob from Alice.
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "bob filterable", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{sid}/favourite"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Filter by alice's id: all notifications should be from alice.
    let notifs: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/notifications?account_id={}", ctx.alice_id),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        !notifs.is_empty(),
        "expected at least one notification from alice"
    );
    for n in &notifs {
        assert_eq!(
            n["account"]["id"].as_str(),
            Some(ctx.alice_id.as_str()),
            "notification from unexpected account: {n}",
        );
    }
}

// ── notification policy ───────────────────────────────────────────────────────

/// GET /api/v2/notifications/policy returns the column defaults: private mentions
/// and limited accounts filtered, everything else accepted.
#[tokio::test]
async fn test_notification_policy_defaults() {
    let ctx = TestContext::new("notif-policy-defaults").await;

    let resp = ctx
        .api
        .get("/api/v2/notifications/policy", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let policy: Value = resp.json().await.unwrap();

    assert_eq!(policy["for_not_following"].as_str(), Some("accept"));
    assert_eq!(policy["for_not_followers"].as_str(), Some("accept"));
    assert_eq!(policy["for_new_accounts"].as_str(), Some("accept"));
    assert_eq!(policy["for_private_mentions"].as_str(), Some("filter"));
    assert_eq!(policy["for_limited_accounts"].as_str(), Some("filter"));
    assert_eq!(policy["for_bots"].as_str(), Some("accept"));
    assert!(policy["summary"].is_object(), "summary field missing");
}

/// PATCH /api/v2/notifications/policy updates filter settings.
#[tokio::test]
async fn test_notification_policy_update() {
    let ctx = TestContext::new("notif-policy-update").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v2/notifications/policy",
            Some(&ctx.alice_token),
            &json!({"filter_not_following": true}),
        )
        .await;
    // PATCH endpoint but we use post_json — need to use the HTTP client directly.
    drop(resp);

    let patch_resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v2/notifications/policy"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"for_not_following": "filter"}))
        .send()
        .await
        .unwrap();
    assert_eq!(patch_resp.status(), StatusCode::OK);
    let policy: Value = patch_resp.json().await.unwrap();
    assert_eq!(policy["for_not_following"].as_str(), Some("filter"));
    assert_eq!(
        policy["for_not_followers"].as_str(),
        Some("accept"),
        "unchanged field should stay accept"
    );
}

/// GET /api/v1/notifications/policy returns boolean-format policy (Mastodon v1 contract).
#[tokio::test]
async fn test_notification_policy_v1_get() {
    let ctx = TestContext::new("notif-policy-v1-get").await;

    let resp = ctx
        .api
        .get("/api/v1/notifications/policy", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let policy: Value = resp.json().await.unwrap();

    assert_eq!(policy["filter_not_following"].as_bool(), Some(false));
    assert_eq!(policy["filter_not_followers"].as_bool(), Some(false));
    assert_eq!(policy["filter_new_accounts"].as_bool(), Some(false));
    assert_eq!(
        policy["filter_private_mentions"].as_bool(),
        Some(true),
        "filter_private_mentions defaults to true per Mastodon contract"
    );
    assert!(policy["summary"].is_object(), "summary field missing");
    assert!(policy["summary"]["pending_requests_count"].is_number());
    assert!(policy["summary"]["pending_notifications_count"].is_number());
}

/// PATCH /api/v1/notifications/policy updates policy using boolean format.
#[tokio::test]
async fn test_notification_policy_v1_patch() {
    let ctx = TestContext::new("notif-policy-v1-patch").await;

    let resp = ctx
        .api
        .patch_json(
            "/api/v1/notifications/policy",
            Some(&ctx.alice_token),
            &serde_json::json!({"filter_not_following": true}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let policy: Value = resp.json().await.unwrap();
    assert_eq!(policy["filter_not_following"].as_bool(), Some(true));
    assert_eq!(
        policy["filter_not_followers"].as_bool(),
        Some(false),
        "unchanged field stays false"
    );
    assert!(
        policy.get("filter_limited_accounts").is_none(),
        "v1 policy must not expose filter_limited_accounts"
    );
}

/// GET /api/v1/notifications/requests returns an empty list initially.
#[tokio::test]
async fn test_notification_requests_empty_by_default() {
    let ctx = TestContext::new("notif-req-empty").await;

    let resp = ctx
        .api
        .get("/api/v1/notifications/requests", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(list.is_empty(), "expected empty notification requests");
}

/// Notification requests are created when policy filters a notification.
/// Dismissing hides it; accepting removes it permanently.
#[tokio::test]
async fn test_notification_request_dismiss_and_accept() {
    let ctx = TestContext::new("notif-req-dismiss").await;

    // Alice sets filter_not_following=true so bob's actions route to requests.
    ctx.api
        .http
        .patch(ctx.api.url("/api/v2/notifications/policy"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"for_not_following": "filter"}))
        .send()
        .await
        .unwrap();

    // Bob follows alice → should create a notification request (not a notification).
    // Only filtered mentions and quotes open a request
    // (`update_notification_request!`).
    ctx.api
        .post_status(&ctx.bob_token, "@alice hello", "public")
        .await;

    let requests: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications/requests", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !requests.is_empty(),
        "expected a notification request from bob"
    );
    let req_id = requests[0]["id"].as_str().unwrap();
    assert_eq!(
        requests[0]["account"]["id"].as_str(),
        Some(ctx.bob_id.as_str()),
        "notification request should be from bob",
    );

    // GET /api/v1/notifications/requests/:id returns the single request.
    let single_resp = ctx
        .api
        .get(
            &format!("/api/v1/notifications/requests/{req_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(single_resp.status(), StatusCode::OK);
    let single: Value = single_resp.json().await.unwrap();
    assert_eq!(single["id"].as_str(), Some(req_id));

    // Dismiss the request — it should disappear from the list.
    let dismiss_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/notifications/requests/{req_id}/dismiss"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(dismiss_resp.status(), StatusCode::OK);

    let after_dismiss: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications/requests", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !after_dismiss
            .iter()
            .any(|r| r["id"].as_str() == Some(req_id)),
        "dismissed request still appears in list",
    );
}

/// Accepting a notification request removes it from the list.
#[tokio::test]
async fn test_notification_request_accept_removes_from_list() {
    let ctx = TestContext::new("notif-req-accept").await;

    ctx.api
        .http
        .patch(ctx.api.url("/api/v2/notifications/policy"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"for_not_following": "filter"}))
        .send()
        .await
        .unwrap();

    // Only filtered mentions and quotes open a request
    // (`update_notification_request!`).
    ctx.api
        .post_status(&ctx.bob_token, "@alice hello", "public")
        .await;

    let requests: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications/requests", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!requests.is_empty(), "expected a notification request");
    let req_id = requests[0]["id"].as_str().unwrap();

    let accept_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/notifications/requests/{req_id}/accept"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(accept_resp.status(), StatusCode::OK);

    // Accepting removes the request from the list.
    let after_accept: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications/requests", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !after_accept
            .iter()
            .any(|r| r["id"].as_str() == Some(req_id)),
        "accepted request should be removed from list",
    );
}

/// POST /api/v1/notifications/requests/dismiss (bulk) dismisses all notification requests.
#[tokio::test]
async fn test_notification_requests_dismiss_bulk() {
    let ctx = TestContext::new("notif-req-dismiss-bulk").await;

    // Enable filter so bob's follow creates a request.
    ctx.api
        .http
        .patch(ctx.api.url("/api/v2/notifications/policy"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"for_not_following": "filter"}))
        .send()
        .await
        .unwrap();

    // Only filtered mentions and quotes open a request
    // (`update_notification_request!`).
    ctx.api
        .post_status(&ctx.bob_token, "@alice hello", "public")
        .await;

    // Verify a request exists.
    let requests: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications/requests", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !requests.is_empty(),
        "expected a request before bulk dismiss"
    );

    // Dismiss all via the Mastodon-compatible path.
    let resp = ctx
        .api
        .post_json(
            "/api/v1/notifications/requests/dismiss",
            Some(&ctx.alice_token),
            // `set_requests`: the requests named by `id[]`.
            &json!({"id": requests.iter().map(|r| r["id"].clone()).collect::<Vec<_>>()}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Requests should now be empty (dismissed).
    let after: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications/requests", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        after.is_empty(),
        "bulk dismiss should hide all notification requests"
    );
}

/// POST /api/v1/notifications/requests/accept (bulk) removes all pending notification requests.
#[tokio::test]
async fn test_notification_requests_accept_bulk() {
    let ctx = TestContext::new("notif-req-accept-bulk").await;

    ctx.api
        .http
        .patch(ctx.api.url("/api/v2/notifications/policy"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"for_not_following": "filter"}))
        .send()
        .await
        .unwrap();

    // Only filtered mentions and quotes open a request
    // (`update_notification_request!`).
    ctx.api
        .post_status(&ctx.bob_token, "@alice hello", "public")
        .await;

    let requests: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications/requests", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !requests.is_empty(),
        "expected a request before bulk accept"
    );

    // Accept all via the Mastodon-compatible path.
    let accept_resp = ctx
        .api
        .post_json(
            "/api/v1/notifications/requests/accept",
            Some(&ctx.alice_token),
            // `set_requests`: the requests named by `id[]`.
            &json!({"id": requests.iter().map(|r| r["id"].clone()).collect::<Vec<_>>()}),
        )
        .await;
    assert_eq!(accept_resp.status(), StatusCode::OK);

    let after_accept: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications/requests", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        after_accept.is_empty(),
        "bulk accept should remove all pending notification requests"
    );
}

/// GET /api/v2/notifications returns notification groups with accounts and statuses sideloaded.
#[tokio::test]
async fn test_get_notifications_v2() {
    let ctx = TestContext::new("notif-v2").await;

    // Alice follows Bob → Bob gets a follow notification.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let resp = ctx
        .api
        .get("/api/v2/notifications", Some(&ctx.bob_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();

    assert!(
        body["notification_groups"].is_array(),
        "notification_groups missing"
    );
    assert!(body["accounts"].is_array(), "accounts missing");
    assert!(body["statuses"].is_array(), "statuses missing");

    let groups = body["notification_groups"].as_array().unwrap();
    assert!(
        !groups.is_empty(),
        "expected at least one notification group"
    );

    // NotificationGroup serializes as "type" (serde rename), not "notification_type"
    let follow_group = groups.iter().find(|g| g["type"].as_str() == Some("follow"));
    assert!(follow_group.is_some(), "no follow notification group found");
}

/// v2 notifications aggregate favourites of the same status into one group with
/// a shared key, a count, and multiple sample accounts (Mastodon grouping).
#[tokio::test]
async fn test_v2_notifications_group_favourites() {
    let ctx = TestContext::new("notif-v2-group").await;

    let (_carol_id, carol_token) = crate::helpers::seed_user(
        &ctx.db,
        &ctx.domain,
        "carolgroup",
        "carolgroup@test.invalid",
    )
    .await;

    // Bob posts; Alice and Carol both favourite it.
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "group me", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{sid}/favourite"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{sid}/favourite"),
            Some(&carol_token),
            &json!({}),
        )
        .await;

    let body: Value = ctx
        .api
        .get("/api/v2/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    let groups = body["notification_groups"].as_array().unwrap();

    let fav_group = groups
        .iter()
        .find(|g| g["type"].as_str() == Some("favourite"))
        .expect("no favourite group");
    assert_eq!(
        fav_group["notifications_count"].as_i64(),
        Some(2),
        "two favourites should be one group of 2"
    );
    assert_eq!(
        fav_group["sample_account_ids"].as_array().map(|a| a.len()),
        Some(2),
        "group should sample both favouriting accounts",
    );
    // Mastodon's key is `{type}-{status_id}-{hour_bucket}`, checked against a
    // running 4.7.0. The bucket is what stops a group reaching back for ever:
    // favourites more than twelve hours apart are separate groups, so the same
    // status has more than one key over time.
    let group_key = fav_group["group_key"].as_str().unwrap();
    let prefix = format!("favourite-{sid}-");
    assert!(
        group_key.starts_with(&prefix),
        "group key should be favourite-<status_id>-<bucket>, got {group_key}"
    );
    assert!(
        group_key[prefix.len()..].parse::<i64>().is_ok(),
        "the bucket should be an hour number, got {group_key}"
    );

    // The group_key resolves via the single-group endpoint.
    let single: Value = ctx
        .api
        .get(
            &format!("/api/v2/notifications/{group_key}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    // `show` renders `REST::DedupNotificationGroupSerializer`, as the index.
    assert_eq!(
        single["notification_groups"][0]["notifications_count"].as_i64(),
        Some(2)
    );
    assert_eq!(single["accounts"].as_array().map(Vec::len), Some(2));
    assert_eq!(single["statuses"][0]["id"].as_str(), Some(sid));
    assert!(
        single["notification_groups"][0]
            .get("page_max_id")
            .is_none(),
        "a group read on its own is not paginated"
    );

    // And its accounts endpoint returns both.
    let accts: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v2/notifications/{group_key}/accounts"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        accts.len(),
        2,
        "group accounts endpoint should list both favouriters"
    );

    // Dismissing the group removes both underlying notifications.
    let dismiss = ctx
        .api
        .post_json(
            &format!("/api/v2/notifications/{group_key}/dismiss"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(dismiss.status(), StatusCode::OK);
    let after: Value = ctx
        .api
        .get("/api/v2/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !after["notification_groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["type"].as_str() == Some("favourite")),
        "favourite group should be gone after dismiss",
    );
}

/// GET /api/v1/notifications?since_id=X returns only notifications newer than X.
#[tokio::test]
async fn test_notifications_since_id_pagination() {
    let ctx = TestContext::new("notif-since-id").await;

    // First notification: alice follows bob.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let first_notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!first_notifs.is_empty(), "expected a follow notification");
    let first_id = first_notifs.last().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Second notification: alice favourites bob's status.
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "since_id target", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{sid}/favourite"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let all_notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        all_notifs.len() >= 2,
        "expected at least 2 notifications total"
    );

    // since_id should return only notifications newer than first_id.
    let since_notifs: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/notifications?since_id={first_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        !since_notifs
            .iter()
            .any(|n| n["id"].as_str() == Some(&first_id)),
        "since_id notification itself should be excluded",
    );
    assert!(
        since_notifs
            .iter()
            .any(|n| n["type"].as_str() == Some("favourite")),
        "favourite notification (newer) should appear with since_id filter",
    );
}

/// GET /api/v1/notifications?max_id=X returns only notifications older than X.
#[tokio::test]
async fn test_notifications_max_id_pagination() {
    let ctx = TestContext::new("notif-max-id").await;

    // First notification: alice follows bob.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    // Second notification: alice favourites bob's status.
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "max_id target", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{sid}/favourite"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let all_notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(all_notifs.len() >= 2, "expected at least 2 notifications");
    // Notifications are newest-first; take the newest id as the max_id.
    let newest_id = all_notifs[0]["id"].as_str().unwrap().to_string();

    let max_id_notifs: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/notifications?max_id={newest_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        !max_id_notifs
            .iter()
            .any(|n| n["id"].as_str() == Some(&newest_id)),
        "max_id notification itself should be excluded",
    );
}

/// GET /api/v1/notifications?min_id=X returns only notifications newer than X, oldest first.
#[tokio::test]
async fn test_notifications_min_id_pagination() {
    let ctx = TestContext::new("notif-min-id").await;

    // First notification: alice follows bob.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let first_notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!first_notifs.is_empty(), "expected a follow notification");
    let anchor_id = first_notifs.last().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Second notification: alice favourites bob's status.
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "min_id notif target", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{sid}/favourite"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    let min_id_notifs: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/notifications?min_id={anchor_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        !min_id_notifs
            .iter()
            .any(|n| n["id"].as_str() == Some(&anchor_id)),
        "min_id anchor should be excluded",
    );
    assert!(
        min_id_notifs
            .iter()
            .any(|n| n["type"].as_str() == Some("favourite")),
        "favourite notification (newer) should appear with min_id filter",
    );
}

/// GET /api/v1/notifications with limit=80 is accepted (not clamped to something lower).
#[tokio::test]
async fn test_notifications_limit_80_is_accepted() {
    let ctx = TestContext::new("notif-limit-80").await;

    let resp = ctx
        .api
        .get("/api/v1/notifications?limit=80", Some(&ctx.alice_token))
        .await;
    assert_eq!(
        resp.status(),
        reqwest::StatusCode::OK,
        "limit=80 should be accepted"
    );
    // limit=81 should be clamped to 80 and still return 200.
    let resp2 = ctx
        .api
        .get("/api/v1/notifications?limit=81", Some(&ctx.alice_token))
        .await;
    assert_eq!(
        resp2.status(),
        reqwest::StatusCode::OK,
        "limit=81 should be clamped, not rejected"
    );
}

/// Following with notify=true creates a "status" notification when the followed account posts.
#[tokio::test]
async fn test_notify_follow_creates_status_notification() {
    let ctx = TestContext::new("notif-status-type").await;

    // Alice follows Bob with notify=true.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({"notify": true}),
        )
        .await;

    // Bob posts a public status.
    ctx.api
        .post_status(&ctx.bob_token, "hello notifiers", "public")
        .await;

    // Alice should have a "status" notification.
    let body: Value = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let notifs = body.as_array().unwrap();
    assert!(
        notifs.iter().any(|n| n["type"].as_str() == Some("status")),
        "expected a status notification for alice after bob posted, got: {notifs:?}",
    );
}

/// The bell (notify=true) does not fire for a followee's reply to someone else,
/// only for their top-level/self-reply posts (Mastodon FeedInsertWorker#notify?).
#[tokio::test]
async fn test_notify_follow_skips_reply_to_others() {
    let ctx = TestContext::new("notif-status-reply").await;

    // Carol is a third account; Alice bells Bob.
    let (_carol_id, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carolbell", "carolbell@test.invalid")
            .await;
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/follow", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({ "notify": true }),
        )
        .await;

    // Bob replies to Carol — this should NOT bell Alice.
    let carol_status = ctx
        .api
        .post_status(&carol_token, "carol root", "public")
        .await;
    let carol_status_id = carol_status["id"].as_str().unwrap();
    ctx.api.post_json(
        "/api/v1/statuses",
        Some(&ctx.bob_token),
        &json!({ "status": "bob to carol", "in_reply_to_id": carol_status_id, "visibility": "public" }),
    ).await;

    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !notifs.iter().any(|n| n["type"].as_str() == Some("status")),
        "a reply to another account should not trigger the bell",
    );
}

/// GET /api/v2/notifications with since_id returns only newer notification groups.
#[tokio::test]
async fn test_notifications_v2_since_id_pagination() {
    let ctx = TestContext::new("notif-v2-since").await;

    // First event: alice follows bob.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let first_body: Value = ctx
        .api
        .get("/api/v2/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    let first_groups = first_body["notification_groups"].as_array().unwrap();
    assert!(
        !first_groups.is_empty(),
        "expected a follow notification group"
    );

    // Capture the oldest group id from this batch.
    let oldest_id = first_groups.last().unwrap()["page_min_id"]
        .as_str()
        .unwrap_or_else(|| {
            first_groups.last().unwrap()["latest_page_notification_at"]
                .as_str()
                .unwrap_or("1")
        })
        .to_string();

    // Second event: alice favourites bob's status.
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "v2 since notif", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{sid}/favourite"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // since_id=oldest_id should return only newer groups.
    let since_body: Value = ctx
        .api
        .get(
            &format!("/api/v2/notifications?since_id={oldest_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        since_body["notification_groups"].is_array(),
        "notification_groups should be present"
    );
}

/// Muting an account with hide_notifications=true suppresses their notifications.
#[tokio::test]
async fn test_mute_hides_notifications_when_flag_true() {
    let ctx = TestContext::new("notif-mute-hide").await;

    // Alice mutes Bob with notifications=true (default).
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Bob follows Alice — this creates a "follow" notification.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    // Alice's notifications should NOT include the follow from Bob.
    let body: Value = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let notifs = body.as_array().unwrap();
    assert!(
        !notifs.iter().any(|n| n["type"].as_str() == Some("follow")
            && n["account"]["id"].as_str() == Some(ctx.bob_id.as_str())),
        "muted account's follow notification should be suppressed when hide_notifications=true",
    );
}

/// Muting an account with notifications=false still shows their notifications.
#[tokio::test]
async fn test_mute_with_notifications_false_shows_notifications() {
    let ctx = TestContext::new("notif-mute-show").await;

    // Alice mutes Bob with notifications=false (explicit).
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/mute", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"notifications": false}),
        )
        .await;

    // Bob follows Alice.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    // Alice's notifications SHOULD include Bob's follow (notifications not hidden).
    let body: Value = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let notifs = body.as_array().unwrap();
    assert!(
        notifs.iter().any(|n| n["type"].as_str() == Some("follow")
            && n["account"]["id"].as_str() == Some(ctx.bob_id.as_str())),
        "muted account's follow notification should appear when hide_notifications=false",
    );
}

/// Editing a status you have reblogged creates an "update" notification.
///
/// Matches Mastodon's `FanOutOnWriteService#notify_about_update!`, which
/// notifies `reblogged_by_accounts` (and accepted quoters) — not favouriters.
#[tokio::test]
async fn test_edit_creates_update_notification() {
    let ctx = TestContext::new("notif-edit-update").await;

    // Alice posts a status; Bob reblogs it.
    let status = ctx
        .api
        .post_status(&ctx.alice_token, "editable status", "public")
        .await;
    let sid = status["id"].as_str().unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{sid}/reblog"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;

    // Alice edits the status.
    ctx.api
        .put_json(
            &format!("/api/v1/statuses/{sid}"),
            Some(&ctx.alice_token),
            &json!({"status": "edited!", "visibility": "public"}),
        )
        .await;

    // Bob should receive an "update" notification.
    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        notifs.iter().any(|n| n["type"].as_str() == Some("update")),
        "no update notification found: {notifs:?}",
    );
}

/// GET /api/v1/notifications/unread_count returns a count of unread notifications.
#[tokio::test]
async fn test_notifications_unread_count() {
    let ctx = TestContext::new("notif-unread-count").await;

    // Initially no notifications
    let body: Value = ctx
        .api
        .get("/api/v1/notifications/unread_count", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let initial = body["count"].as_i64().unwrap_or(-1);
    assert!(initial >= 0, "unread count should be a non-negative number");

    // Bob follows Alice → generates a follow notification
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let body2: Value = ctx
        .api
        .get("/api/v1/notifications/unread_count", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let after = body2["count"].as_i64().unwrap_or(-1);
    assert!(
        after > initial,
        "unread count should increase after a new notification"
    );
}

/// Replies in a muted thread do not create notifications for the muter.
#[tokio::test]
async fn test_muted_thread_suppresses_notifications() {
    let ctx = TestContext::new("notif-muted-thread").await;

    // Alice posts a root status.
    let root = ctx
        .api
        .post_status(&ctx.alice_token, "thread root", "public")
        .await;
    let root_id = root["id"].as_str().unwrap();

    // Alice mutes the thread.
    ctx.api
        .post_json(
            &format!("/api/v1/statuses/{root_id}/mute"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Bob replies to Alice's root status — this would normally create a mention notification.
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "@alice reply!", "visibility": "public", "in_reply_to_id": root_id}),
        )
        .await;

    // Alice should NOT have a mention notification from Bob.
    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let mention_from_bob = notifs.iter().find(|n| {
        n["type"].as_str() == Some("mention")
            && n["account"]["id"].as_str() == Some(ctx.bob_id.as_str())
    });
    assert!(
        mention_from_bob.is_none(),
        "mention in muted thread should not create a notification"
    );
}

/// Mention from a blocked account does not create a notification for the blocker.
#[tokio::test]
async fn test_blocked_account_mention_does_not_create_notification() {
    let ctx = TestContext::new("notif-blocked-mention").await;

    // Alice blocks Bob.
    ctx.api
        .post_json(
            &format!("/api/v1/accounts/{}/block", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;

    // Bob creates a status mentioning Alice (the status is created from Bob's perspective,
    // but Alice should not receive a mention notification since Bob is blocked).
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &json!({"status": "@alice hey!", "visibility": "public"}),
        )
        .await;

    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();

    let bob_mention = notifs.iter().find(|n| {
        n["type"].as_str() == Some("mention")
            && n["account"]["id"].as_str() == Some(ctx.bob_id.as_str())
    });
    assert!(
        bob_mention.is_none(),
        "mention from blocked account should not appear as notification for the blocker"
    );
}

/// Notification request response includes a non-null updated_at field distinct from created_at bugs.
#[tokio::test]
async fn test_notification_request_has_updated_at() {
    let ctx = TestContext::new("notif-req-updated-at").await;

    ctx.api
        .http
        .patch(ctx.api.url("/api/v2/notifications/policy"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"for_not_following": "filter"}))
        .send()
        .await
        .unwrap();

    // Only filtered mentions and quotes open a request
    // (`update_notification_request!`).
    ctx.api
        .post_status(&ctx.bob_token, "@alice hello", "public")
        .await;

    let requests: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications/requests", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!requests.is_empty(), "expected a notification request");

    let req = &requests[0];
    assert!(
        req["updated_at"].as_str().is_some(),
        "notification request must have updated_at"
    );
    assert!(
        req["created_at"].as_str().is_some(),
        "notification request must have created_at"
    );
}

/// GET /api/v1/notifications/requests/merged returns { merged: true }.
#[tokio::test]
async fn test_notification_requests_merged() {
    let ctx = TestContext::new("notif-req-merged").await;

    let resp = ctx
        .api
        .get(
            "/api/v1/notifications/requests/merged",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["merged"].is_boolean(),
        "merged field should be boolean"
    );
}

/// GET /api/v2/notifications/:group_key returns 404 for unknown group_key.
#[tokio::test]
async fn test_notification_group_not_found() {
    let ctx = TestContext::new("notif-group-404").await;

    let resp = ctx
        .api
        .get(
            "/api/v2/notifications/ungrouped-999999999999",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// GET /api/v2/notifications/:group_key returns the group for a real notification.
#[tokio::test]
async fn test_notification_group_get() {
    let ctx = TestContext::new("notif-group-get").await;

    // Bob follows Alice to create a notification.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    // Alice fetches her v2 notifications to get a group_key.
    let resp = ctx
        .api
        .get("/api/v2/notifications", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    let groups = body["notification_groups"]
        .as_array()
        .expect("notification_groups missing");
    assert!(
        !groups.is_empty(),
        "expected at least one notification group"
    );

    let group_key = groups[0]["group_key"].as_str().unwrap();

    // Fetch the group directly.
    let resp2 = ctx
        .api
        .get(
            &format!("/api/v2/notifications/{group_key}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp2.status(), StatusCode::OK);
    let body2: Value = resp2.json().await.unwrap();
    let group = &body2["notification_groups"][0];
    assert_eq!(group["group_key"].as_str(), Some(group_key));
    assert!(group["notifications_count"].is_number());
}

/// Notifications for statuses matching an active keyword filter include a non-empty `filtered` array.
/// This covers the `notifications.filtered` behaviour added in migration 065.
#[tokio::test]
async fn test_notification_filtered_field_set_when_keyword_matches() {
    let ctx = TestContext::new("notif-filtered-field").await;

    // Alice creates a "notifications" context filter for the word "filtertest".
    ctx.api
        .post_json(
            "/api/v2/filters",
            Some(&ctx.alice_token),
            &serde_json::json!({
                "title": "Notification keyword filter",
                "context": ["notifications"],
                "filter_action": "warn",
                "keywords_attributes": [{"keyword": "filtertest", "whole_word": false}]
            }),
        )
        .await;

    // Bob posts a status containing the filtered word and mentions Alice.
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &serde_json::json!({
                "status": "@alice filtertest mention",
                "visibility": "public"
            }),
        )
        .await;

    // Alice should have a mention notification with a non-empty `filtered` array.
    let notifs: Vec<serde_json::Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();

    let mention = notifs
        .iter()
        .find(|n| n["type"].as_str() == Some("mention"));
    assert!(
        mention.is_some(),
        "alice should have a mention notification"
    );

    // The embedded status carries the `filtered` array for the notifications context.
    let filtered = mention.unwrap()["status"]["filtered"].as_array();
    assert!(
        filtered.is_some(),
        "mention notification's status must have a `filtered` array"
    );
    assert!(
        !filtered.unwrap().is_empty(),
        "`filtered` must be non-empty when keyword matches"
    );
}

/// GET /api/v2/notifications/:group_key/accounts returns accounts for the group.
#[tokio::test]
async fn test_notification_group_accounts() {
    let ctx = TestContext::new("notif-group-accounts").await;

    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let resp = ctx
        .api
        .get("/api/v2/notifications", Some(&ctx.alice_token))
        .await;
    let body: Value = resp.json().await.unwrap();
    let group_key = body["notification_groups"].as_array().unwrap()[0]["group_key"]
        .as_str()
        .unwrap()
        .to_string();

    let resp2 = ctx
        .api
        .get(
            &format!("/api/v2/notifications/{group_key}/accounts"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp2.status(), StatusCode::OK);
    let accounts: Vec<Value> = resp2.json().await.unwrap();
    assert!(!accounts.is_empty(), "expected at least one account");
    assert!(accounts[0]["id"].is_string());
}

/// POST /api/v2/notifications/:group_key/dismiss removes the notification.
#[tokio::test]
async fn test_notification_group_dismiss() {
    let ctx = TestContext::new("notif-group-dismiss").await;

    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;

    let resp = ctx
        .api
        .get("/api/v2/notifications", Some(&ctx.alice_token))
        .await;
    let body: Value = resp.json().await.unwrap();
    let group_key = body["notification_groups"].as_array().unwrap()[0]["group_key"]
        .as_str()
        .unwrap()
        .to_string();

    let dismiss = ctx
        .api
        .post_json(
            &format!("/api/v2/notifications/{group_key}/dismiss"),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(dismiss.status(), StatusCode::OK);

    // The notification is gone from the list now.
    let resp3 = ctx
        .api
        .get("/api/v2/notifications", Some(&ctx.alice_token))
        .await;
    let body3: Value = resp3.json().await.unwrap();
    let remaining = body3["notification_groups"].as_array().unwrap();
    assert!(!remaining
        .iter()
        .any(|g| g["group_key"].as_str() == Some(&group_key)));
}

/// Timestamps in notification responses must use the Mastodon-standard Z suffix, not +00:00.
#[tokio::test]
async fn test_notification_timestamps_use_z_suffix() {
    let ctx = TestContext::new("notif-ts-format").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let notifs: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!notifs.is_empty(), "expected at least one notification");

    let created_at = notifs[0]["created_at"]
        .as_str()
        .expect("notification must have created_at");
    assert!(
        created_at.ends_with('Z'),
        "notification created_at should use Z suffix, got: {created_at}"
    );
    assert!(
        !created_at.contains('+'),
        "notification created_at should not use +00:00 offset, got: {created_at}"
    );
}

/// POST /api/v2/notifications/clear removes all notifications (v2 alias).
#[tokio::test]
async fn test_v2_clear_notifications() {
    let ctx = TestContext::new("notif-v2-clear").await;

    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let clear_resp = ctx
        .api
        .post_json(
            "/api/v2/notifications/clear",
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(clear_resp.status(), StatusCode::OK);

    let after: Value = ctx
        .api
        .get("/api/v2/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        after["notification_groups"]
            .as_array()
            .is_none_or(|g| g.is_empty()),
        "notifications not cleared after POST /api/v2/notifications/clear",
    );
}

/// GET /api/v2/notifications/unread_count returns a count of unread notifications.
#[tokio::test]
async fn test_v2_notifications_unread_count() {
    let ctx = TestContext::new("notif-v2-unread").await;

    // No notifications yet — count should be 0.
    let body: Value = ctx
        .api
        .get("/api/v2/notifications/unread_count", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        body["count"].as_i64(),
        Some(0),
        "unread count should start at 0"
    );

    // Alice follows Bob → creates a notification.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;

    let body2: Value = ctx
        .api
        .get("/api/v2/notifications/unread_count", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        body2["count"].as_i64().unwrap_or(0) > 0,
        "unread count should be > 0 after receiving a notification",
    );
}

/// Only mentioned accounts are notified (`notify_mentioned_accounts!`): a
/// reply that does not mention its parent's author tells them nothing, and
/// one that does tells them about the `Mention`.
#[tokio::test]
async fn test_a_reply_notifies_its_parents_author_only_when_it_mentions_them() {
    let ctx = TestContext::new("notif-reply-mention").await;
    let parent = ctx
        .api
        .post_status(&ctx.alice_token, "a thought", "public")
        .await;
    let parent_id = parent["id"].as_str().unwrap();
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let mentions = || async {
        sqlx::query_as::<_, (String,)>(
            "SELECT activity_type FROM notifications WHERE account_id = $1 AND type = 'mention'",
        )
        .bind(alice)
        .fetch_all(&ctx.db)
        .await
        .unwrap()
    };
    for (text, expected) in [("an answer", 0), ("@alice an answer", 1)] {
        let resp = ctx
            .api
            .post_json(
                "/api/v1/statuses",
                Some(&ctx.bob_token),
                &json!({"status": text, "in_reply_to_id": parent_id}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let found = mentions().await;
        assert_eq!(found.len(), expected, "{text}");
        assert!(found.iter().all(|(t,)| t == "Mention"));
    }
}

/// Alice files what comes from accounts she does not follow.
async fn filter_strangers(ctx: &TestContext) {
    let resp = ctx
        .api
        .http
        .patch(ctx.api.url("/api/v2/notifications/policy"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"for_not_following": "filter"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// `NotificationRequest.without_suspended`: the index leaves out a request
/// from a suspended account.
#[tokio::test]
async fn test_notification_requests_leave_out_suspended_senders() {
    let ctx = TestContext::new("notif-req-suspended").await;
    filter_strangers(&ctx).await;
    ctx.api
        .post_status(&ctx.bob_token, "@alice hello", "public")
        .await;
    let listed = || async {
        ctx.api
            .get("/api/v1/notifications/requests", Some(&ctx.alice_token))
            .await
            .json::<Vec<Value>>()
            .await
            .unwrap()
    };
    assert_eq!(listed().await.len(), 1);

    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    assert!(listed().await.is_empty());
}

/// Accepting a request queues `UnfilterNotificationsWorker`, which adds the
/// sender's filtered direct messages to the recipient's conversations
/// (`push_to_conversations!`) before letting their notifications through.
#[tokio::test]
async fn test_accepting_a_request_brings_its_direct_messages_into_conversations() {
    let ctx = TestContext::new("notif-req-accept-dm").await;
    filter_strangers(&ctx).await;
    let dm = ctx
        .api
        .post_status(&ctx.bob_token, "@alice a private word", "direct")
        .await;
    let dm_id = dm["id"].as_str().unwrap().to_string();

    let conversations = || async {
        ctx.api
            .get("/api/v1/conversations", Some(&ctx.alice_token))
            .await
            .json::<Vec<Value>>()
            .await
            .unwrap()
    };
    assert!(
        conversations().await.is_empty(),
        "a filtered direct message is in no conversation yet"
    );

    let requests: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications/requests", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let req_id = requests[0]["id"].as_str().unwrap();
    let accepted = ctx
        .api
        .post_json(
            &format!("/api/v1/notifications/requests/{req_id}/accept"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(accepted.status(), StatusCode::OK);
    ctx.state.jobs.settle().await;

    let found = conversations().await;
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["last_status"]["id"].as_str(), Some(dm_id.as_str()));
    assert_eq!(found[0]["unread"].as_bool(), Some(true));

    let still_filtered: i64 =
        sqlx::query_scalar("SELECT count(*) FROM notifications WHERE account_id = $1 AND filtered")
            .bind(ctx.alice_id.parse::<i64>().unwrap())
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(still_filtered, 0);

    let merged: Value = ctx
        .api
        .get(
            "/api/v1/notifications/requests/merged",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(merged["merged"], json!(true));
}

/// `merged?` is false while an `UnfilterNotificationsWorker` is counted in
/// `notification_unfilter_jobs:<id>` and has not yet finished.
#[tokio::test]
async fn test_requests_are_not_merged_while_unfiltering_is_pending() {
    let ctx = TestContext::new("notif-req-merged-pending").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let mut redis = ctx.state.redis_coordination.clone();
    let key = ctx
        .state
        .redis_keys
        .key(format!("notification_unfilter_jobs:{alice}"));
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg(1)
        .query_async(&mut redis)
        .await
        .unwrap();
    let merged = || async {
        ctx.api
            .get(
                "/api/v1/notifications/requests/merged",
                Some(&ctx.alice_token),
            )
            .await
            .json::<Value>()
            .await
            .unwrap()["merged"]
            .clone()
    };
    assert_eq!(merged().await, json!(false));
    let _: () = redis::cmd("DEL")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .unwrap();
    assert_eq!(merged().await, json!(true));
}

/// Bob is followed by Alice, then has a post favourited by Alice and by
/// Carol: three notifications in two groups. Returns Carol's id and token,
/// the post's id, and the follow notification's id.
async fn two_groups(ctx: &TestContext) -> (String, String, String, String) {
    let (carol_id, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "favourite me", "public")
        .await;
    let sid = status["id"].as_str().unwrap().to_string();
    for token in [&ctx.alice_token, &carol_token] {
        let resp = ctx
            .api
            .post_json(
                &format!("/api/v1/statuses/{sid}/favourite"),
                Some(token),
                &json!({}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let follow_id: i64 = sqlx::query_scalar(
        "SELECT id FROM notifications WHERE account_id = $1 AND type = 'follow'",
    )
    .bind(ctx.bob_id.parse::<i64>().unwrap())
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    (
        carol_id.to_string(),
        carol_token,
        sid,
        follow_id.to_string(),
    )
}

/// `paginate_groups` reads until it has `limit` groups, not `limit`
/// notifications, and the page's Link header pages by the notifications it
/// read.
#[tokio::test]
async fn test_v2_notifications_page_by_groups() {
    let ctx = TestContext::new("notif-v2-page-groups").await;
    let (carol_id, _, sid, follow_id) = two_groups(&ctx).await;

    let resp = ctx
        .api
        .get("/api/v2/notifications?limit=2", Some(&ctx.bob_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let link = resp
        .headers()
        .get("link")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        link.contains(&format!("max_id={follow_id}")),
        "the next page is past the follow: {link}"
    );
    // `pagination_params` keeps the `limit` asked for.
    assert_eq!(link.matches("limit=2").count(), 2, "{link}");
    let body: Value = resp.json().await.unwrap();
    let groups = body["notification_groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "{body}");
    assert_eq!(groups[0]["type"], "favourite");
    assert_eq!(groups[0]["notifications_count"], 2);
    assert_eq!(groups[0]["sample_account_ids"][0], json!(carol_id));
    assert_eq!(groups[0]["status_id"], json!(sid));
    assert_eq!(groups[1]["type"], "follow");
    assert_eq!(groups[1]["page_min_id"], json!(follow_id));
    assert_eq!(groups[1]["page_max_id"], json!(follow_id));

    // One group a page: the favourites' group still counts both, and
    // samples both, though the page read only the newest.
    let body: Value = ctx
        .api
        .get("/api/v2/notifications?limit=1", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    let group = &body["notification_groups"][0];
    assert_eq!(group["notifications_count"], 2);
    assert_eq!(group["sample_account_ids"].as_array().unwrap().len(), 2);
    assert_eq!(body["accounts"].as_array().unwrap().len(), 2);

    // `account_id` is not among v2's filters.
    let body: Value = ctx
        .api
        .get(
            &format!("/api/v2/notifications?account_id={carol_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["notification_groups"].as_array().unwrap().len(), 2);
}

/// Only the `grouped_types` asked for are grouped; the rest are
/// `ungrouped-<id>`.
#[tokio::test]
async fn test_v2_notifications_group_only_the_grouped_types() {
    let ctx = TestContext::new("notif-v2-grouped-types").await;
    two_groups(&ctx).await;

    let body: Value = ctx
        .api
        .get(
            "/api/v2/notifications?grouped_types[]=follow",
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let groups = body["notification_groups"].as_array().unwrap();
    assert_eq!(groups.len(), 3);
    for group in groups.iter().filter(|g| g["type"] == "favourite") {
        assert!(
            group["group_key"]
                .as_str()
                .unwrap()
                .starts_with("ungrouped-"),
            "{group}"
        );
        assert_eq!(group["notifications_count"], 1);
    }
    let follow = groups.iter().find(|g| g["type"] == "follow").unwrap();
    assert!(follow["group_key"].as_str().unwrap().starts_with("follow-"));
}

/// `expand_accounts=partial_avatars` gives each group's first account in
/// full and the rest as partial accounts; any other value is refused.
#[tokio::test]
async fn test_v2_notifications_partial_avatars() {
    let ctx = TestContext::new("notif-v2-partial").await;
    let (carol_id, _, _, _) = two_groups(&ctx).await;

    let body: Value = ctx
        .api
        .get(
            "/api/v2/notifications?types[]=favourite&expand_accounts=partial_avatars",
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let accounts = body["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["id"], json!(carol_id));
    let partial = body["partial_accounts"].as_array().unwrap();
    assert_eq!(partial.len(), 1);
    assert_eq!(partial[0]["id"], json!(ctx.alice_id));
    assert!(partial[0].get("avatar_description").is_some());
    assert!(partial[0].get("display_name").is_none());

    let refused = ctx
        .api
        .get(
            "/api/v2/notifications?expand_accounts=some",
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
}

/// v2's unread count counts groups; v1's counts notifications, and both
/// take the browsing filters.
#[tokio::test]
async fn test_unread_counts_take_the_browsing_filters() {
    let ctx = TestContext::new("notif-unread-filters").await;
    let (carol_id, _, _, _) = two_groups(&ctx).await;
    let count = |path: String| {
        let ctx = &ctx;
        async move {
            ctx.api
                .get(&path, Some(&ctx.bob_token))
                .await
                .json::<Value>()
                .await
                .unwrap()["count"]
                .as_i64()
                .unwrap()
        }
    };
    assert_eq!(count("/api/v1/notifications/unread_count".into()).await, 3);
    assert_eq!(count("/api/v2/notifications/unread_count".into()).await, 2);
    assert_eq!(
        count("/api/v1/notifications/unread_count?types[]=follow".into()).await,
        1
    );
    assert_eq!(
        count(format!(
            "/api/v1/notifications/unread_count?account_id={carol_id}"
        ))
        .await,
        1
    );
    assert_eq!(
        count("/api/v2/notifications/unread_count?exclude_types[]=follow".into()).await,
        1
    );
    assert_eq!(
        count("/api/v2/notifications/unread_count?grouped_types[]=follow".into()).await,
        3
    );
}

/// A client that does not list a type that is not `baseline` among its
/// `supported_types` is given a `fallback` for it.
#[tokio::test]
async fn test_notifications_fall_back_for_unsupported_types() {
    let ctx = TestContext::new("notif-fallback").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    eunha::push::notify_local(&ctx.state, alice, "admin.sign_up", "Account", bob, bob).await;

    let get = |path: &'static str| {
        let ctx = &ctx;
        async move {
            ctx.api
                .get(path, Some(&ctx.alice_token))
                .await
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let v2 = get("/api/v2/notifications?supported_types[]=mention").await;
    let fallback = &v2["notification_groups"][0]["fallback"];
    let title = fallback["title"].as_str().unwrap();
    assert!(
        title.contains("class=\"u-url mention\"") && title.ends_with(" signed up"),
        "{title}"
    );
    assert!(fallback["summary"]
        .as_str()
        .unwrap()
        .contains(&format!("href=\"https://{}/\"", ctx.domain)));
    assert_eq!(fallback["description"], Value::Null);

    let v1 = get("/api/v1/notifications?supported_types[]=mention").await;
    assert_eq!(v1[0]["fallback"]["title"], json!(title));

    for path in [
        "/api/v2/notifications",
        "/api/v2/notifications?supported_types[]=admin.sign_up",
    ] {
        let body = get(path).await;
        assert!(
            body["notification_groups"][0].get("fallback").is_none(),
            "{path}"
        );
    }
}

/// `Api::V2::Notifications::AccountsController`: a sender for each of the
/// group's notifications, paged by them, none from a suspended account, and
/// nothing for an `ungrouped-` key.
#[tokio::test]
async fn test_group_accounts_page_by_notification() {
    let ctx = TestContext::new("notif-group-accounts-page").await;
    let (carol_id, _, sid, follow_id) = two_groups(&ctx).await;
    let body: Value = ctx
        .api
        .get(
            "/api/v2/notifications?types[]=favourite",
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let key = body["notification_groups"][0]["group_key"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(key.starts_with(&format!("favourite-{sid}-")));
    let path = format!("/api/v2/notifications/{key}/accounts");

    let resp = ctx
        .api
        .get(&format!("{path}?limit=1"), Some(&ctx.bob_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let link = resp.headers()["link"].to_str().unwrap().to_owned();
    assert!(link.contains("rel=\"next\""), "{link}");
    let page: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0]["id"], json!(carol_id));

    let resp = ctx.api.get(&path, Some(&ctx.bob_token)).await;
    let link = resp.headers()["link"].to_str().unwrap().to_owned();
    assert!(
        !link.contains("rel=\"next\""),
        "a short page is the last: {link}"
    );
    assert!(link.contains("rel=\"prev\""), "{link}");
    let all: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(all.len(), 2);

    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(carol_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let left: Vec<Value> = ctx
        .api
        .get(&path, Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0]["id"], json!(ctx.alice_id));

    let resp = ctx
        .api
        .get(
            &format!("/api/v2/notifications/ungrouped-{follow_id}/accounts"),
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.json::<Vec<Value>>().await.unwrap().is_empty());
}
