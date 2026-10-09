use reqwest::StatusCode;

use crate::helpers::TestContext;

/// GET /api/v1/domain_blocks is empty initially.
#[tokio::test]
async fn test_domain_blocks_empty_initially() {
    let ctx = TestContext::new("dblk-empty").await;

    let resp = ctx
        .api
        .get("/api/v1/domain_blocks", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let body: Vec<String> = resp.json().await.unwrap();
    assert!(body.is_empty());
}

/// GET /api/v1/domain_blocks requires authentication.
#[tokio::test]
async fn test_domain_blocks_requires_auth() {
    let ctx = TestContext::new("dblk-unauth").await;

    let resp = ctx.api.get("/api/v1/domain_blocks", None).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// POST /api/v1/domain_blocks adds a domain; GET returns it.
#[tokio::test]
async fn test_domain_blocks_add_and_list() {
    let ctx = TestContext::new("dblk-add").await;

    let post_resp = ctx
        .api
        .post_json(
            "/api/v1/domain_blocks",
            Some(&ctx.alice_token),
            &serde_json::json!({"domain": "evil.example"}),
        )
        .await;
    assert_eq!(post_resp.status(), StatusCode::OK);

    let resp = ctx
        .api
        .get("/api/v1/domain_blocks", Some(&ctx.alice_token))
        .await;
    let body: Vec<String> = resp.json().await.unwrap();
    assert!(
        body.contains(&"evil.example".to_string()),
        "blocked domain not listed"
    );
}

/// `AccountDomainBlock` normalizes and validates the domain, so a blank or
/// malformed one is a 422 and a mixed-case one is stored as its host.
#[tokio::test]
async fn test_domain_blocks_validate_the_domain() {
    let ctx = TestContext::new("dblk-validate").await;
    for domain in ["", "  ", "not a domain", "under_score.example"] {
        let resp = ctx
            .api
            .post_json(
                "/api/v1/domain_blocks",
                Some(&ctx.alice_token),
                &serde_json::json!({ "domain": domain }),
            )
            .await;
        assert_eq!(
            resp.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{domain:?}"
        );
    }

    let resp = ctx
        .api
        .post_json(
            "/api/v1/domain_blocks",
            Some(&ctx.alice_token),
            &serde_json::json!({ "domain": " Evil.EXAMPLE/ " }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Vec<String> = ctx
        .api
        .get("/api/v1/domain_blocks", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body, vec!["evil.example".to_string()]);
}

/// Blocking a domain clears the blocker's notifications and pending follow
/// requests from that domain (Mastodon AfterBlockDomainFromAccountService).
#[tokio::test]
async fn test_domain_block_clears_notifications_and_requests() {
    let ctx = TestContext::new("dblk-cleanup").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();

    // A remote account on evil.example.
    let carol_id = eunha::snowflake::next_id();
    sqlx::query!(
        r#"INSERT INTO accounts
             (id, username, display_name, note, domain, url, uri, public_key,
              inbox_url, outbox_url, shared_inbox_url, discoverable, id_scheme, protocol, created_at, updated_at)
           VALUES ($1,'carol','carol','', 'evil.example',
                   'https://evil.example/carol','https://evil.example/users/carol','k',
                   'https://evil.example/users/carol/inbox','https://evil.example/users/carol/outbox','',
                   true, 0, 1, now(), now())"#,
        carol_id,
    ).execute(&ctx.db).await.unwrap();

    // A notification and a pending follow request from carol to alice.
    sqlx::query!(
        r#"INSERT INTO notifications (id, activity_id, activity_type, account_id, from_account_id, "type", created_at, updated_at)
           VALUES ($1, $2, 'Follow', $3, $4, 'follow', now(), now())"#,
        eunha::snowflake::next_id(), eunha::snowflake::next_id(), alice_id, carol_id,
    ).execute(&ctx.db).await.unwrap();
    sqlx::query!(
        "INSERT INTO follow_requests (account_id, target_account_id, created_at, updated_at) VALUES ($1, $2, now(), now())",
        carol_id, alice_id,
    ).execute(&ctx.db).await.unwrap();

    // Alice blocks the domain.
    let resp = ctx
        .api
        .post_json(
            "/api/v1/domain_blocks",
            Some(&ctx.alice_token),
            &serde_json::json!({ "domain": "evil.example" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let carol = carol_id.to_string();
    let notifs: Vec<serde_json::Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !notifs
            .iter()
            .any(|n| n["account"]["id"].as_str() == Some(carol.as_str())),
        "notification from blocked domain should be cleared",
    );
    let reqs: Vec<serde_json::Value> = ctx
        .api
        .get("/api/v1/follow_requests", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        reqs.is_empty(),
        "pending follow request from blocked domain should be removed"
    );
}

/// POST is idempotent — blocking an already-blocked domain returns 200.
#[tokio::test]
async fn test_domain_blocks_idempotent() {
    let ctx = TestContext::new("dblk-idem").await;

    for _ in 0..2 {
        let resp = ctx
            .api
            .post_json(
                "/api/v1/domain_blocks",
                Some(&ctx.alice_token),
                &serde_json::json!({"domain": "spam.example"}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let list: Vec<String> = ctx
        .api
        .get("/api/v1/domain_blocks", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        list.iter().filter(|d| d.as_str() == "spam.example").count(),
        1
    );
}

/// DELETE /api/v1/domain_blocks removes the domain from the list.
#[tokio::test]
async fn test_domain_blocks_delete() {
    let ctx = TestContext::new("dblk-del").await;

    ctx.api
        .post_json(
            "/api/v1/domain_blocks",
            Some(&ctx.alice_token),
            &serde_json::json!({"domain": "gone.example"}),
        )
        .await;

    let del_resp = ctx
        .api
        .delete_json(
            "/api/v1/domain_blocks",
            &ctx.alice_token,
            &serde_json::json!({"domain": "gone.example"}),
        )
        .await;
    assert_eq!(del_resp.status(), StatusCode::OK);

    let list: Vec<String> = ctx
        .api
        .get("/api/v1/domain_blocks", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !list.contains(&"gone.example".to_string()),
        "domain still blocked after delete"
    );
}

/// DELETE of a non-blocked domain returns 200 (idempotent).
#[tokio::test]
async fn test_domain_blocks_delete_nonexistent_ok() {
    let ctx = TestContext::new("dblk-del-nx").await;

    let resp = ctx
        .api
        .delete_json(
            "/api/v1/domain_blocks",
            &ctx.alice_token,
            &serde_json::json!({"domain": "notblocked.example"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}
