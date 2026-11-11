use chrono::Datelike as _;
use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::TestContext;

/// Polls the report's state until it is no longer being generated.
async fn wait_for_report(ctx: &TestContext, year: i32) -> String {
    for _ in 0..100 {
        let resp = ctx
            .api
            .get(
                &format!("/api/v1/annual_reports/{year}/state"),
                Some(&ctx.alice_token),
            )
            .await;
        let body: Value = resp.json().await.unwrap();
        let state = body["state"].as_str().unwrap_or_default().to_owned();
        if state != "generating" {
            return state;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the annual report was never generated");
}

/// GET /api/v1/annual_reports returns empty wrapped response when no reports.
#[tokio::test]
async fn test_annual_reports_empty() {
    let ctx = TestContext::new("annrep-empty").await;

    let resp = ctx
        .api
        .get("/api/v1/annual_reports", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert!(body["annual_reports"]
        .as_array()
        .is_some_and(|a| a.is_empty()));
    assert!(body["accounts"].as_array().is_some());
    assert!(body["statuses"].as_array().is_some());
}

/// GET /api/v1/annual_reports/{year}/state returns "ineligible" for a year with no activity.
#[tokio::test]
async fn test_annual_report_state_ineligible() {
    let ctx = TestContext::new("annrep-inelig").await;

    let resp = ctx
        .api
        .get("/api/v1/annual_reports/1999/state", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["state"].as_str(), Some("ineligible"));
}

/// `AnnualReport.current_campaign`: the year from 10 to 31 December while
/// the `wrapstodon` setting is on, else none.
fn campaign() -> Option<i32> {
    let now = chrono::Utc::now();
    (now.month() == 12 && now.day() >= 10).then(|| now.year())
}

/// A post of alice's in `year`, its id drawn from then, as Mastodon's ids
/// are, with a hashtag so that the year is eligible.
async fn post_in(ctx: &TestContext, year: i32, text: &str) {
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let at = chrono::NaiveDate::from_ymd_opt(year, 6, 15)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
        .and_utc();
    let status_id = (at.timestamp_millis() << 16) | rand::random::<u16>() as i64;
    sqlx::query(
        "INSERT INTO statuses (id, account_id, text, visibility, created_at, updated_at)
         VALUES ($1, $2, $3, 0, $4, now())",
    )
    .bind(status_id)
    .bind(alice_id)
    .bind(text)
    .bind(at.naive_utc())
    .execute(&ctx.db)
    .await
    .unwrap();
    let tag_id: i64 = sqlx::query_scalar(
        "INSERT INTO tags (name, created_at, updated_at) VALUES ('annualtag', now(), now())
         ON CONFLICT DO NOTHING RETURNING id",
    )
    .fetch_optional(&ctx.db)
    .await
    .unwrap()
    .unwrap_or_default();
    let tag_id = if tag_id == 0 {
        sqlx::query_scalar("SELECT id FROM tags WHERE name = 'annualtag'")
            .fetch_one(&ctx.db)
            .await
            .unwrap()
    } else {
        tag_id
    };
    sqlx::query("INSERT INTO statuses_tags (status_id, tag_id) VALUES ($1, $2)")
        .bind(status_id)
        .bind(tag_id)
        .execute(&ctx.db)
        .await
        .unwrap();
}

/// `GenerateAnnualReportWorker`, run at once.
async fn generate(ctx: &TestContext, year: i32) {
    use eunha::jobs::Job as _;
    eunha::api::mastodon::annual_reports::GenerateAnnualReportWorker {
        account_id: ctx.alice_id.parse().unwrap(),
        year,
    }
    .perform(&ctx.state)
    .await
    .unwrap();
}

/// `generate` outside the campaign renders an empty object and makes
/// nothing, whatever the year.
#[tokio::test]
async fn test_annual_report_generate_outside_the_campaign_is_empty() {
    let ctx = TestContext::new("annrep-curyr").await;
    crate::helpers::set_setting(&ctx.db, "wrapstodon", "false").await;
    post_in(&ctx, 2023, "a #annualtag post").await;

    for year in [chrono::Utc::now().year(), 2023] {
        let resp = ctx
            .api
            .post_json(
                &format!("/api/v1/annual_reports/{year}/generate"),
                Some(&ctx.alice_token),
                &serde_json::json!({}),
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("mastodon-async-refresh").is_none());
        assert_eq!(resp.json::<Value>().await.unwrap(), serde_json::json!({}));
    }
    // A past year with posts is not eligible outside its campaign either.
    let resp = ctx
        .api
        .get("/api/v1/annual_reports/2023/state", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.json::<Value>().await.unwrap()["state"], "ineligible");
    let made: i64 = sqlx::query_scalar("SELECT count(*) FROM generated_annual_reports")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(made, 0);
}

/// During the campaign, this year's report is eligible and generated in the
/// background with an async refresh; outside it, there is nothing to check.
#[tokio::test]
async fn test_annual_report_campaign_generates_this_year() {
    let ctx = TestContext::new("annrep-campaign").await;
    let Some(year) = campaign() else {
        return;
    };
    post_in(&ctx, year, "a #annualtag post").await;
    let resp = ctx
        .api
        .get(
            &format!("/api/v1/annual_reports/{year}/state"),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.json::<Value>().await.unwrap()["state"], "eligible");
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/annual_reports/{year}/generate"),
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let header = resp
        .headers()
        .get("mastodon-async-refresh")
        .expect("generate should hand out an async refresh")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(header.ends_with(", retry=2"), "{header}");
    assert_eq!(wait_for_report(&ctx, year).await, "available");
}

/// A report made by the worker: available, listed, shown, and read.
#[tokio::test]
async fn test_annual_report_lifecycle() {
    let ctx = TestContext::new("annrep-life").await;
    post_in(&ctx, 2023, "test post 2023 #annualtag").await;

    generate(&ctx, 2023).await;
    assert_eq!(wait_for_report(&ctx, 2023).await, "available");
    // `share_key: SecureRandom.hex(8)`.
    let share_key: Option<String> =
        sqlx::query_scalar("SELECT share_key FROM generated_annual_reports WHERE year = 2023")
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(share_key.map(|k| k.len()), Some(16));
    // Made once: generating again leaves it be.
    generate(&ctx, 2023).await;
    let made: i64 = sqlx::query_scalar("SELECT count(*) FROM generated_annual_reports")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(made, 1);
    // `generate` renders empty once the report exists.
    let resp = ctx
        .api
        .post_json(
            "/api/v1/annual_reports/2023/generate",
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // List returns the report
    let resp = ctx
        .api
        .get("/api/v1/annual_reports", Some(&ctx.alice_token))
        .await;
    let body: Value = resp.json().await.unwrap();
    let reports = body["annual_reports"].as_array().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["year"].as_i64(), Some(2023));
    assert!(reports[0]["data"].is_object());
    assert_eq!(reports[0]["data"]["archetype"].as_str(), Some("lurker"));

    // GET /api/v1/annual_reports/2023 also works
    let resp = ctx
        .api
        .get("/api/v1/annual_reports/2023", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["annual_reports"][0]["year"].as_i64(), Some(2023));

    // Mark as read
    let resp = ctx
        .api
        .post_json(
            "/api/v1/annual_reports/2023/read",
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // Now list returns empty (viewed)
    let resp = ctx
        .api
        .get("/api/v1/annual_reports", Some(&ctx.alice_token))
        .await;
    let body: Value = resp.json().await.unwrap();
    assert!(body["annual_reports"].as_array().unwrap().is_empty());
}

/// GET /api/v1/annual_reports/{year} returns 404 for non-existent report.
#[tokio::test]
async fn test_annual_report_get_not_found() {
    let ctx = TestContext::new("annrep-404").await;

    let resp = ctx
        .api
        .get("/api/v1/annual_reports/2020", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Annual report data contains required fields when generated.
#[tokio::test]
async fn test_annual_report_data_structure() {
    let ctx = TestContext::new("annrep-data").await;
    for i in 0..5 {
        post_in(&ctx, 2022, &format!("post number {i} #annualtag")).await;
    }
    generate(&ctx, 2022).await;
    assert_eq!(wait_for_report(&ctx, 2022).await, "available");

    let resp = ctx
        .api
        .get("/api/v1/annual_reports/2022", Some(&ctx.alice_token))
        .await;
    let body: Value = resp.json().await.unwrap();
    let data = &body["annual_reports"][0]["data"];
    assert!(data["archetype"].is_string());
    assert!(data["top_statuses"].is_object());
    assert!(data["time_series"].is_array());
    assert!(data["top_hashtags"].is_array());
}

/// alice's 2023 report and its share key, made by the worker.
async fn shared_report(ctx: &TestContext) -> String {
    post_in(ctx, 2023, "a shared #annualtag post").await;
    generate(ctx, 2023).await;
    sqlx::query_scalar("SELECT share_key FROM generated_annual_reports WHERE year = 2023")
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

/// `REST::AnnualReportSerializer#share_url` is `public_wrapstodon_url`, and
/// null without a share key.
#[tokio::test]
async fn test_annual_report_share_url() {
    let ctx = TestContext::new("annrep-share-url").await;
    let share_key = shared_report(&ctx).await;
    let resp = ctx
        .api
        .get("/api/v1/annual_reports/2023", Some(&ctx.alice_token))
        .await;
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["annual_reports"][0]["share_url"],
        format!("https://{}/@alice/wrapstodon/2023/{share_key}", ctx.domain)
    );

    sqlx::query("UPDATE generated_annual_reports SET share_key = NULL")
        .execute(&ctx.db)
        .await
        .unwrap();
    let resp = ctx
        .api
        .get("/api/v1/annual_reports", Some(&ctx.alice_token))
        .await;
    let body: Value = resp.json().await.unwrap();
    assert!(body["annual_reports"][0]["share_url"].is_null());
}

/// `WrapstodonController#show`: the report for anyone with its share URL,
/// as `REST::AnnualReportsSerializer` with no viewer, and `domain`.
#[tokio::test]
async fn test_shared_annual_report() {
    use eunha::api::mastodon::annual_reports::{shared, Shared};

    let ctx = TestContext::new("annrep-shared").await;
    let share_key = shared_report(&ctx).await;

    // The username is matched case-insensitively, as `find_local!` does.
    let Shared::Found(report) = shared(&ctx.state, None, "ALICE", "2023", &share_key)
        .await
        .unwrap()
    else {
        panic!("the report should be shared");
    };
    let payload = &report.payload;
    assert_eq!(payload["annual_reports"][0]["year"], 2023);
    assert_eq!(payload["annual_reports"][0]["account_id"], ctx.alice_id);
    assert_eq!(payload["accounts"][0]["id"], ctx.alice_id);
    assert!(payload["statuses"].is_array());
    assert_eq!(payload["domain"], ctx.domain);
    // No viewer: no `me`, and the statuses say nothing of one.
    assert!(payload.get("me").is_none());
    for status in payload["statuses"].as_array().unwrap() {
        assert!(status.get("favourited").is_none(), "{status}");
    }

    // The page itself: publicly cached for ten minutes when no one is signed
    // in, and carrying the report once the web app is built.
    let path = format!("/@alice/wrapstodon/2023/{share_key}");
    let resp = ctx.api.get(&path, None).await;
    assert_ne!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(resp.headers()["cache-control"], "max-age=600, public");
    assert_eq!(resp.headers()["vary"], "Accept, Accept-Language, Cookie");
    if resp.status() == StatusCode::OK {
        let html = resp.text().await.unwrap();
        assert!(
            html.contains("<title>Wrapstodon 2023 for alice - "),
            "{html}"
        );
        assert!(html.contains(r#"<meta name="robots" content="noindex, noarchive" />"#));
        assert!(html.contains(r#"<script type="application/json" id="wrapstodon-data">"#));
    }

    // A signed-in viewer is `me`, and the page is not cached for others.
    let resp = ctx.api.get(&path, Some(&ctx.bob_token)).await;
    assert_eq!(resp.headers()["cache-control"], "private, no-store");
    if resp.status() == StatusCode::OK {
        let html = resp.text().await.unwrap();
        assert!(
            html.contains(&format!(r#""me":"{}""#, ctx.bob_id)),
            "{html}"
        );
    }
}

/// In limited federation mode the shared page carries the report only to a
/// signed-in viewer; Mastodon sends anyone else to sign in.
#[tokio::test]
async fn test_shared_annual_report_in_limited_federation_mode() {
    use eunha::api::mastodon::annual_reports::{shared, Shared};

    let ctx = TestContext::with_instance("annrep-shared-lfm", |instance| {
        instance.limited_federation_mode = true;
    })
    .await;
    let share_key = shared_report(&ctx).await;
    assert!(matches!(
        shared(&ctx.state, None, "alice", "2023", &share_key)
            .await
            .unwrap(),
        Shared::SignInRequired
    ));
    let resp = ctx
        .api
        .get(
            &format!("/@alice/wrapstodon/2023/{share_key}"),
            Some(&ctx.bob_token),
        )
        .await;
    assert_ne!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(resp.headers()["cache-control"], "private, no-store");
}

/// The shared page refuses what `find_by!` and `AccountOwnedConcern` refuse.
#[tokio::test]
async fn test_shared_annual_report_refusals() {
    let ctx = TestContext::new("annrep-shared-no").await;
    let share_key = shared_report(&ctx).await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let status_of = |path: String| {
        let ctx = &ctx;
        async move { ctx.api.get(&path, None).await.status() }
    };

    // A wrong share key, year, or account is not found.
    for path in [
        "/@alice/wrapstodon/2023/sharks".to_owned(),
        format!("/@alice/wrapstodon/2024/{share_key}"),
        format!("/@bob/wrapstodon/2023/{share_key}"),
        format!("/@nobody/wrapstodon/2023/{share_key}"),
    ] {
        assert_eq!(
            status_of(path.clone()).await,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }

    let path = format!("/@alice/wrapstodon/2023/{share_key}");
    // `check_account_confirmation`.
    sqlx::query("UPDATE users SET confirmed_at = NULL WHERE account_id = $1")
        .bind(alice_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    assert_eq!(status_of(path.clone()).await, StatusCode::NOT_FOUND);
    sqlx::query("UPDATE users SET confirmed_at = now() WHERE account_id = $1")
        .bind(alice_id)
        .execute(&ctx.db)
        .await
        .unwrap();

    // `check_account_suspension`: gone once the suspension cannot be undone,
    // forbidden while it can.
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(alice_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    assert_eq!(status_of(path.clone()).await, StatusCode::GONE);
    sqlx::query(
        "INSERT INTO account_deletion_requests (account_id, created_at, updated_at)
         VALUES ($1, now(), now())",
    )
    .bind(alice_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    let resp = ctx.api.get(&path, None).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert_eq!(resp.headers()["cache-control"], "max-age=180, public");
}

/// `AnnualReport#generate` with `SCHEMA = 2` and its sources: the most
/// boosted post among those with stats and no other, the hashtag by its
/// display name, the follows made in the year, and the presenter's accounts.
#[tokio::test]
async fn test_annual_report_is_mastodons_schema_2() {
    let ctx = TestContext::new("annrep-schema2").await;
    for i in 0..3 {
        post_in(&ctx, 2023, &format!("post {i} #annualtag")).await;
    }
    sqlx::query("UPDATE tags SET display_name = 'AnnualTag' WHERE name = 'annualtag'")
        .execute(&ctx.db)
        .await
        .unwrap();
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM statuses ORDER BY id")
        .fetch_all(&ctx.db)
        .await
        .unwrap();
    // The first is the most boosted; the second's favourites do not count.
    for (id, reblogs, favourites) in [(ids[0], 5_i64, 0_i64), (ids[1], 1, 100)] {
        sqlx::query(
            "INSERT INTO status_stats (id, status_id, reblogs_count, favourites_count, created_at, updated_at)
             VALUES ($1, $1, $2, $3, now(), now())",
        )
        .bind(id)
        .bind(reblogs)
        .bind(favourites)
        .execute(&ctx.db)
        .await
        .unwrap();
    }
    // A follow made in 2023 counts; one made in 2024 does not.
    let (carol_id, _) =
        crate::helpers::seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    for (at, follower) in [
        ("2023-03-01", ctx.bob_id.parse::<i64>().unwrap()),
        ("2024-03-01", carol_id),
    ] {
        sqlx::query(
            "INSERT INTO follows (id, created_at, updated_at, account_id, target_account_id)
             VALUES ((SELECT COALESCE(max(id), 0) + 1 FROM follows), $1::timestamp, now(), $2, $3)",
        )
        .bind(at)
        .bind(follower)
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    }
    generate(&ctx, 2023).await;

    let body: Value = ctx
        .api
        .get("/api/v1/annual_reports/2023", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let report = &body["annual_reports"][0];
    assert_eq!(report["schema_version"], 2);
    assert_eq!(
        report["data"],
        serde_json::json!({
            "archetype": "lurker",
            "top_statuses": {
                "by_reblogs": ids[0].to_string(),
                "by_favourites": null,
                "by_replies": null,
            },
            "time_series": [{ "month": 12, "statuses": 3, "followers": 1 }],
            "top_hashtags": [{ "name": "AnnualTag", "count": 3 }],
        })
    );
    // `account_ids` for schema 2 is the report's own account.
    let accounts = body["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["id"], ctx.alice_id);
    assert_eq!(body["statuses"][0]["id"], ids[0].to_string());
}

/// `TopHashtags` asks for more than one use, and `TopStatuses` for a post
/// with stats.
#[tokio::test]
async fn test_annual_report_leaves_out_a_single_use_and_unboosted_posts() {
    let ctx = TestContext::new("annrep-single").await;
    post_in(&ctx, 2023, "only #annualtag").await;
    generate(&ctx, 2023).await;
    let data: Value = sqlx::query_scalar("SELECT data FROM generated_annual_reports")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(data["top_hashtags"], serde_json::json!([]));
    assert!(data["top_statuses"]["by_reblogs"].is_null());
}

/// Migration 033: a report eunha labelled schema 1 becomes schema 2, as its
/// data is, and one Mastodon made in 2024 is left as it is.
#[tokio::test]
async fn test_migration_033_labels_eunhas_reports_schema_2() {
    let ctx = TestContext::new("annrep-mig033").await;
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let bob_id: i64 = ctx.bob_id.parse().unwrap();
    let eunhas = serde_json::json!({
        "archetype": "oracle",
        "top_statuses": { "by_reblogs": "1", "by_favourites": "2", "by_replies": "3" },
        "time_series": [{ "month": 12, "statuses": 3, "followers": 0 }],
        "top_hashtags": [],
    });
    let mastodons = serde_json::json!({
        "top_statuses": { "by_reblogs": "1", "by_favourites": "2", "by_replies": "3" },
        "most_reblogged_accounts": [],
        "commonly_interacted_with_accounts": [],
    });
    for (account_id, data) in [(alice_id, &eunhas), (bob_id, &mastodons)] {
        sqlx::query(
            "INSERT INTO generated_annual_reports (account_id, year, data, schema_version, created_at, updated_at)
             VALUES ($1, 2024, $2, 1, now(), now())",
        )
        .bind(account_id)
        .bind(data)
        .execute(&ctx.db)
        .await
        .unwrap();
    }
    sqlx::raw_sql(include_str!(
        "../../../migrations/033_annual_report_schema.sql"
    ))
    .execute(&ctx.db)
    .await
    .unwrap();

    let rows: Vec<(i64, i32, Value)> = sqlx::query_as(
        "SELECT account_id, schema_version, data FROM generated_annual_reports ORDER BY account_id",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    let alice = rows.iter().find(|r| r.0 == alice_id).unwrap();
    assert_eq!(alice.1, 2);
    assert_eq!(
        alice.2["top_statuses"],
        serde_json::json!({ "by_reblogs": "1", "by_favourites": null, "by_replies": null })
    );
    assert_eq!(alice.2["archetype"], "oracle");
    let bob = rows.iter().find(|r| r.0 == bob_id).unwrap();
    assert_eq!((bob.1, &bob.2), (1, &mastodons));
}
