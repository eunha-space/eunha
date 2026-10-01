//! Moderation as Mastodon does it: account actions and the strikes, audit log
//! entries and report resolutions they bring, role policies, the admin report
//! API, and reports arriving from other servers.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn make_admin(ctx: &TestContext) {
    crate::helpers::make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;
}

fn id(s: &str) -> i64 {
    s.parse().unwrap()
}

async fn file_report(ctx: &TestContext, token: &str, body: Value) -> Value {
    let resp = ctx
        .api
        .post_json("/api/v1/reports", Some(token), &body)
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "{:?}", resp.text().await);
    resp.json().await.unwrap()
}

/// A suspend action records a strike, logs itself, resolves every open report
/// about the account, and tells the account with `moderation_warning`.
#[tokio::test]
async fn test_suspend_action_strikes_logs_and_resolves_reports() {
    let ctx = TestContext::new("mod-action").await;
    make_admin(&ctx).await;
    let (_, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;

    let first = file_report(&ctx, &carol_token, json!({"account_id": ctx.bob_id})).await;
    let second = file_report(&ctx, &carol_token, json!({"account_id": ctx.bob_id})).await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"type": "silence", "report_id": first["id"], "text": "be nice"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // `process_strike!`: silence is 3_000, the report's posts cited.
    let strike: (i32, String, Option<i64>, i64) = sqlx::query_as(
        "SELECT action, text, report_id, account_id FROM account_warnings WHERE target_account_id = $1",
    )
    .bind(id(&ctx.bob_id))
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(strike.0, 3_000);
    assert_eq!(strike.1, "be nice");
    assert_eq!(strike.2, Some(id(first["id"].as_str().unwrap())));
    assert_eq!(strike.3, id(&ctx.alice_id));

    // `process_reports!`: a non-`none` action resolves all open reports.
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM reports WHERE target_account_id = $1 AND action_taken_at IS NULL",
    )
    .bind(id(&ctx.bob_id))
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(open, 0);
    let by: Option<i64> =
        sqlx::query_scalar("SELECT action_taken_by_account_id FROM reports WHERE id = $1")
            .bind(id(second["id"].as_str().unwrap()))
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(by, Some(id(&ctx.alice_id)));

    // The audit log: one silence on the account, one resolve per report.
    let logs: Vec<(String, String)> = sqlx::query_as(
        "SELECT action, target_type FROM admin_action_logs WHERE account_id = $1 ORDER BY id",
    )
    .bind(id(&ctx.alice_id))
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        logs,
        vec![
            ("silence".to_string(), "Account".to_string()),
            ("resolve".to_string(), "Report".to_string()),
            ("resolve".to_string(), "Report".to_string()),
        ]
    );

    // `LocalNotificationWorker` for the `moderation_warning`.
    let notifications: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    let warning = notifications
        .iter()
        .find(|n| n["type"] == "moderation_warning")
        .expect("bob should be told about the warning");
    assert_eq!(warning["moderation_warning"]["action"], "silence");
    assert_eq!(warning["moderation_warning"]["text"], "be nice");
    assert_eq!(
        warning["moderation_warning"]["target_account"]["id"],
        ctx.bob_id
    );
}

/// A plain warning resolves only the report it came from, and an unknown type
/// is a validation failure.
#[tokio::test]
async fn test_warning_resolves_only_its_report() {
    let ctx = TestContext::new("mod-warn").await;
    make_admin(&ctx).await;
    let (_, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let first = file_report(&ctx, &carol_token, json!({"account_id": ctx.bob_id})).await;
    file_report(&ctx, &carol_token, json!({"account_id": ctx.bob_id})).await;

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"type": "none", "report_id": first["id"], "send_email_notification": "false"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM reports WHERE target_account_id = $1 AND action_taken_at IS NULL",
    )
    .bind(id(&ctx.bob_id))
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(open, 1);

    let bad = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"type": "banish"}),
        )
        .await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// `role.overrides?`: a moderator acts on accounts below its role only, never
/// on an admin or on itself.
#[tokio::test]
async fn test_moderator_cannot_act_on_higher_roles() {
    let ctx = TestContext::new("mod-overrides").await;
    let moderator_role: i64 = sqlx::query_scalar(
        "INSERT INTO user_roles (name, position, permissions, highlighted, created_at, updated_at)
         VALUES ('Mod', 10, $1, true, now(), now()) RETURNING id",
    )
    .bind((1_i64 << 4) | (1_i64 << 10))
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query("UPDATE users SET role_id = $1 WHERE account_id = $2")
        .bind(moderator_role)
        .bind(id(&ctx.alice_id))
        .execute(&ctx.db)
        .await
        .unwrap();
    crate::helpers::grant_admin_scopes(&ctx.db, id(&ctx.alice_id)).await;
    crate::helpers::make_admin(&ctx.db, id(&ctx.bob_id)).await;

    for target in [&ctx.bob_id, &ctx.alice_id] {
        let resp = ctx
            .api
            .post_json(
                &format!("/api/v1/admin/accounts/{target}/action"),
                Some(&ctx.alice_token),
                &json!({"type": "suspend"}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
    let strikes: i64 = sqlx::query_scalar("SELECT count(*) FROM account_warnings")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(strikes, 0, "a refused action leaves nothing behind");
}

/// `AccountPolicy#unsuspend?`: only a suspension made here is undone here.
#[tokio::test]
async fn test_remote_origin_suspension_cannot_be_unsuspended() {
    let ctx = TestContext::new("mod-unsuspend-origin").await;
    make_admin(&ctx).await;
    sqlx::query("UPDATE accounts SET suspended_at = now(), suspension_origin = 1 WHERE id = $1")
        .bind(id(&ctx.bob_id))
        .execute(&ctx.db)
        .await
        .unwrap();
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/unsuspend", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// Unsuspending removes the deletion request, so the 30-day purge leaves the
/// account alone; and a request left behind for an account no longer
/// unavailable is not acted on (`Admin::AccountDeletionWorker`).
#[tokio::test]
async fn test_purge_spares_accounts_no_longer_unavailable() {
    let ctx = TestContext::new("mod-purge-guard").await;
    sqlx::query(
        "INSERT INTO account_deletion_requests (account_id, created_at, updated_at)
         VALUES ($1, now() - interval '40 days', now())",
    )
    .bind(id(&ctx.bob_id))
    .execute(&ctx.db)
    .await
    .unwrap();
    eunha::background::process_deletion_requests(&ctx.state)
        .await
        .unwrap();
    let username: String = sqlx::query_scalar("SELECT display_name FROM accounts WHERE id = $1")
        .bind(id(&ctx.bob_id))
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(username, "bob", "an available account is not purged");
}

/// The admin report entity carries the reported posts, the cited rules, and
/// the moderators assigned to and resolving it, as admin accounts.
#[tokio::test]
async fn test_admin_report_entity() {
    let ctx = TestContext::new("mod-report-entity").await;
    make_admin(&ctx).await;
    let rule: i64 = sqlx::query_scalar(
        "INSERT INTO rules (text, hint, priority, created_at, updated_at)
         VALUES ('Be kind', '', 0, now(), now()) RETURNING id",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let status = ctx.api.post_status(&ctx.bob_token, "rude", "public").await;
    let (_, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let report = file_report(
        &ctx,
        &carol_token,
        json!({"account_id": ctx.bob_id, "status_ids": [status["id"]], "rule_ids": [rule.to_string()]}),
    )
    .await;
    assert_eq!(report["category"], "violation");
    assert_eq!(report["rule_ids"], json!([rule.to_string()]));
    let rid = report["id"].as_str().unwrap();

    let assigned: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/reports/{rid}/assign_to_self"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(assigned["assigned_account"]["id"], ctx.alice_id);
    assert_eq!(assigned["statuses"][0]["id"], status["id"]);
    assert_eq!(assigned["rules"][0]["text"], "Be kind");
    assert!(
        assigned["target_account"]["email"].is_string(),
        "admin accounts carry the email"
    );

    let resolved: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/reports/{rid}/resolve"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(resolved["action_taken"], true);
    assert_eq!(resolved["action_taken_by_account"]["id"], ctx.alice_id);
}

/// PATCH /api/v1/admin/reports/:id recategorises, rejecting rules on anything
/// but a violation and rules that do not exist.
#[tokio::test]
async fn test_admin_update_report() {
    let ctx = TestContext::new("mod-report-patch").await;
    make_admin(&ctx).await;
    let (_, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let report = file_report(&ctx, &carol_token, json!({"account_id": ctx.bob_id})).await;
    let rid = report["id"].as_str().unwrap();
    let url = format!("/api/v1/admin/reports/{rid}");

    let spam: Value = ctx
        .api
        .patch_json(&url, Some(&ctx.alice_token), &json!({"category": "spam"}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(spam["category"], "spam");

    let bad = ctx
        .api
        .patch_json(
            &url,
            Some(&ctx.alice_token),
            &json!({"category": "violation", "rule_ids": ["999999"]}),
        )
        .await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let bad = ctx
        .api
        .patch_json(
            &url,
            Some(&ctx.alice_token),
            &json!({"category": "other", "rule_ids": ["1"]}),
        )
        .await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Staff hear of a report by `admin.report` only if they may manage reports,
/// and not again while an earlier report about the account is still open.
#[tokio::test]
async fn test_report_notifies_staff_once_per_open_target() {
    let ctx = TestContext::new("mod-report-notify").await;
    make_admin(&ctx).await;
    let (_, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    file_report(&ctx, &carol_token, json!({"account_id": ctx.bob_id})).await;
    file_report(&ctx, &carol_token, json!({"account_id": ctx.bob_id})).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE account_id = $1 AND type = 'admin.report'",
    )
    .bind(id(&ctx.alice_id))
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(count, 1);
    let bob: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE account_id = $1 AND type = 'admin.report'",
    )
    .bind(id(&ctx.bob_id))
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(bob, 0, "someone without manage_reports hears nothing");
}

/// A reporter must be able to see every post it attaches.
#[tokio::test]
async fn test_report_cannot_attach_unseen_posts() {
    let ctx = TestContext::new("mod-report-visibility").await;
    let (_, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let private = ctx
        .api
        .post_status(&ctx.bob_token, "followers only", "private")
        .await;
    let resp = ctx
        .api
        .post_json(
            "/api/v1/reports",
            Some(&carol_token),
            &json!({"account_id": ctx.bob_id, "status_ids": [private["id"]]}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A remote server's `Flag` against a local account, named by its actor URI,
/// becomes a report; from a domain blocked with `reject_reports` it does not.
#[tokio::test]
async fn test_inbound_flag() {
    let ctx = TestContext::new("mod-flag").await;
    let remote_id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, inbox_url, created_at, updated_at)
           VALUES ($1, 'mod', 'remote.invalid', '', '', 'https://remote.invalid/actor',
                   'https://remote.invalid/actor', 'https://remote.invalid/inbox', now(), now())"#,
    )
    .bind(remote_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    let status = ctx
        .api
        .post_status(&ctx.bob_token, "reported", "public")
        .await;
    let bob_uri = format!("https://{}/users/bob", ctx.domain);
    let flag = |id: &str| {
        json!({
            "id": format!("https://remote.invalid/{id}"),
            "type": "Flag",
            "actor": "https://remote.invalid/actor",
            "content": "spam",
            "object": [bob_uri, format!("{bob_uri}/statuses/{}", status["id"].as_str().unwrap())],
        })
    };

    eunha::api::ap::inbox::received(&ctx.state, flag("1"))
        .await
        .unwrap();
    let report: (i64, Vec<i64>, String, Option<String>) = sqlx::query_as(
        "SELECT target_account_id, status_ids, comment, uri FROM reports WHERE account_id = $1",
    )
    .bind(remote_id)
    .fetch_one(&ctx.db)
    .await
    .expect("the Flag should become a report");
    assert_eq!(report.0, id(&ctx.bob_id));
    assert_eq!(report.1, vec![id(status["id"].as_str().unwrap())]);
    assert_eq!(report.2, "spam");
    assert_eq!(report.3.as_deref(), Some("https://remote.invalid/1"));

    sqlx::query(
        "INSERT INTO domain_blocks (domain, severity, reject_reports, created_at, updated_at)
         VALUES ('remote.invalid', 2, true, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    eunha::api::ap::inbox::received(&ctx.state, flag("2"))
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM reports WHERE account_id = $1")
        .bind(remote_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(count, 1, "reject_reports drops the second Flag");
}

/// Reporting a remote account with `forward` sends a `Flag` from the instance
/// actor to its server.
#[tokio::test]
async fn test_forwarded_report_sends_flag() {
    let ctx = TestContext::new("mod-forward").await;
    let remote_id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, inbox_url, created_at, updated_at)
           VALUES ($1, 'troll', 'remote.invalid', '', '', 'https://remote.invalid/users/troll',
                   'https://remote.invalid/users/troll', 'https://remote.invalid/users/troll/inbox', now(), now())"#,
    )
    .bind(remote_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    let report = file_report(
        &ctx,
        &ctx.alice_token,
        json!({"account_id": remote_id.to_string(), "comment": "abuse", "forward": true}),
    )
    .await;
    assert_eq!(report["forwarded"], true);

    let job: (Value, String) = sqlx::query_as(
        r#"SELECT payload->'activity', payload->>'inbox' FROM eunha.ojak_queue
           WHERE payload->'activity'->>'type' = 'Flag'"#,
    )
    .fetch_one(&ctx.db)
    .await
    .expect("a Flag delivery should be enqueued");
    assert_eq!(job.1, "https://remote.invalid/users/troll/inbox");
    assert_eq!(
        job.0["actor"].as_str(),
        Some(format!("https://{}/actor", ctx.domain).as_str())
    );
    assert_eq!(job.0["content"], "abuse");
    assert_eq!(job.0["object"][0], "https://remote.invalid/users/troll");
}

/// A suspended local actor is still served, blanked and marked `suspended`,
/// so other servers learn of it; once its data is gone it is a 410.
#[tokio::test]
async fn test_suspended_actor_is_served_blank() {
    let ctx = TestContext::new("mod-actor").await;
    make_admin(&ctx).await;
    sqlx::query("UPDATE accounts SET display_name = 'Bob B', note = 'hello' WHERE id = $1")
        .bind(id(&ctx.bob_id))
        .execute(&ctx.db)
        .await
        .unwrap();
    ctx.api
        .post_json(
            &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
            Some(&ctx.alice_token),
            &json!({"type": "suspend"}),
        )
        .await;
    let resp = ctx.api.ap_get("/users/bob", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let actor: Value = resp.json().await.unwrap();
    assert_eq!(actor["suspended"], true);
    assert_eq!(actor["name"], "bob");
    assert_eq!(actor["summary"], "");

    sqlx::query("DELETE FROM account_deletion_requests WHERE account_id = $1")
        .bind(id(&ctx.bob_id))
        .execute(&ctx.db)
        .await
        .unwrap();
    let resp = ctx.api.ap_get("/users/bob", None).await;
    assert_eq!(resp.status(), StatusCode::GONE);
}

/// A remote actor whose own server suspends it (`suspended: true` in an
/// `Update`) is suspended here with a remote origin, and unsuspended when its
/// server says so again.
#[tokio::test]
async fn test_remote_suspension_follows_the_actor() {
    let ctx = TestContext::new("mod-remote-suspend").await;
    let remote_id = eunha::snowflake::next_id();
    let uri = "https://remote.invalid/users/zed";
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, inbox_url, created_at, updated_at)
           VALUES ($1, 'zed', 'remote.invalid', 'Zed', '', $2, $2, $2 || '/inbox', now(), now())"#,
    )
    .bind(remote_id)
    .bind(uri)
    .execute(&ctx.db)
    .await
    .unwrap();
    let update = |suspended: bool| {
        json!({
            "id": format!("{uri}#updates/{suspended}"),
            "type": "Update",
            "actor": uri,
            "object": {"id": uri, "type": "Person", "preferredUsername": "zed", "name": "Zed",
                       "inbox": format!("{uri}/inbox"), "suspended": suspended},
        })
    };
    eunha::api::ap::inbox::received(&ctx.state, update(true))
        .await
        .unwrap();
    let state: (bool, Option<i32>) = sqlx::query_as(
        "SELECT suspended_at IS NOT NULL, suspension_origin FROM accounts WHERE id = $1",
    )
    .bind(remote_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(state, (true, Some(1)));

    eunha::api::ap::inbox::received(&ctx.state, update(false))
        .await
        .unwrap();
    let suspended: bool =
        sqlx::query_scalar("SELECT suspended_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(remote_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(!suspended);
}

async fn seed_remote(ctx: &TestContext, username: &str, domain: &str) -> i64 {
    let id = eunha::snowflake::next_id();
    let uri = format!("https://{domain}/users/{username}");
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, inbox_url, created_at, updated_at)
           VALUES ($1, $2, $3, $2, 'bio', $4, $4, $4 || '/inbox', now(), now())"#,
    )
    .bind(id)
    .bind(username)
    .bind(domain)
    .bind(&uri)
    .execute(&ctx.db)
    .await
    .unwrap();
    id
}

/// A suspend domain block suspends and purges the accounts already known from
/// the domain and its subdomains, records the follows it cut, and tells the
/// local accounts that lost them; removing it lifts the suspension.
#[tokio::test]
async fn test_domain_block_suspends_existing_accounts() {
    let ctx = TestContext::new("mod-dblock-suspend").await;
    make_admin(&ctx).await;
    let remote = seed_remote(&ctx, "zed", "evil.test").await;
    let sub = seed_remote(&ctx, "yan", "a.evil.test").await;
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, created_at, updated_at) VALUES ($1, $2, now(), now())",
    )
    .bind(id(&ctx.bob_id))
    .bind(remote)
    .execute(&ctx.db)
    .await
    .unwrap();

    let block: Value = ctx
        .api
        .post_json(
            "/api/v1/admin/domain_blocks",
            Some(&ctx.alice_token),
            &json!({"domain": "Evil.test", "severity": "suspend"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(block["domain"], "evil.test", "the domain is normalized");

    for account in [remote, sub] {
        let (suspended, note): (bool, String) =
            sqlx::query_as("SELECT suspended_at IS NOT NULL, note FROM accounts WHERE id = $1")
                .bind(account)
                .fetch_one(&ctx.db)
                .await
                .unwrap();
        assert!(suspended);
        assert_eq!(note, "", "a suspended domain's accounts are purged");
    }
    let follows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM follows WHERE target_account_id = $1")
            .bind(remote)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(follows, 0);

    let notifications: Vec<Value> = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    let severed = notifications
        .iter()
        .find(|n| n["type"] == "severed_relationships")
        .expect("bob should hear of the follow it lost");
    assert_eq!(severed["event"]["type"], "domain_block");
    assert_eq!(severed["event"]["target_name"], "evil.test");
    assert_eq!(severed["event"]["following_count"], 1);

    // A second block on the same domain is refused, naming the existing one.
    let again = ctx
        .api
        .post_json(
            "/api/v1/admin/domain_blocks",
            Some(&ctx.alice_token),
            &json!({"domain": "evil.test", "severity": "silence"}),
        )
        .await;
    assert_eq!(again.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = again.json().await.unwrap();
    assert_eq!(body["existing_domain_block"]["id"], block["id"]);

    let resp = ctx
        .api
        .delete(
            &format!(
                "/api/v1/admin/domain_blocks/{}",
                block["id"].as_str().unwrap()
            ),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let suspended: bool =
        sqlx::query_scalar("SELECT suspended_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(remote)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(!suspended, "unblocking lifts the suspension it made");

    let logs: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM admin_action_logs WHERE target_type = 'DomainBlock' ORDER BY id",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(logs, vec!["create".to_string(), "destroy".to_string()]);
}

/// A silence block limits the domain's accounts, and changing it to noop lifts
/// the limit it made.
#[tokio::test]
async fn test_domain_block_silence_and_retroactive_update() {
    let ctx = TestContext::new("mod-dblock-silence").await;
    make_admin(&ctx).await;
    let remote = seed_remote(&ctx, "zed", "loud.test").await;
    let block: Value = ctx
        .api
        .post_json(
            "/api/v1/admin/domain_blocks",
            Some(&ctx.alice_token),
            &json!({"domain": "loud.test", "severity": "silence"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let silenced: bool =
        sqlx::query_scalar("SELECT silenced_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(remote)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(silenced);

    ctx.api
        .patch_json(
            &format!(
                "/api/v1/admin/domain_blocks/{}",
                block["id"].as_str().unwrap()
            ),
            Some(&ctx.alice_token),
            &json!({"severity": "noop"}),
        )
        .await;
    let silenced: bool =
        sqlx::query_scalar("SELECT silenced_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(remote)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(!silenced);
}

/// `/api/v1/instance/domain_blocks` is off unless `show_domain_blocks` says
/// otherwise, and lists only silence and suspend blocks.
#[tokio::test]
async fn test_instance_domain_blocks_follow_settings() {
    let ctx = TestContext::new("mod-instance-dblocks").await;
    sqlx::query(
        "INSERT INTO domain_blocks (domain, severity, public_comment, created_at, updated_at)
         VALUES ('a.test', 1, 'spam', now(), now()), ('b.test', 2, 'fine', now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let resp = ctx.api.get("/api/v1/instance/domain_blocks", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    sqlx::query(
        "INSERT INTO settings (var, value, created_at, updated_at)
         VALUES ('show_domain_blocks', '--- all' || chr(10), now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let blocks: Vec<Value> = ctx
        .api
        .get("/api/v1/instance/domain_blocks", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["domain"], "a.test");
    assert_eq!(blocks[0]["severity"], "suspend");
    assert!(
        blocks[0]["comment"].is_null(),
        "the rationale is off by default"
    );
}

/// An account first seen from a silenced domain starts out limited.
#[tokio::test]
async fn test_new_account_from_blocked_domain_starts_limited() {
    let ctx = TestContext::new("mod-dblock-new").await;
    sqlx::query(
        "INSERT INTO domain_blocks (domain, severity, created_at, updated_at) VALUES ('quiet.test', 0, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let actor = json!({
        "id": "https://quiet.test/users/newbie",
        "type": "Person",
        "preferredUsername": "newbie",
        "inbox": "https://quiet.test/users/newbie/inbox",
    });
    let id = eunha::api::ap::inbox::resolve_or_fetch_remote_account_prefetched(
        &ctx.state,
        "https://quiet.test/users/newbie",
        actor,
    )
    .await
    .unwrap();
    let silenced: bool =
        sqlx::query_scalar("SELECT silenced_at IS NOT NULL FROM accounts WHERE id = $1")
            .bind(id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(silenced);
}

/// Sign up from `ip`, returning the response, and confirm the sign-up when it
/// was taken.
async fn sign_up(ctx: &TestContext, username: &str, email: &str, ip: &str) -> StatusCode {
    let resp = ctx
        .api
        .http
        .post(ctx.api.url("/api/v1/accounts"))
        .header("host", &ctx.api.host)
        .header("x-forwarded-for", ip)
        .json(&json!({
            "username": username,
            "email": email,
            "password": "a-long-enough-password",
            "agreement": true,
            "reason": "hello",
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    if status == StatusCode::OK {
        let token: String = sqlx::query_scalar(
            "SELECT confirmation_token FROM eunha.pending_signups WHERE username = $1",
        )
        .bind(username)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
        ctx.api
            .get(&format!("/auth/confirm?token={token}"), None)
            .await;
    }
    status
}

/// IP blocks, email domain blocks, canonical email blocks and username blocks
/// all apply at sign-up: refusing it, or sending it to the approval queue.
#[tokio::test]
async fn test_sign_up_blocks() {
    let ctx = TestContext::new("mod-signup-blocks").await;
    sqlx::query(
        "INSERT INTO ip_blocks (ip, severity, comment, created_at, updated_at)
         VALUES ('203.0.113.0/24', 5500, '', now(), now()), ('198.51.100.7', 5000, '', now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO email_domain_blocks (domain, allow_with_approval, created_at, updated_at)
         VALUES ('spam.test', false, now(), now()), ('maybe.test', true, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO username_blocks (username, normalized_username, exact, allow_with_approval, created_at, updated_at)
         VALUES ('admin', 'admin', false, false, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    use sha2::{Digest, Sha256};
    let hash = hex::encode(Sha256::digest(b"banned@ok.test"));
    sqlx::query(
        "INSERT INTO canonical_email_blocks (canonical_email_hash, created_at, updated_at) VALUES ($1, now(), now())",
    )
    .bind(hash)
    .execute(&ctx.db)
    .await
    .unwrap();

    let ok = "192.0.2.10";
    assert_eq!(
        sign_up(&ctx, "ipblocked", "a@ok.test", "203.0.113.9").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        sign_up(&ctx, "dom", "a@mail.spam.test", ok).await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        sign_up(&ctx, "canon", "Ban.ned+x@ok.test", ok).await,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        sign_up(&ctx, "the_4dm1n", "b@ok.test", ok).await,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    // Allowed in, but to the approval queue.
    assert_eq!(
        sign_up(&ctx, "fromip", "c@ok.test", "198.51.100.7").await,
        StatusCode::OK
    );
    assert_eq!(
        sign_up(&ctx, "fromdom", "d@maybe.test", ok).await,
        StatusCode::OK
    );
    assert_eq!(
        sign_up(&ctx, "plain", "e@ok.test", ok).await,
        StatusCode::OK
    );
    let approved: Vec<(String, bool, Option<String>)> = sqlx::query_as(
        "SELECT a.username, u.approved, host(u.sign_up_ip) FROM users u JOIN accounts a ON a.id = u.account_id
         WHERE a.username IN ('fromip', 'fromdom', 'plain') ORDER BY a.username",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        approved,
        vec![
            ("fromdom".to_string(), false, Some(ok.to_string())),
            (
                "fromip".to_string(),
                false,
                Some("198.51.100.7".to_string())
            ),
            ("plain".to_string(), true, Some(ok.to_string())),
        ]
    );
    // The reason given becomes the invite request.
    let reason: String = sqlx::query_scalar(
        "SELECT r.text FROM user_invite_requests r JOIN users u ON u.id = r.user_id
         JOIN accounts a ON a.id = u.account_id WHERE a.username = 'fromip'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(reason, "hello");
}

/// A `no_access` IP block answers everything from the address with 403.
#[tokio::test]
async fn test_no_access_ip_block() {
    let ctx = TestContext::new("mod-no-access").await;
    sqlx::query(
        "INSERT INTO ip_blocks (ip, severity, comment, created_at, updated_at)
         VALUES ('203.0.113.66', 9999, '', now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let get = |ip: &'static str| {
        ctx.api
            .http
            .get(ctx.api.url("/api/v1/instance"))
            .header("host", &ctx.api.host)
            .header("x-forwarded-for", ip)
            .send()
    };
    assert_eq!(
        get("203.0.113.66").await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(get("203.0.113.67").await.unwrap().status(), StatusCode::OK);
}

/// Block endpoints validate as Mastodon's models do: a duplicate is a 422,
/// email domain blocks keep `allow_with_approval`, canonical blocks are
/// made from an email, and each change is logged.
#[tokio::test]
async fn test_block_endpoints_validate_and_log() {
    let ctx = TestContext::new("mod-blocks").await;
    make_admin(&ctx).await;
    let token = Some(ctx.alice_token.as_str());

    let first = ctx
        .api
        .post_json(
            "/api/v1/admin/ip_blocks",
            token,
            &json!({"ip": "192.0.2.0/24", "severity": "no_access"}),
        )
        .await;
    assert_eq!(first.status(), StatusCode::OK);
    let again = ctx
        .api
        .post_json(
            "/api/v1/admin/ip_blocks",
            token,
            &json!({"ip": "192.0.2.0/24", "severity": "sign_up_block"}),
        )
        .await;
    assert_eq!(again.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let email: Value = ctx
        .api
        .post_json(
            "/api/v1/admin/email_domain_blocks",
            token,
            &json!({"domain": "Maybe.Test", "allow_with_approval": true}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(email["domain"], "maybe.test");
    assert_eq!(email["allow_with_approval"], true);
    assert_eq!(email["history"].as_array().map(Vec::len), Some(7));

    let canonical: Value = ctx
        .api
        .post_json(
            "/api/v1/admin/canonical_email_blocks",
            token,
            &json!({"email": "Some.One+tag@Example.com"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let matched: Vec<Value> = ctx
        .api
        .post_json(
            "/api/v1/admin/canonical_email_blocks/test",
            token,
            &json!({"email": "someone@example.com"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0]["id"], canonical["id"]);

    let logged: Vec<String> = sqlx::query_scalar(
        "SELECT target_type FROM admin_action_logs WHERE action = 'create' ORDER BY id",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        logged,
        vec!["IpBlock", "EmailDomainBlock", "CanonicalEmailBlock"]
    );
}
