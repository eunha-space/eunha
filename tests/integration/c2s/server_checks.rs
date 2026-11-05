//! The Server Health page's checks.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{grant_admin_scopes, make_admin, TestContext};

async fn checks(ctx: &TestContext) -> Vec<Value> {
    let response = ctx
        .api
        .get("/api/v1/admin/server_checks", Some(&ctx.alice_token))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

fn check<'a>(all: &'a [Value], key: &str) -> &'a Value {
    all.iter().find(|c| c["key"] == key).unwrap()
}

#[tokio::test]
async fn checks_run_on_schedule_and_can_be_ignored() {
    let ctx = TestContext::new("server_checks").await;
    let alice = ctx.alice_id.parse().unwrap();
    make_admin(&ctx.db, alice).await;
    grant_admin_scopes(&ctx.db, alice).await;

    let before = checks(&ctx).await;
    let keys: Vec<&str> = before.iter().map(|c| c["key"].as_str().unwrap()).collect();
    assert_eq!(
        keys,
        [
            "database_schema",
            "elastic_search",
            "media_privacy",
            "rules",
            "sidekiq_process",
            "software_version"
        ]
    );
    assert_eq!(check(&before, "rules")["category"], "unprocessed");
    // The synchronous check has run for this answer, and is not behind.
    assert_eq!(check(&before, "sidekiq_process")["category"], "completed");

    eunha::server_checks::run_scheduled(&ctx.state)
        .await
        .unwrap();
    let after = checks(&ctx).await;
    let rules = check(&after, "rules");
    assert_eq!(rules["category"], "warning");
    assert_eq!(rules["title"], "Set up server rules");
    assert_eq!(rules["setup_url"], "/admin/rules");
    assert!(rules["checked_at"].is_string());
    assert_eq!(check(&after, "database_schema")["category"], "completed");
    assert_eq!(check(&after, "elastic_search")["category"], "completed");
    assert_eq!(check(&after, "software_version")["category"], "completed");

    let ignore = "/api/v1/admin/server_checks/rules/ignore";
    let response = ctx
        .api
        .post_json(ignore, Some(&ctx.alice_token), &json!({}))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(check(&checks(&ctx).await, "rules")["category"], "ignored");
    let response = ctx.api.delete(ignore, &ctx.alice_token).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(check(&checks(&ctx).await, "rules")["category"], "warning");

    // A rule set, and the next run has it.
    sqlx::query(
        "INSERT INTO rules (text, created_at, updated_at) VALUES ('Be kind', now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    eunha::server_checks::run_scheduled(&ctx.state)
        .await
        .unwrap();
    assert_eq!(check(&checks(&ctx).await, "rules")["category"], "completed");

    // `NoopRunner#authorized?`.
    let response = ctx
        .api
        .post_json(
            "/api/v1/admin/server_checks/nothing/ignore",
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn jobs_nothing_is_working_show_as_a_missing_queue() {
    let ctx = TestContext::new("server_checks_queue").await;
    let alice = ctx.alice_id.parse().unwrap();
    make_admin(&ctx.db, alice).await;
    grant_admin_scopes(&ctx.db, alice).await;
    sqlx::query(
        "INSERT INTO eunha.jobs (queue, kind, args, run_at, max_retries, keep_dead)
         VALUES ('pull', 'Probe', '[]', now() - interval '1 hour', 0, false)",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let all = checks(&ctx).await;
    let queues = check(&all, "sidekiq_process");
    assert_eq!(queues["category"], "critical");
    assert_eq!(queues["data"]["missing_queues"], json!(["pull"]));
    assert!(queues["lead"].as_str().unwrap().contains("pull"));
}

#[tokio::test]
async fn the_page_is_for_those_who_can_view_the_dashboard() {
    let ctx = TestContext::new("server_checks_denied").await;
    let bob = ctx.bob_id.parse().unwrap();
    grant_admin_scopes(&ctx.db, bob).await;
    let response = ctx
        .api
        .get("/api/v1/admin/server_checks", Some(&ctx.bob_token))
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = ctx
        .api
        .post_json(
            "/api/v1/admin/server_checks/rules/ignore",
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
