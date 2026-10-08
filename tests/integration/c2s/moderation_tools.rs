//! The moderation tools Mastodon has only as server-rendered admin pages, as
//! eunha serves them over REST: report and account notes, the audit log,
//! warning presets, username blocks, strikes and appeals, an account's posts
//! and relationships, and managing a user.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn make_admin(ctx: &TestContext) {
    crate::helpers::make_admin(&ctx.db, id(&ctx.alice_id)).await;
}

fn id(s: &str) -> i64 {
    s.parse().unwrap()
}

/// A role with `permissions` at `position`, given to `account_id`, whose
/// tokens get the admin scopes.
async fn give_role(ctx: &TestContext, account_id: &str, position: i32, permissions: i64) -> i64 {
    let role: i64 = sqlx::query_scalar(
        "INSERT INTO user_roles (name, position, permissions, highlighted, created_at, updated_at)
         VALUES ($1, $2, $3, true, now(), now()) RETURNING id",
    )
    .bind(format!("Role {position}"))
    .bind(position)
    .bind(permissions)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query("UPDATE users SET role_id = $1 WHERE account_id = $2")
        .bind(role)
        .bind(id(account_id))
        .execute(&ctx.db)
        .await
        .unwrap();
    crate::helpers::grant_admin_scopes(&ctx.db, id(account_id)).await;
    role
}

async fn json_ok(resp: reqwest::Response) -> Value {
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{text}");
    serde_json::from_str(&text).unwrap()
}

async fn file_report(ctx: &TestContext, token: &str, body: Value) -> Value {
    json_ok(
        ctx.api
            .post_json("/api/v1/reports", Some(token), &body)
            .await,
    )
    .await
}

async fn logs(ctx: &TestContext) -> Vec<(String, String)> {
    sqlx::query_as("SELECT action, target_type FROM admin_action_logs ORDER BY id")
        .fetch_all(&ctx.db)
        .await
        .unwrap()
}

/// Report notes: validated as `ReportNote` is, resolving or reopening the
/// report with the note, and deletable by their author or a higher role.
#[tokio::test]
async fn test_report_notes() {
    let ctx = TestContext::new("tools-report-notes").await;
    make_admin(&ctx).await;
    let (carol_id, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let report = file_report(&ctx, &carol_token, json!({"account_id": ctx.bob_id})).await;
    let rid = report["id"].as_str().unwrap();

    let blank = ctx
        .api
        .post_json(
            "/api/v1/admin/report_notes",
            Some(&ctx.alice_token),
            &json!({"report_id": rid, "content": "  "}),
        )
        .await;
    assert_eq!(blank.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        blank.json::<Value>().await.unwrap()["error"],
        "Validation failed: Content can't be blank"
    );
    let long = ctx
        .api
        .post_json(
            "/api/v1/admin/report_notes",
            Some(&ctx.alice_token),
            &json!({"report_id": rid, "content": "x".repeat(2_001)}),
        )
        .await;
    assert_eq!(long.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let note = json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/report_notes",
                Some(&ctx.alice_token),
                &json!({"report_id": rid, "content": "looked at it", "create_and_resolve": true}),
            )
            .await,
    )
    .await;
    assert_eq!(note["content"], "looked at it");
    assert_eq!(note["account"]["id"], ctx.alice_id);
    let resolved: Option<i64> =
        sqlx::query_scalar("SELECT action_taken_by_account_id FROM reports WHERE id = $1")
            .bind(id(rid))
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(resolved, Some(id(&ctx.alice_id)));
    json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/report_notes",
                Some(&ctx.alice_token),
                &json!({"report_id": rid, "content": "again", "create_and_unresolve": "1"}),
            )
            .await,
    )
    .await;
    assert_eq!(
        logs(&ctx).await,
        vec![
            ("resolve".to_string(), "Report".to_string()),
            ("reopen".to_string(), "Report".to_string()),
        ]
    );

    let notes = json_ok(
        ctx.api
            .get(
                &format!("/api/v1/admin/report_notes?report_id={rid}"),
                Some(&ctx.alice_token),
            )
            .await,
    )
    .await;
    assert_eq!(notes.as_array().unwrap().len(), 2);
    assert_eq!(notes[0]["content"], "looked at it");

    // A moderator below the author's role may not delete the author's note.
    give_role(&ctx, &carol_id.to_string(), 10, 1 << 4).await;
    let refused = ctx
        .api
        .delete(
            &format!(
                "/api/v1/admin/report_notes/{}",
                note["id"].as_str().unwrap()
            ),
            &carol_token,
        )
        .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let own = ctx
        .api
        .delete(
            &format!(
                "/api/v1/admin/report_notes/{}",
                note["id"].as_str().unwrap()
            ),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(own.status(), StatusCode::OK);

    // Without `manage_reports`, nothing.
    let bob = ctx
        .api
        .get(
            &format!("/api/v1/admin/report_notes?report_id={rid}"),
            Some(&ctx.bob_token),
        )
        .await;
    assert_eq!(bob.status(), StatusCode::FORBIDDEN);
}

/// Account moderation notes: written with `manage_reports`, listed with the
/// account page's `manage_users`.
#[tokio::test]
async fn test_account_moderation_notes() {
    let ctx = TestContext::new("tools-account-notes").await;
    make_admin(&ctx).await;
    let note = json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/account_moderation_notes",
                Some(&ctx.alice_token),
                &json!({"target_account_id": ctx.bob_id, "content": "watch this one"}),
            )
            .await,
    )
    .await;
    assert_eq!(note["target_account_id"], ctx.bob_id);
    let missing = ctx
        .api
        .post_json(
            "/api/v1/admin/account_moderation_notes",
            Some(&ctx.alice_token),
            &json!({"target_account_id": "1", "content": "who"}),
        )
        .await;
    assert_eq!(missing.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let notes = json_ok(
        ctx.api
            .get(
                &format!(
                    "/api/v1/admin/account_moderation_notes?target_account_id={}",
                    ctx.bob_id
                ),
                Some(&ctx.alice_token),
            )
            .await,
    )
    .await;
    assert_eq!(notes[0]["content"], "watch this one");
    let deleted = ctx
        .api
        .delete(
            &format!(
                "/api/v1/admin/account_moderation_notes/{}",
                note["id"].as_str().unwrap()
            ),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(deleted.status(), StatusCode::OK);
    assert!(logs(&ctx).await.is_empty(), "notes are not logged");
}

/// The audit log words each entry as Mastodon's log does and filters by
/// account, action type and target account.
#[tokio::test]
async fn test_action_logs() {
    let ctx = TestContext::new("tools-action-logs").await;
    make_admin(&ctx).await;
    json_ok(
        ctx.api
            .post_json(
                &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
                Some(&ctx.alice_token),
                &json!({"type": "silence"}),
            )
            .await,
    )
    .await;
    json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/email_domain_blocks",
                Some(&ctx.alice_token),
                &json!({"domain": "spam.example"}),
            )
            .await,
    )
    .await;

    let all = json_ok(
        ctx.api
            .get("/api/v1/admin/action_logs", Some(&ctx.alice_token))
            .await,
    )
    .await;
    let all = all.as_array().unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0]["text"], "alice blocked email domain spam.example");
    assert_eq!(all[1]["text"], "alice limited bob's account");
    assert_eq!(all[1]["template"], "%{name} limited %{target}'s account");
    assert_eq!(all[1]["action_type"], "silence_account");
    assert_eq!(
        all[1]["target"]["href"],
        format!("/admin/accounts/{}", ctx.bob_id)
    );
    assert_eq!(all[1]["account"]["id"], ctx.alice_id);

    let silenced = json_ok(
        ctx.api
            .get(
                "/api/v1/admin/action_logs?action_type=silence_account",
                Some(&ctx.alice_token),
            )
            .await,
    )
    .await;
    assert_eq!(silenced.as_array().unwrap().len(), 1);
    let about_bob = json_ok(
        ctx.api
            .get(
                &format!("/api/v1/admin/action_logs?target_account_id={}", ctx.bob_id),
                Some(&ctx.alice_token),
            )
            .await,
    )
    .await;
    assert_eq!(about_bob.as_array().unwrap().len(), 1);
    let by_bob = json_ok(
        ctx.api
            .get(
                &format!("/api/v1/admin/action_logs?account_id={}", ctx.bob_id),
                Some(&ctx.alice_token),
            )
            .await,
    )
    .await;
    assert!(by_bob.as_array().unwrap().is_empty());

    let filters = json_ok(
        ctx.api
            .get("/api/v1/admin/action_logs/filters", Some(&ctx.alice_token))
            .await,
    )
    .await;
    assert_eq!(filters["accounts"][0]["label"], "alice");
    assert_eq!(filters["action_types"][0]["label"], "Approve Appeal");

    // `AuditLogPolicy#index?`: `view_audit_log`.
    crate::helpers::grant_admin_scopes(&ctx.db, id(&ctx.bob_id)).await;
    let bob = ctx
        .api
        .get("/api/v1/admin/action_logs", Some(&ctx.bob_token))
        .await;
    assert_eq!(bob.status(), StatusCode::FORBIDDEN);
}

/// Warning presets: managed with `manage_settings`, offered to anyone who may
/// take an account action, and their text leads the warning.
#[tokio::test]
async fn test_warning_presets() {
    let ctx = TestContext::new("tools-presets").await;
    make_admin(&ctx).await;
    let blank = ctx
        .api
        .post_json(
            "/api/v1/admin/warning_presets",
            Some(&ctx.alice_token),
            &json!({"title": "Spam"}),
        )
        .await;
    assert_eq!(blank.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        blank.json::<Value>().await.unwrap()["error"],
        "Validation failed: Text can't be blank"
    );
    let b = json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/warning_presets",
                Some(&ctx.alice_token),
                &json!({"title": "B", "text": "Stop spamming."}),
            )
            .await,
    )
    .await;
    json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/warning_presets",
                Some(&ctx.alice_token),
                &json!({"title": "A", "text": "Be kind."}),
            )
            .await,
    )
    .await;
    let updated = json_ok(
        ctx.api
            .patch_json(
                &format!(
                    "/api/v1/admin/warning_presets/{}",
                    b["id"].as_str().unwrap()
                ),
                Some(&ctx.alice_token),
                &json!({"text": "Stop spamming, please."}),
            )
            .await,
    )
    .await;
    assert_eq!(updated["title"], "B");

    // A moderator without `manage_settings` lists them, but may not edit.
    let (carol_id, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    give_role(&ctx, &carol_id.to_string(), 10, 1 << 10).await;
    let listed = json_ok(
        ctx.api
            .get("/api/v1/admin/warning_presets", Some(&carol_token))
            .await,
    )
    .await;
    assert_eq!(listed[0]["title"], "A", "alphabetic");
    let refused = ctx
        .api
        .delete(
            &format!(
                "/api/v1/admin/warning_presets/{}",
                b["id"].as_str().unwrap()
            ),
            &carol_token,
        )
        .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);

    json_ok(
        ctx.api
            .post_json(
                &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
                Some(&ctx.alice_token),
                &json!({"type": "none", "warning_preset_id": b["id"], "text": "Last chance.",
                        "send_email_notification": false}),
            )
            .await,
    )
    .await;
    let text: String = sqlx::query_scalar("SELECT text FROM account_warnings")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(text, "Stop spamming, please.\n\nLast chance.");
}

/// Username blocks: logged as Mastodon logs them, unique, and what sign-ups
/// are checked against.
#[tokio::test]
async fn test_username_blocks() {
    let ctx = TestContext::new("tools-username-blocks").await;
    make_admin(&ctx).await;
    let block = json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/username_blocks",
                Some(&ctx.alice_token),
                &json!({"username": "Admin", "comparison": "contains"}),
            )
            .await,
    )
    .await;
    assert_eq!(block["comparison"], "contains");
    let taken = ctx
        .api
        .post_json(
            "/api/v1/admin/username_blocks",
            Some(&ctx.alice_token),
            &json!({"username": "admin"}),
        )
        .await;
    assert_eq!(taken.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        taken.json::<Value>().await.unwrap()["error"],
        "Validation failed: Username has already been taken"
    );
    let normalized: String = sqlx::query_scalar("SELECT normalized_username FROM username_blocks")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(normalized, "admin");
    assert!(eunha::moderation::signup::username_blocked(&ctx.state, "the4dm1n", false).await);

    let bid = block["id"].as_str().unwrap();
    let exact = json_ok(
        ctx.api
            .patch_json(
                &format!("/api/v1/admin/username_blocks/{bid}"),
                Some(&ctx.alice_token),
                &json!({"comparison": "equals"}),
            )
            .await,
    )
    .await;
    assert_eq!(exact["comparison"], "equals");
    assert!(!eunha::moderation::signup::username_blocked(&ctx.state, "the4dm1n", false).await);
    assert!(eunha::moderation::signup::username_blocked(&ctx.state, "ADM1N", false).await);

    let deleted = ctx
        .api
        .delete(
            &format!("/api/v1/admin/username_blocks/{bid}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(deleted.status(), StatusCode::OK);
    assert_eq!(
        logs(&ctx).await,
        vec![
            ("create".to_string(), "UsernameBlock".to_string()),
            ("update".to_string(), "UsernameBlock".to_string()),
            ("destroy".to_string(), "UsernameBlock".to_string()),
        ]
    );
    let entries = json_ok(
        ctx.api
            .get("/api/v1/admin/action_logs", Some(&ctx.alice_token))
            .await,
    )
    .await;
    assert_eq!(
        entries[0]["text"],
        "alice removed rule for usernames containing Admin"
    );
}

async fn silence_bob(ctx: &TestContext) -> String {
    json_ok(
        ctx.api
            .post_json(
                &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
                Some(&ctx.alice_token),
                &json!({"type": "silence", "text": "too loud", "send_email_notification": false}),
            )
            .await,
    )
    .await;
    let strikes = json_ok(
        ctx.api
            .get("/api/v1/disputes/strikes", Some(&ctx.bob_token))
            .await,
    )
    .await;
    strikes[0]["id"].as_str().unwrap().to_owned()
}

/// A user sees their strike and appeals it once, within the window; staff
/// with `manage_appeals` approve it, which lifts what the strike did.
#[tokio::test]
async fn test_appeal_approved_undoes_the_strike() {
    let ctx = TestContext::new("tools-appeal-approve").await;
    make_admin(&ctx).await;
    let strike_id = silence_bob(&ctx).await;
    let strike = json_ok(
        ctx.api
            .get(
                &format!("/api/v1/disputes/strikes/{strike_id}"),
                Some(&ctx.bob_token),
            )
            .await,
    )
    .await;
    assert_eq!(strike["action"], "silence");
    assert_eq!(strike["text"], "too loud");
    assert_eq!(strike["can_appeal"], true);
    assert!(strike.get("account").is_none(), "the issuer is for staff");

    // Someone else's strike is not theirs to read.
    let (_, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let refused = ctx
        .api
        .get(
            &format!("/api/v1/disputes/strikes/{strike_id}"),
            Some(&carol_token),
        )
        .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let not_hers = ctx
        .api
        .post_json(
            &format!("/api/v1/disputes/strikes/{strike_id}/appeal"),
            Some(&carol_token),
            &json!({"text": "unfair"}),
        )
        .await;
    assert_eq!(not_hers.status(), StatusCode::NOT_FOUND);

    let blank = ctx
        .api
        .post_json(
            &format!("/api/v1/disputes/strikes/{strike_id}/appeal"),
            Some(&ctx.bob_token),
            &json!({"text": ""}),
        )
        .await;
    assert_eq!(blank.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let appealed = json_ok(
        ctx.api
            .post_json(
                &format!("/api/v1/disputes/strikes/{strike_id}/appeal"),
                Some(&ctx.bob_token),
                &json!({"text": "I was quiet"}),
            )
            .await,
    )
    .await;
    assert_eq!(appealed["appeal"]["state"], "pending");
    assert_eq!(appealed["can_appeal"], false);
    let again = ctx
        .api
        .post_json(
            &format!("/api/v1/disputes/strikes/{strike_id}/appeal"),
            Some(&ctx.bob_token),
            &json!({"text": "please"}),
        )
        .await;
    assert_eq!(again.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        again.json::<Value>().await.unwrap()["error"],
        "Validation failed: Account warning has already been taken"
    );

    let pending = json_ok(
        ctx.api
            .get("/api/v1/admin/disputes/appeals", Some(&ctx.alice_token))
            .await,
    )
    .await;
    assert_eq!(pending.as_array().unwrap().len(), 1);
    assert_eq!(pending[0]["text"], "I was quiet");
    assert_eq!(pending[0]["account"]["id"], ctx.bob_id);
    assert_eq!(pending[0]["strike"]["account"]["id"], ctx.alice_id);
    let appeal_id = pending[0]["id"].as_str().unwrap().to_owned();

    let approved = json_ok(
        ctx.api
            .post_json(
                &format!("/api/v1/admin/disputes/appeals/{appeal_id}/approve"),
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await,
    )
    .await;
    assert_eq!(approved["state"], "approved");
    assert!(approved["strike"]["overruled_at"].is_string());
    let silenced: Option<chrono::NaiveDateTime> =
        sqlx::query_scalar("SELECT silenced_at FROM accounts WHERE id = $1")
            .bind(id(&ctx.bob_id))
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(silenced.is_none(), "approving lifts the limit");
    let twice = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/disputes/appeals/{appeal_id}/approve"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(
        twice.status(),
        StatusCode::FORBIDDEN,
        "only a pending appeal"
    );

    let entries = json_ok(
        ctx.api
            .get(
                "/api/v1/admin/action_logs?action_type=approve_appeal",
                Some(&ctx.alice_token),
            )
            .await,
    )
    .await;
    assert_eq!(
        entries[0]["text"],
        "alice approved moderation decision appeal from bob"
    );
    assert_eq!(
        entries[0]["target"]["href"],
        format!("/disputes/strikes/{strike_id}")
    );
}

/// A rejected appeal leaves the strike standing; a frozen login can still
/// appeal; and a strike past the window cannot be appealed.
#[tokio::test]
async fn test_appeal_rejected_and_window() {
    let ctx = TestContext::new("tools-appeal-reject").await;
    make_admin(&ctx).await;
    json_ok(
        ctx.api
            .post_json(
                &format!("/api/v1/admin/accounts/{}/action", ctx.bob_id),
                Some(&ctx.alice_token),
                &json!({"type": "disable", "send_email_notification": false}),
            )
            .await,
    )
    .await;
    let strikes = json_ok(
        ctx.api
            .get("/api/v1/disputes/strikes", Some(&ctx.bob_token))
            .await,
    )
    .await;
    let strike_id = strikes[0]["id"].as_str().unwrap().to_owned();
    json_ok(
        ctx.api
            .post_json(
                &format!("/api/v1/disputes/strikes/{strike_id}/appeal"),
                Some(&ctx.bob_token),
                &json!({"text": "let me back"}),
            )
            .await,
    )
    .await;
    let appeal_id: i64 = sqlx::query_scalar("SELECT id FROM appeals")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    let rejected = json_ok(
        ctx.api
            .post_json(
                &format!("/api/v1/admin/disputes/appeals/{appeal_id}/reject"),
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await,
    )
    .await;
    assert_eq!(rejected["state"], "rejected");
    let disabled: bool = sqlx::query_scalar("SELECT disabled FROM users WHERE account_id = $1")
        .bind(id(&ctx.bob_id))
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert!(disabled, "a rejected appeal changes nothing");
    let rejected_list = json_ok(
        ctx.api
            .get(
                "/api/v1/admin/disputes/appeals?status=rejected",
                Some(&ctx.alice_token),
            )
            .await,
    )
    .await;
    assert_eq!(rejected_list.as_array().unwrap().len(), 1);
    let unknown = ctx
        .api
        .get(
            "/api/v1/admin/disputes/appeals?status=nope",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);

    // A strike older than `APPEAL_WINDOW`.
    let old: i64 = sqlx::query_scalar(
        "INSERT INTO account_warnings (account_id, target_account_id, action, text, created_at, updated_at)
         VALUES ($1, $2, 0, 'old', now() - interval '21 days', now()) RETURNING id",
    )
    .bind(id(&ctx.alice_id))
    .bind(id(&ctx.bob_id))
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let late = ctx
        .api
        .post_json(
            &format!("/api/v1/disputes/strikes/{old}/appeal"),
            Some(&ctx.bob_token),
            &json!({"text": "late"}),
        )
        .await;
    assert_eq!(late.status(), StatusCode::FORBIDDEN);
}

/// An account's posts as a moderator sees them, and the batch action that
/// gathers some into a report or takes them out of one.
#[tokio::test]
async fn test_admin_statuses_and_batch() {
    let ctx = TestContext::new("tools-statuses").await;
    make_admin(&ctx).await;
    let public = ctx.api.post_status(&ctx.bob_token, "hello", "public").await;
    let private = ctx
        .api
        .post_status(&ctx.bob_token, "secret", "private")
        .await;
    let url = format!("/api/v1/admin/accounts/{}/statuses", ctx.bob_id);

    let listed = json_ok(ctx.api.get(&url, Some(&ctx.alice_token)).await).await;
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 1, "public and unlisted posts only");
    assert_eq!(listed[0]["id"], public["id"]);

    let shown = json_ok(
        ctx.api
            .get(
                &format!("{url}/{}", public["id"].as_str().unwrap()),
                Some(&ctx.alice_token),
            )
            .await,
    )
    .await;
    assert_eq!(shown["edits"].as_array().unwrap().len(), 1);
    let hidden = ctx
        .api
        .get(
            &format!("{url}/{}", private["id"].as_str().unwrap()),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(
        hidden.status(),
        StatusCode::FORBIDDEN,
        "a private post the moderator cannot read and nobody reported"
    );

    let none = ctx
        .api
        .post_json(
            &format!("{url}/batch"),
            Some(&ctx.alice_token),
            &json!({"type": "report", "status_ids": []}),
        )
        .await;
    assert_eq!(none.status(), StatusCode::BAD_REQUEST);

    // The private post is not one the moderator could see, so it stays out.
    let report = json_ok(
        ctx.api
            .post_json(
                &format!("{url}/batch"),
                Some(&ctx.alice_token),
                &json!({"type": "report", "status_ids": [public["id"], private["id"]]}),
            )
            .await,
    )
    .await;
    assert_eq!(report["account"]["id"], ctx.alice_id);
    assert_eq!(report["target_account"]["id"], ctx.bob_id);
    assert_eq!(report["statuses"].as_array().unwrap().len(), 1);
    let rid = report["id"].as_str().unwrap();

    let removed = json_ok(
        ctx.api
            .post_json(
                &format!("{url}/batch"),
                Some(&ctx.alice_token),
                &json!({"type": "remove_from_report", "report_id": rid,
                        "status_ids": [public["id"]]}),
            )
            .await,
    )
    .await;
    assert!(removed["statuses"].as_array().unwrap().is_empty());
}

/// Whom an account follows and who follows it.
#[tokio::test]
async fn test_admin_relationships() {
    let ctx = TestContext::new("tools-relationships").await;
    make_admin(&ctx).await;
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    let url = format!("/api/v1/admin/accounts/{}/relationships", ctx.bob_id);
    let following = json_ok(ctx.api.get(&url, Some(&ctx.alice_token)).await).await;
    assert_eq!(following[0]["id"], ctx.alice_id);
    let followers = json_ok(
        ctx.api
            .get(
                &format!("{url}?relationship=followed_by"),
                Some(&ctx.alice_token),
            )
            .await,
    )
    .await;
    assert!(followers.as_array().unwrap().is_empty());
    let unknown = ctx
        .api
        .get(&format!("{url}?relationship=nope"), Some(&ctx.alice_token))
        .await;
    assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
}

/// Changing a user's role: `manage_roles` over the user's role, and never to
/// a role above one's own.
#[tokio::test]
async fn test_change_role() {
    let ctx = TestContext::new("tools-change-role").await;
    let mine = give_role(&ctx, &ctx.alice_id.clone(), 50, 1 << 17).await;
    let low: i64 = sqlx::query_scalar(
        "INSERT INTO user_roles (name, position, permissions, highlighted, created_at, updated_at)
         VALUES ('Helper', 10, 0, false, now(), now()) RETURNING id",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let high: i64 = sqlx::query_scalar(
        "INSERT INTO user_roles (name, position, permissions, highlighted, created_at, updated_at)
         VALUES ('Owner', 90, 1, false, now(), now()) RETURNING id",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let url = format!("/api/v1/admin/accounts/{}/role", ctx.bob_id);

    let roles = json_ok(
        ctx.api
            .get("/api/v1/admin/roles", Some(&ctx.alice_token))
            .await,
    )
    .await;
    let positions: Vec<i64> = roles
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["position"].as_i64().unwrap())
        .collect();
    assert!(positions.windows(2).all(|w| w[0] <= w[1]), "lowest first");
    assert!(roles
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == "Helper"));
    assert!(
        roles.as_array().unwrap().iter().all(|r| r["id"] != "-99"),
        "never the everyone role"
    );

    let elevated = ctx
        .api
        .put_json(
            &url,
            Some(&ctx.alice_token),
            &json!({"role_id": high.to_string()}),
        )
        .await;
    assert_eq!(elevated.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        elevated.json::<Value>().await.unwrap()["error"],
        "Validation failed: Role cannot be higher than your current role"
    );
    let changed = json_ok(
        ctx.api
            .put_json(
                &url,
                Some(&ctx.alice_token),
                &json!({"role_id": mine.to_string()}),
            )
            .await,
    )
    .await;
    assert_eq!(
        changed["role"]["id"],
        mine.to_string(),
        "an equal role is fine"
    );
    // Now bob holds alice's role, so she no longer outranks him.
    let refused = ctx
        .api
        .put_json(
            &url,
            Some(&ctx.alice_token),
            &json!({"role_id": low.to_string()}),
        )
        .await;
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        logs(&ctx).await,
        vec![("change_role".to_string(), "User".to_string())]
    );
}

/// Resetting a password signs the user out everywhere; disabling two-factor
/// clears it; changing the email waits for the new address to confirm.
#[tokio::test]
async fn test_user_access_management() {
    let ctx = TestContext::new("tools-user-access").await;
    make_admin(&ctx).await;
    let base = format!("/api/v1/admin/accounts/{}", ctx.bob_id);

    sqlx::query(
        "UPDATE users SET otp_required_for_login = true, otp_secret = 'x', otp_backup_codes = '{a,b}'
         WHERE account_id = $1",
    )
    .bind(id(&ctx.bob_id))
    .execute(&ctx.db)
    .await
    .unwrap();
    let resp = ctx
        .api
        .delete(
            &format!("{base}/two_factor_authentication"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (otp, secret): (bool, Option<String>) = sqlx::query_as(
        "SELECT otp_required_for_login, otp_secret FROM users WHERE account_id = $1",
    )
    .bind(id(&ctx.bob_id))
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(!otp && secret.is_none());

    json_ok(
        ctx.api
            .post_json(
                &format!("{base}/change_email"),
                Some(&ctx.alice_token),
                &json!({"unconfirmed_email": "Bob.New@test.invalid"}),
            )
            .await,
    )
    .await;
    let token: Option<String> = sqlx::query_scalar(
        "SELECT confirmation_token FROM users WHERE account_id = $1 AND unconfirmed_email = 'Bob.New@test.invalid'",
    )
    .bind(id(&ctx.bob_id))
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let token = token.expect("a confirmation token is sent");
    // `pending_reconfirmation?` picks the reconfirmation template.
    let mail = ctx
        .mail_to("Bob.New@test.invalid", "Mastodon: Confirm email for")
        .await
        .expect("the reconfirmation mail");
    assert!(mail.html.contains(&format!("/auth/confirm?token={token}")));
    ctx.api
        .get(&format!("/auth/confirm?token={token}"), None)
        .await;
    let email: String = sqlx::query_scalar("SELECT email FROM users WHERE account_id = $1")
        .bind(id(&ctx.bob_id))
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(email, "bob.new@test.invalid");

    // Confirming or resending for a confirmed user is refused.
    let confirm = ctx
        .api
        .post_json(
            &format!("{base}/confirmation"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(confirm.status(), StatusCode::FORBIDDEN);
    let resend = ctx
        .api
        .post_json(
            &format!("{base}/confirmation/resend"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resend.status(), StatusCode::UNPROCESSABLE_ENTITY);

    json_ok(
        ctx.api
            .post_json(&format!("{base}/reset"), Some(&ctx.alice_token), &json!({}))
            .await,
    )
    .await;
    let signed_out = ctx
        .api
        .get("/api/v1/accounts/verify_credentials", Some(&ctx.bob_token))
        .await;
    assert_eq!(signed_out.status(), StatusCode::UNAUTHORIZED);
    let reset_token: Option<String> =
        sqlx::query_scalar("SELECT reset_password_token FROM users WHERE account_id = $1")
            .bind(id(&ctx.bob_id))
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert!(reset_token.is_some(), "the reset link is mailed");

    assert_eq!(
        logs(&ctx).await,
        vec![
            ("disable_2fa".to_string(), "User".to_string()),
            ("change_email".to_string(), "User".to_string()),
            ("reset_password".to_string(), "User".to_string()),
        ]
    );
}

/// Acting on what a report cites: marking its posts sensitive edits the
/// ones with media; removing them discards every one. Each logs, resolves
/// the report, and strikes the account citing the posts.
#[tokio::test]
async fn test_report_moderation_actions() {
    let ctx = TestContext::new("tools-report-actions").await;
    make_admin(&ctx).await;
    let (_, carol_token) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let with_media = ctx.api.post_status(&ctx.bob_token, "look", "public").await;
    let plain = ctx.api.post_status(&ctx.bob_token, "words", "public").await;
    // Attached as `PostStatusService` attaches it, in the status's order.
    sqlx::query(
        "WITH m AS (
             INSERT INTO media_attachments (id, account_id, status_id, type, created_at, updated_at)
             VALUES ($1, $2, $3, 0, now(), now()) RETURNING id, status_id)
         UPDATE statuses s SET ordered_media_attachment_ids = ARRAY[m.id]
         FROM m WHERE s.id = m.status_id",
    )
    .bind(eunha::snowflake::next_id())
    .bind(id(&ctx.bob_id))
    .bind(id(with_media["id"].as_str().unwrap()))
    .execute(&ctx.db)
    .await
    .unwrap();
    let report = file_report(
        &ctx,
        &carol_token,
        json!({"account_id": ctx.bob_id, "status_ids": [with_media["id"], plain["id"]],
               "category": "other"}),
    )
    .await;
    let rid = report["id"].as_str().unwrap();
    let url = format!("/api/v1/admin/reports/{rid}/actions");

    let unknown = ctx
        .api
        .post_json(
            &url,
            Some(&ctx.alice_token),
            &json!({"moderation_action": "nope"}),
        )
        .await;
    assert_eq!(unknown.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let resolved = json_ok(
        ctx.api
            .post_json(
                &url,
                Some(&ctx.alice_token),
                &json!({"moderation_action": "mark_as_sensitive", "text": "tag your media"}),
            )
            .await,
    )
    .await;
    assert_eq!(resolved["action_taken"], true);
    let sensitive: Vec<(i64, bool)> =
        sqlx::query_as("SELECT id, sensitive FROM statuses WHERE account_id = $1 ORDER BY id")
            .bind(id(&ctx.bob_id))
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    assert_eq!(
        sensitive,
        vec![
            (id(with_media["id"].as_str().unwrap()), true),
            (id(plain["id"].as_str().unwrap()), false),
        ]
    );
    let edits: i64 = sqlx::query_scalar("SELECT count(*) FROM status_edits WHERE status_id = $1")
        .bind(id(with_media["id"].as_str().unwrap()))
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(edits, 1, "the version before is kept");

    json_ok(
        ctx.api
            .post_json(
                &url,
                Some(&ctx.alice_token),
                &json!({"moderation_action": "delete"}),
            )
            .await,
    )
    .await;
    let kept: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM statuses WHERE account_id = $1 AND deleted_at IS NULL",
    )
    .bind(id(&ctx.bob_id))
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(kept, 0);

    let strikes: Vec<(i32, Vec<String>)> = sqlx::query_as(
        "SELECT action, status_ids FROM account_warnings WHERE target_account_id = $1 ORDER BY id",
    )
    .bind(id(&ctx.bob_id))
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(strikes.len(), 2);
    assert_eq!(strikes[0].0, 1_250);
    assert_eq!(strikes[1].0, 1_500);
    assert_eq!(strikes[1].1.len(), 2);
    assert_eq!(
        logs(&ctx).await,
        vec![
            ("update".to_string(), "Status".to_string()),
            ("resolve".to_string(), "Report".to_string()),
            ("destroy".to_string(), "Status".to_string()),
            ("destroy".to_string(), "Status".to_string()),
            ("resolve".to_string(), "Report".to_string()),
        ]
    );
    let entries = json_ok(
        ctx.api
            .get(
                "/api/v1/admin/action_logs?action_type=destroy_status",
                Some(&ctx.alice_token),
            )
            .await,
    )
    .await;
    assert_eq!(entries[0]["text"], "alice removed post by bob");
}
