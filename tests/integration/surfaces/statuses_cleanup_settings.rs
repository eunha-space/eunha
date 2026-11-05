//! A member's automated post deletion settings. *maintenance.rs* has what the
//! job does with them.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

const PATH: &str = "/api/eunha/v1/statuses_cleanup";

async fn show(ctx: &TestContext) -> Value {
    ctx.api
        .get(PATH, Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap()
}

async fn patch(ctx: &TestContext, body: Value) -> reqwest::Response {
    ctx.api
        .patch_json(PATH, Some(&ctx.alice_token), &body)
        .await
}

#[tokio::test]
async fn an_account_without_a_policy_is_shown_the_defaults_off() {
    let ctx = TestContext::new("cleanup-defaults").await;
    let policy = show(&ctx).await;
    assert_eq!(policy["enabled"], false);
    assert_eq!(policy["min_status_age"], 1_209_600);
    assert_eq!(policy["keep_direct"], true);
    assert_eq!(policy["keep_pinned"], true);
    assert_eq!(policy["keep_polls"], false);
    assert_eq!(policy["keep_media"], false);
    assert_eq!(policy["keep_self_fav"], true);
    assert_eq!(policy["keep_self_bookmark"], true);
    assert!(policy["min_favs"].is_null());
    assert!(policy["min_reblogs"].is_null());

    // Looking saves nothing.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM account_statuses_cleanup_policies")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(rows, 0);

    assert_eq!(
        ctx.api.get(PATH, None).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn saving_stores_what_the_job_reads_and_a_member_can_turn_it_off() {
    let ctx = TestContext::new("cleanup-save").await;
    let account_id: i64 = ctx.alice_id.parse().unwrap();

    let resp = patch(
        &ctx,
        json!({ "enabled": true, "min_status_age": 604_800, "keep_media": true, "min_favs": 3 }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let policy = eunha::statuses_cleanup::find_policy(&ctx.state, account_id)
        .await
        .unwrap()
        .unwrap();
    assert!(policy.enabled);
    assert_eq!(policy.min_status_age, 604_800);
    assert!(policy.keep_media);
    assert_eq!(policy.min_favs, Some(3));
    // What was not named keeps its default.
    assert!(policy.keep_pinned);

    // A blank threshold clears it, and what is not named stays.
    let body: Value = patch(&ctx, json!({ "min_favs": "" }))
        .await
        .json()
        .await
        .unwrap();
    assert!(body["min_favs"].is_null());
    assert_eq!(body["keep_media"], true);

    // Turning it off is what an imported policy's owner needs.
    let body: Value = patch(&ctx, json!({ "enabled": false }))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["enabled"], false);
    let policy = eunha::statuses_cleanup::find_policy(&ctx.state, account_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!policy.enabled);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM account_statuses_cleanup_policies")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(rows, 1, "saved again in place");
}

#[tokio::test]
async fn only_the_models_values_are_taken() {
    let ctx = TestContext::new("cleanup-invalid").await;
    for body in [
        json!({ "min_status_age": 60 }),
        json!({ "min_status_age": null }),
        json!({ "min_favs": 0 }),
        json!({ "min_reblogs": "many" }),
    ] {
        let resp = patch(&ctx, body.clone()).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM account_statuses_cleanup_policies")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(rows, 0, "a refused form saves nothing");
}

/// A member's own policy reaches only their own account.
#[tokio::test]
async fn a_policy_is_its_owners() {
    let ctx = TestContext::new("cleanup-owner").await;
    patch(&ctx, json!({ "enabled": true })).await;
    let bobs: Value = ctx
        .api
        .get(PATH, Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(bobs["enabled"], false);
}

/// Widening a policy makes the next run look at what the last one kept.
#[tokio::test]
async fn widening_forgets_how_far_the_job_had_looked() {
    let ctx = TestContext::new("cleanup-widen").await;
    let account_id: i64 = ctx.alice_id.parse().unwrap();
    patch(&ctx, json!({ "enabled": true })).await;
    let policy = eunha::statuses_cleanup::find_policy(&ctx.state, account_id)
        .await
        .unwrap()
        .unwrap();
    policy.record_last_inspected(&ctx.state, 12345).await;

    patch(&ctx, json!({ "keep_media": true })).await;
    assert_eq!(policy.last_inspected(&ctx.state).await, Some(12345));

    patch(&ctx, json!({ "keep_pinned": false })).await;
    assert_eq!(policy.last_inspected(&ctx.state).await, None);
}
