//! An instance a hosting service runs (`[instance] hosted = true`): its
//! administrators are shown nothing about the server they cannot tend.

use super::*;

#[tokio::test]
async fn test_hosted_instance_hides_the_server() {
    let ctx = TestContext::with_config("srv-hosted", |config| {
        config.instance.hosted = true;
        // Configured all the same, as a copied configuration might be.
        config.software_update_url = Some("http://127.0.0.1:9/update-check".into());
    })
    .await;
    make_admin(&ctx).await;
    sqlx::query(
        "INSERT INTO software_updates (version, type, urgent, release_notes, created_at, updated_at)
         VALUES ('99.0.0', 2, true, '', now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    // The software updates and server health pages are not there.
    for path in [
        "/api/v1/admin/software_updates",
        "/api/v1/admin/server_checks",
    ] {
        assert_eq!(
            ctx.api.get(path, Some(&ctx.alice_token)).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    assert_eq!(
        ctx.api
            .post_json(
                "/api/v1/admin/server_checks/database_schema/ignore",
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    // The dashboard warns only of what the administrators can fix.
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
    assert_eq!(keys, ["rules_check"]);

    // Nor does it show the software or the space the server uses.
    let dims = json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/dimensions",
                Some(&ctx.alice_token),
                &json!({"keys": ["software_versions", "space_usage", "languages"]}),
            )
            .await,
    )
    .await;
    let keys: Vec<&str> = dims
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["languages"]);

    // The scheduled health checks find nothing to record.
    eunha::server_checks::run_scheduled(&ctx.state)
        .await
        .unwrap();
    let recorded: i64 = sqlx::query_scalar("SELECT count(*) FROM admin_server_checks")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(recorded, 0);

    // A self-destruct value, even a valid one, does nothing.
    let mut instance = ctx.state.instance.as_ref().clone();
    instance.self_destruct = Some(eunha::self_destruct::value(&instance));
    assert!(!eunha::self_destruct::enabled(&instance));
    instance.hosted = false;
    assert!(eunha::self_destruct::enabled(&instance));
}

/// `[instance.hosting_provider]` credits the host on the About page, through
/// `/api/eunha/v1/instance`, which anyone may read.
#[tokio::test]
async fn test_hosting_provider_credit() {
    let ctx = TestContext::new("srv-no-host").await;
    let body = json_ok(ctx.api.get("/api/eunha/v1/instance", None).await).await;
    assert_eq!(body, json!({"hosting_provider": null}));

    let ctx = TestContext::with_config("srv-host", |config| {
        config.instance.hosted = true;
        config.instance.disallow_unauthenticated_api_access = true;
        config.instance.hosting_provider = Some(eunha::config::HostingProvider {
            name: "eunha space".into(),
            url: Some("https://eunha.space/".into()),
        });
    })
    .await;
    let body = json_ok(ctx.api.get("/api/eunha/v1/instance", None).await).await;
    assert_eq!(
        body,
        json!({"hosting_provider": {"name": "eunha space", "url": "https://eunha.space/"}})
    );

    // A link that is not a web address is not offered.
    let ctx = TestContext::with_config("srv-host-js", |config| {
        config.instance.hosting_provider = Some(eunha::config::HostingProvider {
            name: "Somewhere".into(),
            url: Some("javascript:alert(1)".into()),
        });
    })
    .await;
    let body = json_ok(ctx.api.get("/api/eunha/v1/instance", None).await).await;
    assert_eq!(
        body,
        json!({"hosting_provider": {"name": "Somewhere", "url": null}})
    );
}
