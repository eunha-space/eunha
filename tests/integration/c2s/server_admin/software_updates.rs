//! `Admin::SoftwareUpdatesController`.

use super::*;

const VIEW_DEVOPS: i64 = 1 << 1;

async fn record(ctx: &TestContext, version: &str, kind: i32, urgent: bool) {
    sqlx::query(
        "INSERT INTO software_updates (version, type, urgent, release_notes, created_at, updated_at)
         VALUES ($1, $2, $3, 'https://example/' || $1, now(), now())",
    )
    .bind(version)
    .bind(kind)
    .bind(urgent)
    .execute(&ctx.db)
    .await
    .unwrap();
}

/// Without an update server configured the page is not there, as upstream's
/// `check_enabled!` makes it a 404.
#[tokio::test]
async fn test_software_updates_need_the_check() {
    let ctx = TestContext::new("srv-sw-off").await;
    make_admin(&ctx).await;
    assert_eq!(
        ctx.api
            .get("/api/v1/admin/software_updates", Some(&ctx.alice_token))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

/// With it, the recorded releases newer than the one implemented, by
/// version, for `view_devops`.
#[tokio::test]
async fn test_software_updates() {
    let ctx = TestContext::with_config("srv-sw-on", |config| {
        config.software_update_url = Some("http://127.0.0.1:9/update-check".into());
    })
    .await;
    record(&ctx, "4.9.0", 1, false).await;
    record(&ctx, "4.7.0", 0, false).await;
    record(&ctx, "4.7.9", 0, true).await;
    crate::helpers::grant_admin_scopes(&ctx.db, id(&ctx.bob_id)).await;
    assert_eq!(
        ctx.api
            .get("/api/v1/admin/software_updates", Some(&ctx.bob_token))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    give_role(&ctx, &ctx.bob_id, 10, VIEW_DEVOPS).await;
    let updates = json_ok(
        ctx.api
            .get("/api/v1/admin/software_updates", Some(&ctx.bob_token))
            .await,
    )
    .await;
    assert_eq!(updates["current_version"], eunha::version::MASTODON);
    let versions: Vec<&str> = updates["updates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["version"].as_str().unwrap())
        .collect();
    assert_eq!(versions, ["4.7.9", "4.9.0"]);
    assert_eq!(updates["updates"][0]["urgent"], true);
    assert_eq!(updates["updates"][1]["type"], "minor");
}
