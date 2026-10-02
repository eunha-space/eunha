//! `Admin::DashboardController`.

use super::*;

const VIEW_DASHBOARD: i64 = 1 << 3;

/// The counts of what waits, and the system checks the role may see: no
/// rules yet, and a newer release recorded.
#[tokio::test]
async fn test_dashboard() {
    let ctx = TestContext::with_config("srv-dashboard", |config| {
        config.software_update_url = Some("http://127.0.0.1:9/update-check".into());
    })
    .await;
    crate::helpers::grant_admin_scopes(&ctx.db, id(&ctx.bob_id)).await;
    assert_eq!(
        ctx.api
            .get("/api/v1/admin/dashboard", Some(&ctx.bob_token))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );

    // A report and a pending user.
    json_ok(
        ctx.api
            .post_json(
                "/api/v1/reports",
                Some(&ctx.bob_token),
                &json!({"account_id": ctx.alice_id}),
            )
            .await,
    )
    .await;
    sqlx::query("UPDATE users SET approved = false WHERE account_id = $1")
        .bind(id(&ctx.alice_id))
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO software_updates (version, type, urgent, release_notes, created_at, updated_at)
         VALUES ('99.0.0', 2, false, '', now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    // A moderator with the dashboard alone sees the counts and no checks.
    give_role(&ctx, &ctx.bob_id, 10, VIEW_DASHBOARD).await;
    let dashboard = json_ok(
        ctx.api
            .get("/api/v1/admin/dashboard", Some(&ctx.bob_token))
            .await,
    )
    .await;
    assert_eq!(dashboard["pending_reports_count"], 1);
    assert_eq!(dashboard["pending_users_count"], 1);
    assert_eq!(dashboard["pending_appeals_count"], 0);
    assert_eq!(dashboard["system_checks"], json!([]));

    // An administrator also sees the checks.
    sqlx::query("UPDATE users SET approved = true WHERE account_id = $1")
        .bind(id(&ctx.alice_id))
        .execute(&ctx.db)
        .await
        .unwrap();
    make_admin(&ctx).await;
    let dashboard = json_ok(
        ctx.api
            .get("/api/v1/admin/dashboard", Some(&ctx.alice_token))
            .await,
    )
    .await;
    let keys: Vec<&str> = dashboard["system_checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["software_version_check", "rules_check"]);
    assert_eq!(dashboard["system_checks"][1]["action"], "/admin/rules");

    // A rule settles that check.
    json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/rules",
                Some(&ctx.alice_token),
                &json!({"text": "Be kind"}),
            )
            .await,
    )
    .await;
    let dashboard = json_ok(
        ctx.api
            .get("/api/v1/admin/dashboard", Some(&ctx.alice_token))
            .await,
    )
    .await;
    assert_eq!(dashboard["system_checks"].as_array().unwrap().len(), 1);
}

/// A media bucket that answers with a listing fails the privacy check.
#[tokio::test]
async fn test_media_privacy_check() {
    use axum::{routing::any, Router};
    let app = Router::new().fallback(any(|| async {
        "<?xml version=\"1.0\"?><ListBucketResult><Name>media</Name></ListBucketResult>"
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://{addr}");
    let ctx = TestContext::with_config("srv-media-privacy", move |config| {
        config.media_storage.base_url = base;
    })
    .await;
    make_admin(&ctx).await;
    let dashboard = json_ok(
        ctx.api
            .get("/api/v1/admin/dashboard", Some(&ctx.alice_token))
            .await,
    )
    .await;
    let check = dashboard["system_checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["key"] == "upload_check_privacy_error_object_storage")
        .expect("the privacy check fails");
    assert_eq!(check["critical"], true);
}
