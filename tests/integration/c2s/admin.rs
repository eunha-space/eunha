use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// Elevate alice to admin role for tests that need admin privileges.
async fn make_admin(ctx: &TestContext) {
    let alice_uuid: i64 = ctx.alice_id.parse().unwrap();
    crate::helpers::make_admin(&ctx.db, alice_uuid).await;
}

/// Non-admin token gets 403 from admin endpoints.
#[tokio::test]
async fn test_admin_requires_admin_role() {
    let ctx = TestContext::new("admin-403").await;

    let resp = ctx
        .api
        .get("/api/v1/admin/accounts", Some(&ctx.alice_token))
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "non-admin should get 403"
    );
}

/// A Moderator role (position < 100, with manage_reports + manage_users but no
/// administrator flag) can access the report and account moderation endpoints
/// but is denied admin-only endpoints like domain blocks — matching Mastodon's
/// permission-based authorization.
#[tokio::test]
async fn test_moderator_permission_scoping() {
    let ctx = TestContext::new("admin-moderator").await;
    let alice_uuid: i64 = ctx.alice_id.parse().unwrap();

    // Moderator: position 10, permissions = manage_reports (1<<4) | manage_users (1<<10).
    let role_id = sqlx::query_scalar!(
        r#"INSERT INTO user_roles (id, name, position, permissions, highlighted, created_at, updated_at)
           VALUES ($1, 'Moderator', 10, $2, true, now(), now())
           RETURNING id"#,
        eunha::snowflake::next_id(),
        (1_i64 << 4) | (1_i64 << 10),
    ).fetch_one(&ctx.db).await.unwrap();
    sqlx::query!(
        "UPDATE users SET role_id = $1 WHERE account_id = $2",
        role_id,
        alice_uuid
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    crate::helpers::grant_admin_scopes(&ctx.db, alice_uuid).await;

    // Allowed: reports (manage_reports) and account list (manage_users).
    assert_eq!(
        ctx.api
            .get("/api/v1/admin/reports", Some(&ctx.alice_token))
            .await
            .status(),
        StatusCode::OK,
        "moderator should access reports",
    );
    assert_eq!(
        ctx.api
            .get("/api/v1/admin/accounts", Some(&ctx.alice_token))
            .await
            .status(),
        StatusCode::OK,
        "moderator should access account moderation",
    );

    // Denied: domain blocks require manage_federation / admin.
    assert_eq!(
        ctx.api
            .get("/api/v1/admin/domain_blocks", Some(&ctx.alice_token))
            .await
            .status(),
        StatusCode::FORBIDDEN,
        "moderator must not access admin-only domain blocks",
    );
}

/// GET /api/v1/admin/accounts returns all accounts in the instance.
#[tokio::test]
async fn test_admin_list_accounts() {
    let ctx = TestContext::new("admin-list").await;
    make_admin(&ctx).await;

    let resp = ctx
        .api
        .get("/api/v1/admin/accounts", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(
        !list.is_empty(),
        "expected at least alice and bob in admin accounts"
    );
    // All entries should have an id and username.
    for acc in &list {
        assert!(
            acc["id"].as_str().is_some(),
            "admin account missing id: {acc}"
        );
        assert!(
            acc["username"].as_str().is_some(),
            "admin account missing username: {acc}"
        );
    }
}

/// GET /api/v1/admin/accounts?username=alice returns only alice.
#[tokio::test]
async fn test_admin_list_accounts_filter_by_username() {
    let ctx = TestContext::new("admin-list-user").await;
    make_admin(&ctx).await;

    let resp = ctx
        .api
        .get(
            "/api/v1/admin/accounts?username=alice",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(!list.is_empty(), "expected alice");
    for acc in &list {
        assert_eq!(
            acc["username"].as_str(),
            Some("alice"),
            "non-alice account in filtered results"
        );
    }
}

/// GET /api/v1/admin/accounts?limit=1 returns at most 1 account.
#[tokio::test]
async fn test_admin_list_accounts_limit() {
    let ctx = TestContext::new("admin-list-limit").await;
    make_admin(&ctx).await;

    let resp = ctx
        .api
        .get("/api/v1/admin/accounts?limit=1", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert!(
        list.len() <= 1,
        "limit=1 should return at most 1 account, got {}",
        list.len()
    );
}

/// GET /api/v1/admin/accounts/:id returns a specific account.
#[tokio::test]
async fn test_admin_get_account() {
    let ctx = TestContext::new("admin-get").await;
    make_admin(&ctx).await;

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/admin/accounts/{}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let acc: Value = resp.json().await.unwrap();
    assert_eq!(acc["id"].as_str(), Some(ctx.bob_id.as_str()));
    assert_eq!(acc["username"].as_str(), Some("bob"));
    assert!(acc["account"].is_object(), "nested account object missing");
}

/// A silence action and unsilence toggle silenced state.
#[tokio::test]
async fn test_admin_silence_and_unsilence() {
    let ctx = TestContext::new("admin-silence").await;
    make_admin(&ctx).await;

    let silence_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({"type": "silence"}),
        )
        .await;
    assert_eq!(silence_resp.status(), StatusCode::OK);
    let silenced: Value = ctx
        .api
        .get(
            &format!("/api/v1/admin/accounts/{}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        silenced["silenced"].as_bool(),
        Some(true),
        "silenced should be true after silence"
    );

    let unsilence_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/unsilence", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(unsilence_resp.status(), StatusCode::OK);
    let unsilenced: Value = unsilence_resp.json().await.unwrap();
    assert_eq!(
        unsilenced["silenced"].as_bool(),
        Some(false),
        "silenced should be false after unsilence"
    );
}

/// A suspend action and unsuspend toggle suspended state.
#[tokio::test]
async fn test_admin_suspend_and_unsuspend() {
    let ctx = TestContext::new("admin-suspend").await;
    make_admin(&ctx).await;

    let suspend_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({"type": "suspend"}),
        )
        .await;
    assert_eq!(suspend_resp.status(), StatusCode::OK);
    let suspended: Value = ctx
        .api
        .get(
            &format!("/api/v1/admin/accounts/{}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(suspended["suspended"].as_bool(), Some(true));

    let unsuspend_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/unsuspend", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(unsuspend_resp.status(), StatusCode::OK);
    let unsuspended: Value = unsuspend_resp.json().await.unwrap();
    assert_eq!(unsuspended["suspended"].as_bool(), Some(false));
}

/// `Account#suspend!` records a deletion request (what makes the suspension
/// reversible, and what the 30-day scheduler acts on) and blocks the email;
/// `#unsuspend!` takes both back.
#[tokio::test]
async fn test_admin_suspend_records_deletion_request() {
    let ctx = TestContext::new("admin-suspend-req").await;
    make_admin(&ctx).await;
    let bob_account_id: i64 = ctx.bob_id.parse().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({"type": "suspend"}),
        )
        .await;

    let requests: i64 =
        sqlx::query_scalar("SELECT count(*) FROM account_deletion_requests WHERE account_id = $1")
            .bind(bob_account_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(requests, 1, "suspension should record a deletion request");
    let blocks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM canonical_email_blocks WHERE reference_account_id = $1",
    )
    .bind(bob_account_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(blocks, 1, "suspension should block the email");

    ctx.api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/unsuspend", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;

    let requests: i64 =
        sqlx::query_scalar("SELECT count(*) FROM account_deletion_requests WHERE account_id = $1")
            .bind(bob_account_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(requests, 0, "unsuspending should drop the deletion request");
    let blocks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM canonical_email_blocks WHERE reference_account_id = $1",
    )
    .bind(bob_account_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(blocks, 0, "unsuspending should drop the email block");
}

/// DELETE /api/v1/admin/accounts/:id mirrors `AccountPolicy#destroy?`: only an
/// account whose suspension is still reversible can be purged.
#[tokio::test]
async fn test_admin_delete_account_requires_suspension() {
    let ctx = TestContext::new("admin-del-403").await;
    make_admin(&ctx).await;

    let resp = ctx
        .api
        .http
        .delete(
            ctx.api
                .url(&format!("/api/v1/admin/accounts/{}", ctx.bob_id)),
        )
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "deleting a live account should be refused"
    );
}

/// `Admin::AccountDeletionWorker`: purge the data of a suspended account while
/// keeping both records (`reserve_username: true, reserve_email: true`).
#[tokio::test]
async fn test_admin_delete_account_purges_suspended_account() {
    let ctx = TestContext::new("admin-del-purge").await;
    make_admin(&ctx).await;
    let bob_account_id: i64 = ctx.bob_id.parse().unwrap();

    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &serde_json::json!({"status": "before the ban"}),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({"type": "suspend"}),
        )
        .await;

    let resp = ctx
        .api
        .http
        .delete(
            ctx.api
                .url(&format!("/api/v1/admin/accounts/{}", ctx.bob_id)),
        )
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let statuses: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses WHERE account_id = $1")
        .bind(bob_account_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(statuses, 0, "statuses should be purged");

    // Both records are reserved; the user is disabled rather than destroyed.
    let disabled: bool = sqlx::query_scalar("SELECT disabled FROM users WHERE account_id = $1")
        .bind(bob_account_id)
        .fetch_one(&ctx.db)
        .await
        .expect("user record should be reserved");
    assert!(disabled, "user should be disabled");
    let account_exists: Option<i64> = sqlx::query_scalar("SELECT id FROM accounts WHERE id = $1")
        .bind(bob_account_id)
        .fetch_optional(&ctx.db)
        .await
        .unwrap();
    assert!(
        account_exists.is_some(),
        "account record should be reserved"
    );
}

/// GET /api/v1/admin/reports returns a list (empty when no reports filed).
#[tokio::test]
async fn test_admin_list_reports_empty() {
    let ctx = TestContext::new("admin-reports").await;
    make_admin(&ctx).await;

    let resp = ctx
        .api
        .get("/api/v1/admin/reports", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<Value> = resp.json().await.unwrap();
}

/// Admin can resolve and reopen a report.
#[tokio::test]
async fn test_admin_resolve_and_reopen_report() {
    let ctx = TestContext::new("admin-report-res").await;
    make_admin(&ctx).await;

    // Bob files a report against alice.
    let report_resp = ctx
        .api
        .post_json(
            "/api/v1/reports",
            Some(&ctx.bob_token),
            &json!({
                "account_id": ctx.alice_id,
                "comment": "test report for admin resolve"
            }),
        )
        .await;
    assert_eq!(report_resp.status(), StatusCode::OK);
    let report: Value = report_resp.json().await.unwrap();
    let report_id = report["id"].as_str().expect("report id missing");

    // Admin resolves it.
    let resolve_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/reports/{report_id}/resolve"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resolve_resp.status(), StatusCode::OK);
    let resolved: Value = resolve_resp.json().await.unwrap();
    assert!(
        resolved["action_taken"].as_bool().unwrap_or(false),
        "action_taken should be true after resolve"
    );

    // Reopen it.
    let reopen_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/reports/{report_id}/reopen"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(reopen_resp.status(), StatusCode::OK);
    let reopened: Value = reopen_resp.json().await.unwrap();
    assert!(
        !reopened["action_taken"].as_bool().unwrap_or(true),
        "action_taken should be false after reopen"
    );
}

/// GET /api/v1/admin/reports/:id returns the specific report.
#[tokio::test]
async fn test_admin_get_report() {
    let ctx = TestContext::new("admin-get-report").await;
    make_admin(&ctx).await;

    let report: Value = ctx
        .api
        .post_json(
            "/api/v1/reports",
            Some(&ctx.bob_token),
            &json!({"account_id": ctx.alice_id, "comment": "admin get report test"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let report_id = report["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/admin/reports/{report_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"].as_str(), Some(report_id));
}

/// Admin domain allows: create, list, delete.
#[tokio::test]
async fn test_admin_domain_allows_crud() {
    let ctx = TestContext::new("admin-dallow").await;
    make_admin(&ctx).await;

    let create_resp = ctx
        .api
        .post_json(
            "/api/v1/admin/domain_allows",
            Some(&ctx.alice_token),
            &json!({"domain": "trusted.example.com"}),
        )
        .await;
    assert_eq!(create_resp.status(), StatusCode::OK);
    let allow: Value = create_resp.json().await.unwrap();
    let allow_id = allow["id"].as_str().expect("id missing");
    assert_eq!(allow["domain"].as_str(), Some("trusted.example.com"));

    let list: Vec<Value> = ctx
        .api
        .get("/api/v1/admin/domain_allows", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        list.iter().any(|a| a["id"].as_str() == Some(allow_id)),
        "created allow not in list"
    );

    let del = ctx
        .api
        .delete(
            &format!("/api/v1/admin/domain_allows/{allow_id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(del.status(), StatusCode::OK);

    let after: Vec<Value> = ctx
        .api
        .get("/api/v1/admin/domain_allows", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !after.iter().any(|a| a["id"].as_str() == Some(allow_id)),
        "deleted allow still in list"
    );
}

/// Admin domain blocks: create, list, delete.
#[tokio::test]
async fn test_admin_domain_blocks_crud() {
    let ctx = TestContext::new("admin-dblock").await;
    make_admin(&ctx).await;

    let create_resp = ctx
        .api
        .post_json(
            "/api/v1/admin/domain_blocks",
            Some(&ctx.alice_token),
            &json!({
                "domain": "spam.example.com",
                "severity": "silence",
                "reject_media": true
            }),
        )
        .await;
    assert_eq!(create_resp.status(), StatusCode::OK);
    let block: Value = create_resp.json().await.unwrap();
    let block_id = block["id"].as_str().expect("id missing");
    assert_eq!(block["domain"].as_str(), Some("spam.example.com"));
    assert_eq!(block["severity"].as_str(), Some("silence"));
    assert_eq!(block["reject_media"].as_bool(), Some(true));
    // `DomainBlock#severity`: `{ silence: 0, suspend: 1, noop: 2 }`.
    let stored: Option<i32> =
        sqlx::query_scalar("SELECT severity FROM domain_blocks WHERE id = $1")
            .bind(block_id.parse::<i64>().unwrap())
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(stored, Some(0));

    let list: Vec<Value> = ctx
        .api
        .get("/api/v1/admin/domain_blocks", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        list.iter().any(|b| b["id"].as_str() == Some(block_id)),
        "created block not in list"
    );

    let del = ctx
        .api
        .delete(
            &format!("/api/v1/admin/domain_blocks/{block_id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(del.status(), StatusCode::OK);

    let after: Vec<Value> = ctx
        .api
        .get("/api/v1/admin/domain_blocks", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !after.iter().any(|b| b["id"].as_str() == Some(block_id)),
        "deleted block still in list"
    );
}

/// POST /api/v1/admin/accounts/:id/approve approves a pending account, and
/// `UserPolicy#approve?` refuses one already approved.
#[tokio::test]
async fn test_admin_approve_account() {
    let ctx = TestContext::new("admin-approve").await;
    make_admin(&ctx).await;
    sqlx::query("UPDATE users SET approved = false WHERE account_id = $1")
        .bind(ctx.bob_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/approve", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let acc: Value = resp.json().await.unwrap();
    assert_eq!(acc["id"].as_str(), Some(ctx.bob_id.as_str()));
    assert_eq!(acc["approved"].as_bool(), Some(true));

    let again = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/approve", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(again.status(), StatusCode::FORBIDDEN);
}

/// A disable action freezes the login (`users.disabled`) without suspending
/// the account, and POST /api/v1/admin/accounts/:id/enable undoes it.
#[tokio::test]
async fn test_admin_enable_account() {
    let ctx = TestContext::new("admin-enable").await;
    make_admin(&ctx).await;

    let disable = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"type": "disable"}),
        )
        .await;
    assert_eq!(disable.status(), StatusCode::OK);
    let disabled: Value = ctx
        .api
        .get(
            &format!("/api/v1/admin/accounts/{}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(disabled["disabled"].as_bool(), Some(true));
    assert_eq!(disabled["suspended"].as_bool(), Some(false));
    let requests: i64 =
        sqlx::query_scalar("SELECT count(*) FROM account_deletion_requests WHERE account_id = $1")
            .bind(ctx.bob_id.parse::<i64>().unwrap())
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(requests, 0, "disabling is not suspending");

    let enable_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/enable", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(enable_resp.status(), StatusCode::OK);
    let acc: Value = enable_resp.json().await.unwrap();
    assert_eq!(acc["disabled"].as_bool(), Some(false));
}

/// POST /api/v1/admin/measures returns an array of measure objects.
#[tokio::test]
async fn test_admin_measures() {
    let ctx = TestContext::new("admin-measures").await;
    make_admin(&ctx).await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/admin/measures",
            Some(&ctx.alice_token),
            &json!({
                "keys": ["new_users", "active_users"],
                "start_at": "2020-01-01T00:00:00Z",
                "end_at": "2099-01-01T00:00:00Z"
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let measures: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(measures.len(), 2, "expected 2 measure objects");
    for m in &measures {
        assert!(m["key"].as_str().is_some(), "measure missing key: {m}");
        assert!(m["total"].as_str().is_some(), "measure missing total: {m}");
    }
}

/// POST /api/v1/admin/dimensions returns an array of dimension objects.
#[tokio::test]
async fn test_admin_dimensions() {
    let ctx = TestContext::new("admin-dimensions").await;
    make_admin(&ctx).await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/admin/dimensions",
            Some(&ctx.alice_token),
            &json!({
                "keys": ["sources"],
                "start_at": "2020-01-01T00:00:00Z",
                "end_at": "2099-01-01T00:00:00Z"
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let dims: Vec<Value> = resp.json().await.unwrap();
    // Even with no data, should return an array.
    for d in &dims {
        assert!(d["key"].as_str().is_some(), "dimension missing key: {d}");
    }
}

/// POST /api/v1/admin/retention returns an array (empty when no cohorts in range).
#[tokio::test]
async fn test_admin_retention() {
    let ctx = TestContext::new("admin-retention").await;
    make_admin(&ctx).await;

    // Use a past date range that predates any test accounts to get an empty array quickly.
    let resp = ctx
        .api
        .post_json(
            "/api/v1/admin/retention",
            Some(&ctx.alice_token),
            &json!({
                "start_at": "2015-01-01T00:00:00Z",
                "end_at": "2020-01-01T00:00:00Z",
                "frequency": "month"
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let result: Vec<Value> = resp.json().await.unwrap();
    // No accounts were created in that range so the cohort array is empty.
    assert!(result.is_empty() || result.iter().all(|c| c["period"].is_string()));
}

/// Admin IP blocks: create, list, get, delete.
#[tokio::test]
async fn test_admin_ip_blocks_crud() {
    let ctx = TestContext::new("admin-ipblock").await;
    make_admin(&ctx).await;

    let create_resp = ctx
        .api
        .post_json(
            "/api/v1/admin/ip_blocks",
            Some(&ctx.alice_token),
            &json!({
                "ip": "192.0.2.1",
                "severity": "sign_up_block",
                "comment": "test ip block"
            }),
        )
        .await;
    assert_eq!(create_resp.status(), StatusCode::OK);
    let block: Value = create_resp.json().await.unwrap();
    let block_id = block["id"].as_str().expect("id missing");
    assert_eq!(block["ip"].as_str(), Some("192.0.2.1"));
    assert_eq!(block["severity"].as_str(), Some("sign_up_block"));

    // List.
    let list: Vec<Value> = ctx
        .api
        .get("/api/v1/admin/ip_blocks", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(list.iter().any(|b| b["id"].as_str() == Some(block_id)));

    // Get single.
    let get_resp = ctx
        .api
        .get(
            &format!("/api/v1/admin/ip_blocks/{block_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(get_resp.status(), StatusCode::OK);
    let got: Value = get_resp.json().await.unwrap();
    assert_eq!(got["id"].as_str(), Some(block_id));

    // Delete.
    let del = ctx
        .api
        .delete(
            &format!("/api/v1/admin/ip_blocks/{block_id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(del.status(), StatusCode::OK);
}

/// Admin email domain blocks: create, list, get, delete.
#[tokio::test]
async fn test_admin_email_domain_blocks_crud() {
    let ctx = TestContext::new("admin-edblock").await;
    make_admin(&ctx).await;

    let create_resp = ctx
        .api
        .post_json(
            "/api/v1/admin/email_domain_blocks",
            Some(&ctx.alice_token),
            &json!({"domain": "spam-email.example.com"}),
        )
        .await;
    assert_eq!(create_resp.status(), StatusCode::OK);
    let block: Value = create_resp.json().await.unwrap();
    let block_id = block["id"].as_str().expect("id missing");
    assert_eq!(block["domain"].as_str(), Some("spam-email.example.com"));

    let list: Vec<Value> = ctx
        .api
        .get("/api/v1/admin/email_domain_blocks", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(list.iter().any(|b| b["id"].as_str() == Some(block_id)));

    let get_resp = ctx
        .api
        .get(
            &format!("/api/v1/admin/email_domain_blocks/{block_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(get_resp.status(), StatusCode::OK);

    let del = ctx
        .api
        .delete(
            &format!("/api/v1/admin/email_domain_blocks/{block_id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(del.status(), StatusCode::OK);
}

/// PATCH /api/v1/admin/ip_blocks/:id updates an existing IP block.
#[tokio::test]
async fn test_admin_update_ip_block() {
    let ctx = TestContext::new("admin-ipblock-upd").await;
    make_admin(&ctx).await;

    let block: Value = ctx
        .api
        .post_json(
            "/api/v1/admin/ip_blocks",
            Some(&ctx.alice_token),
            &json!({"ip": "192.0.2.99", "severity": "sign_up_block"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let block_id = block["id"].as_str().unwrap();

    let update_resp = ctx
        .api
        .patch_json(
            &format!("/api/v1/admin/ip_blocks/{block_id}"),
            Some(&ctx.alice_token),
            &json!({"ip": "192.0.2.99", "severity": "no_access", "comment": "updated"}),
        )
        .await;
    assert_eq!(update_resp.status(), StatusCode::OK);
    let updated: Value = update_resp.json().await.unwrap();
    assert_eq!(updated["severity"].as_str(), Some("no_access"));
    assert_eq!(updated["comment"].as_str(), Some("updated"));

    // Clean up.
    ctx.api
        .delete(
            &format!("/api/v1/admin/ip_blocks/{block_id}"),
            &ctx.alice_token,
        )
        .await;
}

/// GET /api/v1/admin/custom_emojis returns a JSON array (may be empty).
#[tokio::test]
async fn test_admin_list_custom_emojis() {
    let ctx = TestContext::new("admin-emojis").await;
    make_admin(&ctx).await;

    let resp = ctx
        .api
        .get("/api/v1/admin/custom_emojis", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let _: Vec<Value> = resp.json().await.unwrap();
}

/// GET /api/v1/admin/accounts?suspended=true returns only suspended accounts.
#[tokio::test]
async fn test_admin_list_accounts_filter_by_status() {
    let ctx = TestContext::new("admin-status-filter").await;
    make_admin(&ctx).await;

    // Before suspension, suspended=true should not include bob.
    let before: Vec<Value> = ctx
        .api
        .get(
            "/api/v1/admin/accounts?suspended=true",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        before
            .iter()
            .all(|a| a["suspended"].as_bool() != Some(false)),
        "suspended=true should only return suspended accounts",
    );

    // Suspend bob.
    ctx.api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"type": "suspend"}),
        )
        .await;

    // Now bob should appear in suspended=true results.
    let after: Vec<Value> = ctx
        .api
        .get(
            "/api/v1/admin/accounts?suspended=true",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        after
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())),
        "bob should appear in suspended=true after suspension",
    );

    // active=true should NOT include bob now.
    let active: Vec<Value> = ctx
        .api
        .get("/api/v1/admin/accounts?active=true", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        active
            .iter()
            .all(|a| a["id"].as_str() != Some(ctx.bob_id.as_str())),
        "suspended bob should not appear in active=true",
    );
}

/// POST /api/v1/admin/accounts/:id/reject deletes a pending signup outright
/// (`DeleteAccountService(reserve_email: false, reserve_username: false)`).
#[tokio::test]
async fn test_admin_reject_account() {
    let ctx = TestContext::new("admin-reject").await;
    make_admin(&ctx).await;
    let bob_account_id: i64 = ctx.bob_id.parse().unwrap();

    // Only a signup awaiting approval can be rejected (`UserPolicy#reject?`).
    sqlx::query("UPDATE users SET approved = false WHERE account_id = $1")
        .bind(bob_account_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/reject", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Neither record survives a rejection.
    let get_resp = ctx
        .api
        .get(
            &format!("/api/v1/admin/accounts/{}", ctx.bob_id),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(
        get_resp.status(),
        StatusCode::NOT_FOUND,
        "rejected account should be gone"
    );
    let user_exists: Option<i64> = sqlx::query_scalar("SELECT id FROM users WHERE account_id = $1")
        .bind(bob_account_id)
        .fetch_optional(&ctx.db)
        .await
        .unwrap();
    assert!(user_exists.is_none(), "rejected user should be gone");
}

/// Suspension hides content instead of deleting it: the statuses stay in the
/// database, become invisible (`StatusPolicy#show?`), and come back on
/// unsuspend. Data only goes when the deletion request comes due.
#[tokio::test]
async fn test_suspension_hides_statuses_and_unsuspend_restores() {
    let ctx = TestContext::new("admin-suspend-hide").await;
    make_admin(&ctx).await;
    let bob_account_id: i64 = ctx.bob_id.parse().unwrap();

    let status = ctx
        .api
        .post_status(&ctx.bob_token, "visible for now", "public")
        .await;
    let status_id = status["id"].as_str().unwrap().to_string();

    ctx.api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({"type": "suspend"}),
        )
        .await;

    // Hidden from readers …
    assert_eq!(
        ctx.api
            .get(
                &format!("/api/v1/statuses/{status_id}"),
                Some(&ctx.alice_token)
            )
            .await
            .status(),
        StatusCode::NOT_FOUND,
        "a suspended author's status should be invisible",
    );
    let public: Vec<Value> = ctx
        .api
        .get("/api/v1/timelines/public", None)
        .await
        .json()
        .await
        .unwrap();
    assert!(
        public.iter().all(|s| s["id"].as_str() != Some(&status_id)),
        "suspended author's status should be out of the public timeline",
    );

    // … but not deleted.
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM statuses WHERE account_id = $1 AND deleted_at IS NULL",
    )
    .bind(bob_account_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(live, 1, "suspension must not delete the account's statuses");

    ctx.api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/unsuspend", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;

    assert_eq!(
        ctx.api
            .get(
                &format!("/api/v1/statuses/{status_id}"),
                Some(&ctx.alice_token)
            )
            .await
            .status(),
        StatusCode::OK,
        "unsuspending should bring the status back",
    );
}

/// A suspended account can't act, but its tokens survive so unsuspending
/// restores access without a new sign-in (`require_not_suspended!`).
#[tokio::test]
async fn test_suspension_blocks_tokens_reversibly() {
    let ctx = TestContext::new("admin-suspend-token").await;
    make_admin(&ctx).await;

    ctx.api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({"type": "suspend"}),
        )
        .await;

    let blocked = ctx
        .api
        .get("/api/v1/accounts/verify_credentials", Some(&ctx.bob_token))
        .await;
    assert!(
        blocked.status() == StatusCode::UNAUTHORIZED || blocked.status() == StatusCode::FORBIDDEN,
        "a suspended account should not be able to act, got {}",
        blocked.status(),
    );
    let revoked: Option<bool> = sqlx::query_scalar(
        "SELECT revoked_at IS NOT NULL FROM oauth_access_tokens WHERE token = $1",
    )
    .bind(&ctx.bob_token)
    .fetch_optional(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        revoked,
        Some(false),
        "the token should be blocked, not revoked",
    );

    ctx.api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/unsuspend", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;

    assert_eq!(
        ctx.api
            .get("/api/v1/accounts/verify_credentials", Some(&ctx.bob_token))
            .await
            .status(),
        StatusCode::OK,
        "the same token should work again after unsuspending",
    );
}

/// `Scheduler::SuspendedUserCleanupScheduler`: a suspension older than
/// `DELAY_TO_DELETION` is purged; a fresh one is left alone.
#[tokio::test]
async fn test_suspended_account_cleanup_after_delay() {
    let ctx = TestContext::new("admin-cleanup").await;
    make_admin(&ctx).await;
    let bob_account_id: i64 = ctx.bob_id.parse().unwrap();

    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.bob_token),
            &serde_json::json!({"status": "still here"}),
        )
        .await;
    ctx.api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &serde_json::json!({"type": "suspend"}),
        )
        .await;

    // A fresh suspension is still reversible, so nothing is purged yet.
    eunha::background::process_deletion_requests(&ctx.state)
        .await
        .unwrap();
    let requests: i64 =
        sqlx::query_scalar("SELECT count(*) FROM account_deletion_requests WHERE account_id = $1")
            .bind(bob_account_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(requests, 1, "a fresh suspension should not be purged");

    // Backdate it past the delay and run the pass again.
    sqlx::query(
        "UPDATE account_deletion_requests SET created_at = now() - interval '31 days' WHERE account_id = $1",
    )
    .bind(bob_account_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    eunha::background::process_deletion_requests(&ctx.state)
        .await
        .unwrap();

    let requests: i64 =
        sqlx::query_scalar("SELECT count(*) FROM account_deletion_requests WHERE account_id = $1")
            .bind(bob_account_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(requests, 0, "the deletion request should be fulfilled");
    let statuses: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses WHERE account_id = $1")
        .bind(bob_account_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(statuses, 0, "the account's content should be purged");
}

/// Rejecting an already-approved account is refused, so `reject` can't be used
/// as a delete button for live accounts.
#[tokio::test]
async fn test_admin_reject_approved_account_is_forbidden() {
    let ctx = TestContext::new("admin-reject-403").await;
    make_admin(&ctx).await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/reject", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// admin/accounts?active=true returns approved, non-suspended accounts.
#[tokio::test]
async fn test_admin_list_accounts_active_includes_approved_users() {
    let ctx = TestContext::new("admin-active-filter").await;
    make_admin(&ctx).await;

    let active: Vec<Value> = ctx
        .api
        .get("/api/v1/admin/accounts?active=true", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();

    // Alice is an approved, non-suspended user: she should appear in active=true.
    assert!(
        active
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.alice_id.as_str())),
        "alice (approved, not suspended) should appear in active=true",
    );
    // Suspended or silenced accounts must not appear.
    for a in &active {
        assert_eq!(
            a["suspended"].as_bool(),
            Some(false),
            "suspended account appeared in active=true: {a}",
        );
    }
}

/// admin/accounts?pending=true returns only unapproved accounts.
#[tokio::test]
async fn test_admin_list_accounts_pending_filter() {
    let ctx = TestContext::new("admin-pending-filter").await;
    make_admin(&ctx).await;

    let alice_uuid: i64 = ctx.alice_id.parse().unwrap();
    let bob_uuid: i64 = ctx.bob_id.parse().unwrap();

    // Set bob's approved_at to NULL to simulate a pending account.
    let db = ctx.db.clone();
    sqlx::query!(
        "UPDATE users SET approved = false WHERE account_id = $1",
        bob_uuid
    )
    .execute(&db)
    .await
    .unwrap();

    let pending: Vec<Value> = ctx
        .api
        .get(
            "/api/v1/admin/accounts?pending=true",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();

    // Bob should appear in pending.
    assert!(
        pending
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.bob_id.as_str())),
        "bob (approved_at=NULL) should appear in pending=true: {pending:?}",
    );
    // Alice (approved) should NOT appear in pending.
    assert!(
        !pending
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.alice_id.as_str())),
        "alice (approved) should not appear in pending=true",
    );
    let _ = alice_uuid; // suppress unused warning
}

/// Admin accounts list is ordered by id DESC so the pagination cursor is consistent.
#[tokio::test]
async fn test_admin_accounts_ordered_by_id_desc() {
    let ctx = TestContext::new("admin-acct-order").await;
    make_admin(&ctx).await;

    let accounts: Vec<serde_json::Value> = ctx
        .api
        .get("/api/v1/admin/accounts", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();

    assert!(
        accounts.len() >= 2,
        "need at least alice and bob to verify ordering"
    );

    let ids: Vec<i64> = accounts
        .iter()
        .filter_map(|a| a["id"].as_str().and_then(|s| s.parse::<i64>().ok()))
        .collect();
    let sorted_desc: Vec<i64> = {
        let mut s = ids.clone();
        s.sort_unstable_by(|a, b| b.cmp(a));
        s
    };
    assert_eq!(
        ids, sorted_desc,
        "admin accounts should be ordered by id DESC"
    );
}

/// GET /api/v1/admin/domain_blocks/:id returns the specific block.
#[tokio::test]
async fn test_admin_get_domain_block() {
    let ctx = TestContext::new("admin-dblock-get").await;
    make_admin(&ctx).await;

    let block: Value = ctx
        .api
        .post_json(
            "/api/v1/admin/domain_blocks",
            Some(&ctx.alice_token),
            &json!({"domain": "gettest.example.com", "severity": "silence"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let block_id = block["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/admin/domain_blocks/{block_id}"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let fetched: Value = resp.json().await.unwrap();
    assert_eq!(fetched["id"].as_str(), Some(block_id));
    assert_eq!(fetched["domain"].as_str(), Some("gettest.example.com"));

    ctx.api
        .delete(
            &format!("/api/v1/admin/domain_blocks/{block_id}"),
            &ctx.alice_token,
        )
        .await;
}

/// PATCH /api/v1/admin/domain_blocks/:id updates an existing block.
#[tokio::test]
async fn test_admin_update_domain_block() {
    let ctx = TestContext::new("admin-dblock-upd").await;
    make_admin(&ctx).await;

    let block: Value = ctx
        .api
        .post_json(
            "/api/v1/admin/domain_blocks",
            Some(&ctx.alice_token),
            &json!({"domain": "patchtest.example.com", "severity": "silence"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let block_id = block["id"].as_str().unwrap();

    let resp = ctx.api.patch_json(
        &format!("/api/v1/admin/domain_blocks/{block_id}"),
        Some(&ctx.alice_token),
        &json!({"domain": "patchtest.example.com", "severity": "suspend", "reject_media": true}),
    ).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let updated: Value = resp.json().await.unwrap();
    assert_eq!(updated["severity"].as_str(), Some("suspend"));
    assert_eq!(updated["reject_media"].as_bool(), Some(true));

    ctx.api
        .delete(
            &format!("/api/v1/admin/domain_blocks/{block_id}"),
            &ctx.alice_token,
        )
        .await;
}

/// GET /api/v1/admin/tags returns a JSON array.
#[tokio::test]
async fn test_admin_list_tags() {
    let ctx = TestContext::new("admin-tags-list").await;
    make_admin(&ctx).await;

    ctx.api
        .post_status(&ctx.alice_token, "Post with #admintag1", "public")
        .await;

    let resp = ctx
        .api
        .get("/api/v1/admin/tags?name=admintag1", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let tags: Vec<Value> = resp.json().await.unwrap();
    assert!(
        tags.iter().any(|t| t["name"].as_str() == Some("admintag1")),
        "created tag should appear in admin tags list: {tags:?}"
    );
    let tag = tags
        .iter()
        .find(|t| t["name"].as_str() == Some("admintag1"))
        .unwrap();
    assert!(tag["id"].as_str().is_some());
    assert!(tag["trendable"].as_bool().is_some());
    assert!(tag["usable"].as_bool().is_some());
    assert!(tag["listable"].as_bool().is_some());
    assert!(tag["requires_review"].as_bool().is_some());
}

/// PATCH /api/v1/admin/tags/:id updates tag moderation settings.
#[tokio::test]
async fn test_admin_update_tag() {
    let ctx = TestContext::new("admin-tags-upd").await;
    make_admin(&ctx).await;

    ctx.api
        .post_status(&ctx.alice_token, "Post with #updatabletag", "public")
        .await;

    let tags: Vec<Value> = ctx
        .api
        .get(
            "/api/v1/admin/tags?name=updatabletag",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    let tag = tags
        .iter()
        .find(|t| t["name"].as_str() == Some("updatabletag"))
        .expect("tag not found in admin list");
    let tag_id = tag["id"].as_str().unwrap();

    let resp = ctx
        .api
        .patch_json(
            &format!("/api/v1/admin/tags/{tag_id}"),
            Some(&ctx.alice_token),
            &json!({"trendable": true, "usable": true, "listable": true}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let updated: Value = resp.json().await.unwrap();
    assert_eq!(updated["trendable"].as_bool(), Some(true));
    assert_eq!(updated["usable"].as_bool(), Some(true));
    assert_eq!(updated["requires_review"].as_bool(), Some(false));
}

// ── GET /api/v2/admin/accounts ────────────────────────────────────────────────

/// GET /api/v2/admin/accounts returns all local accounts.
#[tokio::test]
async fn test_admin_v2_accounts_list() {
    let ctx = TestContext::new("adm-v2-list").await;
    make_admin(&ctx).await;

    let resp = ctx
        .api
        .get("/api/v2/admin/accounts", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let accounts: Vec<Value> = resp.json().await.unwrap();
    assert!(!accounts.is_empty(), "should return at least alice and bob");
    // Each account should have id and username
    for a in &accounts {
        assert!(a["id"].as_str().is_some());
        assert!(a["username"].as_str().is_some());
    }
}

/// origin=local returns only local accounts.
#[tokio::test]
async fn test_admin_v2_accounts_filter_origin_local() {
    let ctx = TestContext::new("adm-v2-local").await;
    make_admin(&ctx).await;

    let resp = ctx
        .api
        .get(
            "/api/v2/admin/accounts?origin=local",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let accounts: Vec<Value> = resp.json().await.unwrap();
    // All returned accounts should be local (no domain)
    for a in &accounts {
        assert!(
            a["domain"].is_null(),
            "expected local account, got domain={:?}",
            a["domain"]
        );
    }
    assert!(accounts
        .iter()
        .any(|a| a["username"].as_str() == Some("alice")));
}

/// origin=remote returns zero accounts when no remote accounts exist.
#[tokio::test]
async fn test_admin_v2_accounts_filter_origin_remote() {
    let ctx = TestContext::new("adm-v2-remote").await;
    make_admin(&ctx).await;

    let resp = ctx
        .api
        .get(
            "/api/v2/admin/accounts?origin=remote",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let accounts: Vec<Value> = resp.json().await.unwrap();
    // No remote accounts seeded in test context
    assert!(
        accounts.is_empty(),
        "no remote accounts expected in test instance"
    );
}

/// display_name filter narrows results.
#[tokio::test]
async fn test_admin_v2_accounts_filter_display_name() {
    let ctx = TestContext::new("adm-v2-dname").await;
    make_admin(&ctx).await;

    // Update alice's display_name
    ctx.api
        .patch_multipart(
            "/api/v1/accounts/update_credentials",
            &ctx.alice_token,
            &[("display_name", "UniqueDisplayNameXYZ")],
        )
        .await;

    let resp = ctx
        .api
        .get(
            "/api/v2/admin/accounts?display_name=UniqueDisplayNameXYZ",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let accounts: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["username"].as_str(), Some("alice"));
}

/// Federation reads `domain_blocks.severity` with Mastodon's integers: a
/// suspend (1) drops the domain, a noop (2) and a silence (0) do not, and the
/// most specific block wins, as `DomainBlock.rule_for` picks it.
#[tokio::test]
async fn test_domain_block_severity_integers() {
    let ctx = TestContext::new("admin-dblock-ints").await;
    sqlx::query(
        "INSERT INTO domain_blocks (domain, severity, created_at, updated_at)
         VALUES ('suspended.test', 1, now(), now()), ('noop.test', 2, now(), now()),
                ('silenced.test', 0, now(), now()), ('ok.suspended.test', 2, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let suspended =
        |uri: &'static str| eunha::federation::moderation::actor_is_suspended(&ctx.state, uri);
    assert!(suspended("https://suspended.test/users/a").await);
    assert!(suspended("https://sub.suspended.test/users/a").await);
    assert!(!suspended("https://ok.suspended.test/users/a").await);
    assert!(!suspended("https://noop.test/users/a").await);
    assert!(!suspended("https://silenced.test/users/a").await);

    let all = eunha::federation::moderation::suspended_domains(&ctx.state).await;
    assert_eq!(all, vec!["suspended.test".to_string()]);
}
