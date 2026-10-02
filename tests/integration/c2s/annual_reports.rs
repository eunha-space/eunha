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

/// POST /api/v1/annual_reports/{year}/generate for current year returns error (not a completed year).
#[tokio::test]
async fn test_annual_report_generate_current_year_rejected() {
    let ctx = TestContext::new("annrep-curyr").await;
    let current_year = chrono::Utc::now().year();
    let path = format!("/api/v1/annual_reports/{current_year}/generate");

    let resp = ctx
        .api
        .post_json(&path, Some(&ctx.alice_token), &serde_json::json!({}))
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Full lifecycle: generate a report for a past year with activity, read it, mark it read.
#[tokio::test]
async fn test_annual_report_lifecycle() {
    let ctx = TestContext::new("annrep-life").await;

    // Insert a status in the past year (2023) directly via DB
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let status_id = eunha::snowflake::next_id();
    sqlx::query!(
        "INSERT INTO statuses (id, account_id, text, visibility, created_at, updated_at)
         VALUES ($1, $2, 'test post 2023', 0, '2023-06-15T12:00:00Z'::timestamptz, now())",
        status_id,
        alice_id,
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    // State should be eligible
    let resp = ctx
        .api
        .get("/api/v1/annual_reports/2023/state", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["state"].as_str(), Some("eligible"));

    // Generate the report
    let resp = ctx
        .api
        .post_json(
            "/api/v1/annual_reports/2023/generate",
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    // Generated in the background, with an async refresh to poll.
    let header = resp
        .headers()
        .get("mastodon-async-refresh")
        .expect("generate should hand out an async refresh")
        .to_str()
        .unwrap()
        .to_owned();
    assert!(header.starts_with("id=\""), "{header}");
    assert!(header.ends_with(", retry=2"), "{header}");

    // State is "available" once the report is written.
    assert_eq!(wait_for_report(&ctx, 2023).await, "available");

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

    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    // Insert several statuses in 2022
    for i in 0..5 {
        let sid = eunha::snowflake::next_id();
        sqlx::query!(
            "INSERT INTO statuses (id, account_id, text, visibility, created_at, updated_at)
             VALUES ($1, $2, $3, 0, '2022-03-01T12:00:00Z'::timestamptz, now())",
            sid,
            alice_id,
            format!("post number {i}"),
        )
        .execute(&ctx.db)
        .await
        .unwrap();
    }

    ctx.api
        .post_json(
            "/api/v1/annual_reports/2022/generate",
            Some(&ctx.alice_token),
            &serde_json::json!({}),
        )
        .await;
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
