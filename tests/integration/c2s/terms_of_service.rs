//! Terms of service and the privacy policy: the public endpoints Mastodon
//! serves, and the administration it does through web forms, served here over
//! REST (the `terms-of-service-rest-api` divergence).

use chrono::{Duration, NaiveDate, Utc};
use reqwest::StatusCode;
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::helpers::{make_admin, seed_user, set_setting, user_id_for, TestContext};

fn today() -> NaiveDate {
    Utc::now().date_naive()
}

fn day(offset: i64) -> NaiveDate {
    today() + Duration::days(offset)
}

/// A version, published a minute ago unless `published` is false.
async fn insert(db: &PgPool, text: &str, effective: Option<NaiveDate>, published: bool) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO terms_of_services (text, changelog, effective_date, published_at, created_at, updated_at)
         VALUES ($1, 'Changes.', $2,
                 CASE WHEN $3 THEN (now() AT TIME ZONE 'UTC') - interval '1 minute' END,
                 now() AT TIME ZONE 'UTC', now() AT TIME ZONE 'UTC')
         RETURNING id",
    )
    .bind(text)
    .bind(effective)
    .bind(published)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn get_json(ctx: &TestContext, path: &str, token: Option<&str>) -> (StatusCode, Value) {
    let resp = ctx.api.get(path, token).await;
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

// ── Public endpoints ──────────────────────────────────────────────────────

#[tokio::test]
async fn no_terms_is_a_404_and_no_url() {
    let ctx = TestContext::new("tos-none").await;
    let (status, _) = get_json(&ctx, "/api/v1/instance/terms_of_service", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A draft is not terms anyone is held to.
    insert(&ctx.db, "Draft.", Some(day(3)), false).await;
    let (status, _) = get_json(&ctx, "/api/v1/instance/terms_of_service", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, instance) = get_json(&ctx, "/api/v2/instance", None).await;
    let urls = &instance["configuration"]["urls"];
    assert!(urls["terms_of_service"].is_null());
    // There is always a privacy policy, if only Mastodon's own.
    assert_eq!(
        urls["privacy_policy"],
        format!("https://{}/privacy-policy", ctx.domain)
    );
}

#[tokio::test]
async fn current_is_the_latest_live_version() {
    let ctx = TestContext::new("tos-current").await;
    insert(&ctx.db, "Oldest.", Some(day(-30)), true).await;
    insert(&ctx.db, "Live.", Some(day(-2)), true).await;
    insert(&ctx.db, "Upcoming.", Some(day(5)), true).await;
    insert(&ctx.db, "Draft.", Some(day(7)), false).await;

    let (status, body) = get_json(&ctx, "/api/v1/instance/terms_of_service", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"], "<p>Live.</p>\n");
    assert_eq!(body["effective_date"], day(-2).to_string());
    assert_eq!(body["effective"], true);
    // `succeeded_by` is the latest published version effective on or after
    // this one — the upcoming one here.
    assert_eq!(body["succeeded_by"], day(5).to_string());

    let (_, instance) = get_json(&ctx, "/api/v2/instance", None).await;
    assert_eq!(
        instance["configuration"]["urls"]["terms_of_service"],
        format!("https://{}/terms-of-service", ctx.domain)
    );
}

#[tokio::test]
async fn upcoming_terms_stand_in_until_any_are_live() {
    let ctx = TestContext::new("tos-upcoming").await;
    insert(&ctx.db, "Later.", Some(day(20)), true).await;
    insert(&ctx.db, "Sooner.", Some(day(4)), true).await;

    let (status, body) = get_json(&ctx, "/api/v1/instance/terms_of_service", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"], "<p>Sooner.</p>\n");
    assert_eq!(body["effective"], false);
    assert_eq!(body["succeeded_by"], day(20).to_string());
}

#[tokio::test]
async fn terms_going_live_today_are_live_but_not_yet_effective() {
    let ctx = TestContext::new("tos-today").await;
    insert(&ctx.db, "Older.", Some(day(-10)), true).await;
    insert(&ctx.db, "Today.", Some(today()), true).await;

    let (_, body) = get_json(&ctx, "/api/v1/instance/terms_of_service", None).await;
    // `live` compares the date with now, so today's terms are live; but
    // `effective?` is `effective_date.past?`, which today is not.
    assert_eq!(body["content"], "<p>Today.</p>\n");
    assert_eq!(body["effective"], false);
    assert!(body["succeeded_by"].is_null());
}

#[tokio::test]
async fn versions_by_date() {
    let ctx = TestContext::new("tos-by-date").await;
    insert(&ctx.db, "First.", Some(day(-30)), true).await;
    insert(&ctx.db, "Second.", Some(day(-1)), true).await;
    insert(&ctx.db, "Draft.", Some(day(9)), false).await;

    let (status, body) = get_json(
        &ctx,
        &format!("/api/v1/instance/terms_of_service/{}", day(-30)),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"], "<p>First.</p>\n");
    assert_eq!(body["effective"], true);
    assert_eq!(body["succeeded_by"], day(-1).to_string());

    for missing in [day(9).to_string(), day(1).to_string(), "nonsense".into()] {
        let (status, _) = get_json(
            &ctx,
            &format!("/api/v1/instance/terms_of_service/{missing}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    }
}

#[tokio::test]
async fn content_is_escaped_markdown_with_the_domain() {
    let ctx = TestContext::new("tos-content").await;
    insert(
        &ctx.db,
        "## At %{domain}\n\nNo <script>alert(1)</script> and no ![img](https://x.invalid/a.png), 100%% sure.",
        Some(day(-1)),
        true,
    )
    .await;

    let (_, body) = get_json(&ctx, "/api/v1/instance/terms_of_service", None).await;
    let content = body["content"].as_str().unwrap();
    assert!(
        content.starts_with(&format!("<h2>At {}</h2>", ctx.domain)),
        "{content}"
    );
    assert!(content.contains("&lt;script&gt;"), "{content}");
    assert!(!content.contains("<script>"), "{content}");
    assert!(!content.contains("<img"), "{content}");
    assert!(
        content.contains("![img](https://x.invalid/a.png)"),
        "{content}"
    );
    assert!(content.contains("100% sure"), "{content}");
}

/// Ruby's `format(text, domain:)`: a bare `% s` prints the argument hash, and
/// what it raises on is upstream's unrescued exception.
#[tokio::test]
async fn percent_signs_are_read_as_ruby_reads_them() {
    let ctx = TestContext::new("tos-percent").await;
    insert(&ctx.db, "100% sure.", Some(day(-2)), true).await;
    let (status, body) = get_json(&ctx, "/api/v1/instance/terms_of_service", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["content"],
        format!("<p>100{{domain: \"{}\"}}ure.</p>\n", ctx.domain)
    );

    insert(&ctx.db, "At %{domain}, 100% sure.", Some(day(-1)), true).await;
    let (status, body) = get_json(&ctx, "/api/v1/instance/terms_of_service", None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body,
        serde_json::json!({ "status": 500, "error": "Internal Server Error" })
    );

    set_setting(&ctx.db, "site_terms", "\"Ask %{someone}.\"").await;
    let (status, _) = get_json(&ctx, "/api/v1/instance/privacy_policy", None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn configured_terms_are_served_until_a_version_is_published() {
    let ctx = TestContext::with_instance_config("tos-config", |instance| {
        instance.terms_of_service = "Configured *terms*.".into();
    })
    .await;

    let (status, body) = get_json(&ctx, "/api/v1/instance/terms_of_service", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["content"], "<p>Configured <em>terms</em>.</p>\n");
    assert_eq!(body["effective_date"], "2025-01-01");
    assert_eq!(body["effective"], true);
    let (status, _) = get_json(&ctx, "/api/v1/instance/terms_of_service/2025-01-01", None).await;
    assert_eq!(status, StatusCode::OK);

    // A new draft starts from the configured text.
    let (admin_id, admin) =
        seed_user(&ctx.db, &ctx.domain, "tosadmin", "tosadmin@test.invalid").await;
    make_admin(&ctx.db, admin_id).await;
    let (_, draft) = get_json(&ctx, "/api/v1/admin/terms_of_service/draft", Some(&admin)).await;
    assert_eq!(draft["text"], "Configured *terms*.");
    assert!(draft["id"].is_null());

    // Once anything is published, the configuration is no longer read.
    insert(&ctx.db, "Published.", Some(day(3)), true).await;
    let (_, body) = get_json(&ctx, "/api/v1/instance/terms_of_service", None).await;
    assert_eq!(body["content"], "<p>Published.</p>\n");
    let (status, _) = get_json(&ctx, "/api/v1/instance/terms_of_service/2025-01-01", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn privacy_policy_from_the_setting_the_config_or_the_default() {
    let ctx = TestContext::with_instance_config("privacy", |instance| {
        instance.privacy_policy = String::new();
    })
    .await;

    // Mastodon's own policy, about this domain.
    let (status, body) = get_json(&ctx, "/api/v1/instance/privacy_policy", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["updated_at"], "2022-10-07T00:00:00+00:00");
    let content = body["content"].as_str().unwrap();
    assert!(content.contains(&ctx.domain), "{content}");
    assert!(!content.contains("%{domain}"));

    // `site_terms`, when an administrator has written one.
    set_setting(
        &ctx.db,
        "site_terms",
        "\"We keep **nothing** at %{domain}.\"",
    )
    .await;
    let (_, body) = get_json(&ctx, "/api/v1/instance/privacy_policy", None).await;
    assert_eq!(
        body["content"],
        format!(
            "<p>We keep <strong>nothing</strong> at {}.</p>\n",
            ctx.domain
        )
    );
    let updated_at = body["updated_at"].as_str().unwrap();
    assert!(
        updated_at.ends_with('Z') && updated_at.len() == 20,
        "{updated_at}"
    );
}

#[tokio::test]
async fn configured_privacy_policy_stands_in_for_a_blank_setting() {
    let ctx = TestContext::with_instance_config("privacy-config", |instance| {
        instance.privacy_policy = "Configured policy.".into();
    })
    .await;
    set_setting(&ctx.db, "site_terms", "\"\"").await;
    let (_, body) = get_json(&ctx, "/api/v1/instance/privacy_policy", None).await;
    assert_eq!(body["content"], "<p>Configured policy.</p>\n");
}

// ── Administration ────────────────────────────────────────────────────────

async fn admin(ctx: &TestContext) -> (i64, String) {
    let (id, token) = seed_user(&ctx.db, &ctx.domain, "tosadmin", "tosadmin@test.invalid").await;
    make_admin(&ctx.db, id).await;
    (id, token)
}

#[tokio::test]
async fn administration_needs_manage_settings() {
    let ctx = TestContext::new("tos-perm").await;
    for path in [
        "/api/v1/admin/terms_of_service/draft",
        "/api/v1/admin/terms_of_service/history",
        "/api/v1/admin/terms_of_service/generate",
    ] {
        let (status, _) = get_json(&ctx, path, Some(&ctx.alice_token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }
    let resp = ctx
        .api
        .put_json(
            "/api/v1/admin/terms_of_service/draft",
            Some(&ctx.alice_token),
            &json!({"text": "Mine."}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn drafting_and_publishing() {
    let ctx = TestContext::new("tos-draft").await;
    let (admin_id, token) = admin(&ctx).await;

    // Nothing yet: an unsaved draft, effective ten days out.
    let (status, draft) =
        get_json(&ctx, "/api/v1/admin/terms_of_service/draft", Some(&token)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(draft["id"].is_null());
    assert_eq!(draft["text"], "");
    assert_eq!(draft["effective_date"], day(10).to_string());
    let (status, _) = get_json(&ctx, "/api/v1/admin/terms_of_service", Some(&token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Text is required even for a draft.
    let resp = ctx
        .api
        .put_json(
            "/api/v1/admin/terms_of_service/draft",
            Some(&token),
            &json!({"text": "  ", "action_type": "save_draft"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "Validation failed: Text can't be blank");

    let resp = ctx
        .api
        .put_json(
            "/api/v1/admin/terms_of_service/draft",
            Some(&token),
            &json!({"text": "Be *kind*.", "action_type": "save_draft"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let saved: Value = resp.json().await.unwrap();
    let id = saved["id"].as_str().unwrap().to_owned();
    assert!(saved["published_at"].is_null());
    assert_eq!(saved["text_html"], "<p>Be <em>kind</em>.</p>\n");

    // Publishing wants a changelog and a date no earlier than today.
    let resp = ctx
        .api
        .put_json(
            "/api/v1/admin/terms_of_service/draft",
            Some(&token),
            &json!({"effective_date": day(-1).to_string(), "action_type": "publish"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        format!(
            "Validation failed: Changelog can't be blank, Effective date is too soon, must be later than {}",
            today()
        )
    );
    let resp = ctx
        .api
        .put_json(
            "/api/v1/admin/terms_of_service/draft",
            Some(&token),
            &json!({"effective_date": "", "changelog": "New.", "action_type": "publish"}),
        )
        .await;
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Validation failed: Effective date can't be blank"
    );
    // A refused publish leaves the draft a draft.
    let (_, draft) = get_json(&ctx, "/api/v1/admin/terms_of_service/draft", Some(&token)).await;
    assert_eq!(draft["id"], id.as_str());
    assert!(draft["published_at"].is_null());

    let resp = ctx
        .api
        .put_json(
            "/api/v1/admin/terms_of_service/draft",
            Some(&token),
            &json!({
                "effective_date": day(2).to_string(),
                "changelog": "Kindness.",
                "action_type": "publish",
            }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let published: Value = resp.json().await.unwrap();
    assert_eq!(published["id"], id.as_str());
    assert!(published["published_at"].is_string());
    assert!(published["notification_sent_at"].is_null());

    let logged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_action_logs
         WHERE account_id = $1 AND action = 'publish' AND target_type = 'TermsOfService'
           AND target_id = $2",
    )
    .bind(admin_id)
    .bind(id.parse::<i64>().unwrap())
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(logged, 1);

    let (_, current) = get_json(&ctx, "/api/v1/admin/terms_of_service", Some(&token)).await;
    assert_eq!(current["id"], id.as_str());
    let (_, history) = get_json(&ctx, "/api/v1/admin/terms_of_service/history", Some(&token)).await;
    assert_eq!(history.as_array().unwrap().len(), 1);

    // The next draft starts over — from the live text, of which there is
    // none until the published version takes effect.
    let (_, draft) = get_json(&ctx, "/api/v1/admin/terms_of_service/draft", Some(&token)).await;
    assert!(draft["id"].is_null());

    // A date another version has is taken, drafts included.
    let resp = ctx
        .api
        .put_json(
            "/api/v1/admin/terms_of_service/draft",
            Some(&token),
            &json!({"text": "Again.", "effective_date": day(2).to_string()}),
        )
        .await;
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Validation failed: Effective date has already been taken"
    );
}

#[tokio::test]
async fn effective_dates_cannot_precede_the_live_version() {
    let ctx = TestContext::new("tos-min-date").await;
    let (_, token) = admin(&ctx).await;
    insert(&ctx.db, "Live.", Some(today()), true).await;

    let (_, draft) = get_json(&ctx, "/api/v1/admin/terms_of_service/draft", Some(&token)).await;
    assert_eq!(draft["text"], "Live.");

    let resp = ctx
        .api
        .put_json(
            "/api/v1/admin/terms_of_service/draft",
            Some(&token),
            &json!({"effective_date": day(-1).to_string()}),
        )
        .await;
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        format!(
            "Validation failed: Effective date is too soon, must be later than {}",
            today()
        )
    );
}

#[tokio::test]
async fn generating_from_the_template() {
    let ctx = TestContext::new("tos-generate").await;
    let (_, token) = admin(&ctx).await;

    let (status, form) = get_json(
        &ctx,
        "/api/v1/admin/terms_of_service/generate",
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(form["domain"], ctx.domain.as_str());
    assert!(form["jurisdiction"].is_null());

    let resp = ctx
        .api
        .post_json(
            "/api/v1/admin/terms_of_service/generate",
            Some(&token),
            &json!({"domain": ctx.domain, "min_age": "16"}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap().starts_with(
            "Validation failed: Admin email can't be blank, Arbitration address can't be blank"
        ),
        "{body}"
    );

    let fields = json!({
        "admin_email": "legal@example.invalid",
        "arbitration_address": "1 Arbitration Way",
        "arbitration_website": "https://arbitration.invalid",
        "choice_of_law": "Seoul",
        "dmca_address": "2 Copyright Road",
        "dmca_email": "dmca@example.invalid",
        "domain": ctx.domain,
        "jurisdiction": "the Republic of Korea",
        "min_age": "16",
    });
    let resp = ctx
        .api
        .post_json(
            "/api/v1/admin/terms_of_service/generate",
            Some(&token),
            &fields,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let generated: Value = resp.json().await.unwrap();
    let text = generated["text"].as_str().unwrap();
    assert!(
        text.contains(&format!("located at {}", ctx.domain)),
        "generated text"
    );
    assert!(text.contains("at least 16 years old"));
    assert!(text.contains("legal@example.invalid"));
    assert!(!text.contains("%{"));
    assert!(generated["published_at"].is_null());
    assert!(generated["effective_date"].is_null());

    // It is the draft now.
    let (_, draft) = get_json(&ctx, "/api/v1/admin/terms_of_service/draft", Some(&token)).await;
    assert_eq!(draft["id"], generated["id"]);
}

#[tokio::test]
async fn distributing_mails_the_active_and_flags_the_rest() {
    let ctx = TestContext::new("tos-distribute").await;
    let (_, token) = admin(&ctx).await;

    let (recent_id, _) = seed_user(&ctx.db, &ctx.domain, "recent", "recent@test.invalid").await;
    let (stale_id, _) = seed_user(&ctx.db, &ctx.domain, "stale", "stale@test.invalid").await;
    let (suspended_id, _) =
        seed_user(&ctx.db, &ctx.domain, "suspended", "suspended@test.invalid").await;
    let (never_id, _) = seed_user(&ctx.db, &ctx.domain, "never", "never@test.invalid").await;
    for (id, sign_in) in [
        (recent_id, Some("now() - interval '3 days'")),
        (stale_id, Some("now() - interval '2 years'")),
        (suspended_id, Some("now() - interval '1 day'")),
        (never_id, None),
    ] {
        let at = sign_in.unwrap_or("NULL");
        sqlx::query(&format!(
            "UPDATE users SET current_sign_in_at = {at}, created_at = now() - interval '3 years'
             WHERE account_id = $1"
        ))
        .bind(id)
        .execute(&ctx.db)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(suspended_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    // Unpublished versions are not distributed.
    let draft_id = insert(&ctx.db, "Draft.", Some(day(30)), false).await;
    let (status, _) = get_json(
        &ctx,
        &format!("/api/v1/admin/terms_of_service/{draft_id}/preview"),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let id = insert(&ctx.db, "Published.", Some(day(10)), true).await;
    // Signed up after it was published: neither mailed nor flagged.
    let (late_id, _) = seed_user(&ctx.db, &ctx.domain, "late", "late@test.invalid").await;
    sqlx::query("UPDATE users SET current_sign_in_at = now() WHERE account_id = $1")
        .bind(late_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    let tos = eunha::terms_of_service::find(&ctx.db, id).await.unwrap();
    let mailed: Vec<String> = eunha::terms_of_service::scope_for_notification(&ctx.db, &tos)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.email)
        .collect();
    assert_eq!(mailed, vec!["recent@test.invalid".to_owned()]);

    let (status, preview) = get_json(
        &ctx,
        &format!("/api/v1/admin/terms_of_service/{id}/preview"),
        Some(&token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preview["user_count"], 1);

    // Not for anyone without the permission.
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/terms_of_service/{id}/distribution"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/terms_of_service/{id}/test"),
            Some(&token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/terms_of_service/{id}/distribution"),
            Some(&token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(body["notification_sent_at"].is_string());
    // `Admin::DistributeTermsOfServiceNotificationWorker` runs from the queue.
    ctx.state.jobs.settle().await;

    let flagged: Vec<i64> = sqlx::query_scalar(
        "SELECT account_id FROM users WHERE require_tos_interstitial ORDER BY account_id",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    let mut expected = vec![stale_id, suspended_id, never_id];
    expected.sort();
    assert_eq!(flagged, expected);

    // Once is enough.
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/terms_of_service/{id}/distribution"),
            Some(&token),
            &json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_interstitial_shows_until_dismissed() {
    let ctx = TestContext::new("tos-interstitial").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    insert(&ctx.db, "Read me.", Some(day(5)), true).await;

    let path = "/api/eunha/v1/terms_of_service/interstitial";
    let (_, body) = get_json(&ctx, path, Some(&ctx.alice_token)).await;
    assert!(body["terms_of_service"].is_null());

    let user_id = user_id_for(&ctx.db, alice_id).await;
    sqlx::query("UPDATE users SET require_tos_interstitial = true WHERE id = $1")
        .bind(user_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    let (_, body) = get_json(&ctx, path, Some(&ctx.alice_token)).await;
    assert_eq!(body["terms_of_service"]["content"], "<p>Read me.</p>\n");
    assert_eq!(body["terms_of_service"]["effective"], false);

    let resp = ctx.api.delete(path, &ctx.alice_token).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (_, body) = get_json(&ctx, path, Some(&ctx.alice_token)).await;
    assert!(body["terms_of_service"].is_null());

    let (status, _) = get_json(&ctx, path, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
