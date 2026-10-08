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
