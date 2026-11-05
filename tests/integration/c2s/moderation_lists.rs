//! Moderation list subscriptions: reading a list, applying what it advises or
//! suggesting it, retracting what it stops listing.

use std::sync::{Arc, Mutex};

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::{grant_admin_scopes, make_admin, TestContext};
use eunha::moderation::lists;

async fn context(label: &str) -> TestContext {
    let ctx = TestContext::with_config(label, |config| {
        config.allowed_private_networks = vec!["127.0.0.0/8".into()];
        config.instance.experimental_features = vec!["moderation_subscriptions".into()];
    })
    .await;
    make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    grant_admin_scopes(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    ctx
}

/// A server publishing a list, which the test can change.
async fn list_server(body: &str) -> (String, Arc<Mutex<String>>) {
    let list = Arc::new(Mutex::new(body.to_owned()));
    let served = list.clone();
    let app = axum::Router::new().route(
        "/list.csv",
        axum::routing::get(move || {
            let served = served.clone();
            async move { served.lock().unwrap().clone() }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/list.csv"), list)
}

async fn subscribe(ctx: &TestContext, url: &str, form: Value) -> String {
    let mut body = json!({"name": "Shared list", "url": url, "priority": 10});
    body.as_object_mut()
        .unwrap()
        .extend(form.as_object().unwrap().clone());
    let response = ctx
        .api
        .post_json(
            "/api/v1/admin/moderation_subscriptions",
            Some(&ctx.alice_token),
            &body,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let created: Value = response.json().await.unwrap();
    created["id"].as_str().unwrap().to_owned()
}

async fn block_of(ctx: &TestContext, domain: &str) -> Option<(i32, Option<i64>)> {
    sqlx::query_as(
        "SELECT severity, moderation_subscription_id FROM domain_blocks WHERE domain = $1",
    )
    .bind(domain)
    .fetch_optional(&ctx.db)
    .await
    .unwrap()
}

async fn logs_count(ctx: &TestContext) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM admin_action_logs WHERE account_id = -99")
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

async fn suggestions(ctx: &TestContext) -> Value {
    ctx.api
        .get(
            "/api/v1/admin/moderation_suggestions",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn the_feature_off_answers_not_found() {
    let ctx = TestContext::new("mod_lists_off").await;
    make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    grant_admin_scopes(&ctx.db, ctx.alice_id.parse().unwrap()).await;
    let response = ctx
        .api
        .get(
            "/api/v1/admin/moderation_subscriptions",
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    // And nothing is fetched by the scheduler.
    lists::run_scheduled(&ctx.state).await.unwrap();
}

#[tokio::test]
async fn subscriptions_are_validated_and_for_those_managing_federation() {
    let ctx = context("mod_lists_crud").await;
    let path = "/api/v1/admin/moderation_subscriptions";
    let refused = ctx
        .api
        .post_json(path, Some(&ctx.bob_token), &json!({"name": "x"}))
        .await;
    assert!(matches!(
        refused.status(),
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED
    ));
    for bad in [
        json!({"url": "https://list.example/l.csv", "priority": 1}),
        json!({"name": "n", "url": "ftp://list.example/", "priority": 1}),
        json!({"name": "n", "url": "https://list.example/l.csv", "priority": 2147483648_i64}),
        json!({"name": "n", "url": "https://list.example/l.csv", "priority": 1, "apply_conditions": "sometimes"}),
    ] {
        let response = ctx.api.post_json(path, Some(&ctx.alice_token), &bad).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
    }
    let id = subscribe(
        &ctx,
        "https://list.example/l.csv",
        json!({"apply_conditions": "always", "list_action": "reject"}),
    )
    .await;
    let shown: Value = ctx
        .api
        .get(&format!("{path}/{id}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(shown["subscription"]["type"], "csv_list");
    assert_eq!(shown["subscription"]["apply_conditions"], "always");
    assert_eq!(shown["subscription"]["list_action"], "reject");
    assert_eq!(shown["advisories"], json!([]));

    let updated: Value = ctx
        .api
        .patch_json(
            &format!("{path}/{id}"),
            Some(&ctx.alice_token),
            &json!({"list_action": "", "apply_conditions": "safely", "name": "Renamed"}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(updated["name"], "Renamed");
    assert_eq!(updated["list_action"], Value::Null);
    assert_eq!(updated["apply_conditions"], "safely");

    let response = ctx
        .api
        .delete(&format!("{path}/{id}"), &ctx.alice_token)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let logged: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM admin_action_logs WHERE target_type = 'ModerationSubscription' ORDER BY id",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(logged, ["create", "update", "destroy"]);
}

#[tokio::test]
async fn an_automatic_subscription_applies_and_retracts() {
    let ctx = context("mod_lists_auto").await;
    let (url, list) =
        list_server("#domain,#severity\nbad.example,suspend\nmeh.example,silence\n").await;
    let id: i64 = subscribe(&ctx, &url, json!({"apply_conditions": "always"}))
        .await
        .parse()
        .unwrap();

    lists::run_scheduled(&ctx.state).await.unwrap();
    assert_eq!(block_of(&ctx, "bad.example").await, Some((1, Some(id))));
    assert_eq!(block_of(&ctx, "meh.example").await, Some((0, Some(id))));
    assert_eq!(suggestions(&ctx).await, json!([]));
    assert_eq!(logs_count(&ctx).await, 2);

    // Nothing changed, nothing is done again.
    lists::run_scheduled(&ctx.state).await.unwrap();
    assert_eq!(logs_count(&ctx).await, 2);

    // Audit log entries name what was automatic, and the subscription.
    let logs: Value = ctx
        .api
        .get(
            "/api/v1/admin/moderation_subscriptions/logs",
            Some(&ctx.alice_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        logs[0]["template"],
        "Automatically blocked domain %{target}"
    );
    assert_eq!(logs[0]["moderation_subscription"]["name"], "Shared list");

    *list.lock().unwrap() = "#domain,#severity\nmeh.example,suspend\n".to_owned();
    lists::run_scheduled(&ctx.state).await.unwrap();
    assert_eq!(block_of(&ctx, "bad.example").await, None);
    assert_eq!(block_of(&ctx, "meh.example").await, Some((1, Some(id))));

    // Deleting the subscription retracts the rest of it.
    let response = ctx
        .api
        .delete(
            &format!("/api/v1/admin/moderation_subscriptions/{id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(block_of(&ctx, "meh.example").await, None);
}

#[tokio::test]
async fn an_automatic_subscription_leaves_manual_blocks_unless_told_otherwise() {
    let ctx = context("mod_lists_manual").await;
    sqlx::query(
        "INSERT INTO domain_blocks (domain, severity, created_at, updated_at)
         VALUES ('manual.example', 0, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let (url, _list) = list_server("#domain,#severity\nmanual.example,suspend\n").await;
    let id: i64 = subscribe(&ctx, &url, json!({"apply_conditions": "always"}))
        .await
        .parse()
        .unwrap();
    lists::run_scheduled(&ctx.state).await.unwrap();
    assert_eq!(block_of(&ctx, "manual.example").await, Some((0, None)));
    assert_eq!(suggestions(&ctx).await[0]["target_key"], "manual.example");

    ctx.api
        .patch_json(
            &format!("/api/v1/admin/moderation_subscriptions/{id}"),
            Some(&ctx.alice_token),
            &json!({"override_unattributed": true}),
        )
        .await;
    lists::run_scheduled(&ctx.state).await.unwrap();
    assert_eq!(block_of(&ctx, "manual.example").await, Some((1, Some(id))));
}

#[tokio::test]
async fn a_safe_subscription_suggests_what_would_break_follows() {
    let ctx = context("mod_lists_safe").await;
    let remote_id: i64 = 987_654_321;
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, inbox_url, actor_type, created_at, updated_at)
           VALUES ($1, 'friend', 'friends.example', '', '', 'https://friends.example/friend',
                   'https://friends.example/friend', 'https://friends.example/inbox', 'Person', now(), now())"#,
    )
    .bind(remote_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, now(), now())",
    )
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .bind(remote_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    let (url, _list) = list_server("friends.example\nstrangers.example\n").await;
    subscribe(&ctx, &url, json!({"apply_conditions": "safely"})).await;
    lists::run_scheduled(&ctx.state).await.unwrap();
    assert!(block_of(&ctx, "friends.example").await.is_none());
    assert!(block_of(&ctx, "strangers.example").await.is_some());
    let waiting = suggestions(&ctx).await;
    assert_eq!(waiting.as_array().unwrap().len(), 1);
    assert_eq!(waiting[0]["target_key"], "friends.example");
}

#[tokio::test]
async fn suggestions_are_mailed_confirmed_applied_and_dismissed() {
    let ctx = context("mod_lists_suggest").await;
    let (url, _list) =
        list_server("#domain,#severity\nbad.example,suspend\nmeh.example,limit\n").await;
    subscribe(&ctx, &url, json!({"apply_conditions": "never"})).await;
    lists::run_scheduled(&ctx.state).await.unwrap();

    assert!(block_of(&ctx, "bad.example").await.is_none());
    assert!(ctx
        .mail_to("alice@test.invalid", "New moderation suggestions")
        .await
        .is_some());
    let waiting = suggestions(&ctx).await;
    assert_eq!(waiting.as_array().unwrap().len(), 2);
    assert_eq!(waiting[0]["target_key"], "bad.example");
    assert_eq!(waiting[0]["current_state"], "accept");
    assert_eq!(waiting[0]["suggestions"][0]["state"], "mailed");
    assert_eq!(waiting[0]["advisories"][0]["action"], "reject");

    let id = waiting[0]["suggestions"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let apply = format!("/api/v1/admin/moderation_suggestions/{id}/apply");
    let asked: Value = ctx
        .api
        .post_json(&apply, Some(&ctx.alice_token), &json!({}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(asked["result"], "reject_confirmation_required");
    assert!(block_of(&ctx, "bad.example").await.is_none());
    let done: Value = ctx
        .api
        .post_json(&apply, Some(&ctx.alice_token), &json!({"confirm": true}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(done["result"], "applied");
    assert_eq!(block_of(&ctx, "bad.example").await.map(|b| b.0), Some(1));

    let id = waiting[1]["suggestions"][0]["id"].as_str().unwrap();
    let response = ctx
        .api
        .delete(
            &format!("/api/v1/admin/moderation_suggestions/{id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(suggestions(&ctx).await, json!([]));

    // A dismissed suggestion stays so when the list is read again.
    lists::run_scheduled(&ctx.state).await.unwrap();
    assert_eq!(suggestions(&ctx).await, json!([]));
}

#[tokio::test]
async fn a_manual_retraction_is_suggested_and_a_downgrade_confirmed() {
    let ctx = context("mod_lists_retract").await;
    let (url, list) = list_server("#domain,#severity\nbad.example,suspend\n").await;
    let id: i64 = subscribe(
        &ctx,
        &url,
        json!({"apply_conditions": "always", "retract_automatically": false}),
    )
    .await
    .parse()
    .unwrap();
    lists::run_scheduled(&ctx.state).await.unwrap();
    assert_eq!(block_of(&ctx, "bad.example").await, Some((1, Some(id))));

    // Not downgraded automatically, since this does not retract automatically.
    *list.lock().unwrap() = "#domain,#severity\nbad.example,limit\n".to_owned();
    lists::run_scheduled(&ctx.state).await.unwrap();
    assert_eq!(block_of(&ctx, "bad.example").await, Some((1, Some(id))));
    let waiting = suggestions(&ctx).await;
    assert_eq!(waiting[0]["current_state"], "reject");
    let suggestion = waiting[0]["suggestions"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let apply = format!("/api/v1/admin/moderation_suggestions/{suggestion}/apply");
    let asked: Value = ctx
        .api
        .post_json(&apply, Some(&ctx.alice_token), &json!({}))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(asked["result"], "downgrade_confirmation_required");
    let done: Value = ctx
        .api
        .post_json(
            &apply,
            Some(&ctx.alice_token),
            &json!({"confirm_downgrade": true}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(done["result"], "applied");
    assert_eq!(block_of(&ctx, "bad.example").await.map(|b| b.0), Some(0));

    // Taken off the list, it waits for someone to retract it.
    *list.lock().unwrap() = "#domain,#severity\nother.example,limit\n".to_owned();
    lists::run_scheduled(&ctx.state).await.unwrap();
    assert!(block_of(&ctx, "bad.example").await.is_some());
    let waiting = suggestions(&ctx).await;
    let retraction = waiting
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["target_key"] == "bad.example")
        .unwrap();
    let entry = retraction["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["action"] == "retract")
        .unwrap();
    let id = entry["id"].as_str().unwrap();
    let done: Value = ctx
        .api
        .post_json(
            &format!("/api/v1/admin/moderation_suggestions/{id}/apply"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(done["result"], "retracted");
    assert!(block_of(&ctx, "bad.example").await.is_none());
}

#[tokio::test]
async fn the_lower_priority_number_wins() {
    let ctx = context("mod_lists_priority").await;
    let (first, _a) = list_server("#domain,#severity\nboth.example,silence\n").await;
    let (second, _b) = list_server("#domain,#severity\nboth.example,suspend\n").await;
    let winner: i64 = subscribe(
        &ctx,
        &first,
        json!({"priority": 1, "apply_conditions": "never"}),
    )
    .await
    .parse()
    .unwrap();
    let other = subscribe(
        &ctx,
        &second,
        json!({"priority": 2, "apply_conditions": "always"}),
    )
    .await;
    lists::run_scheduled(&ctx.state).await.unwrap();
    // The lower-priority list conflicts with the winner's, so it only suggests.
    assert!(block_of(&ctx, "both.example").await.is_none());
    let waiting = suggestions(&ctx).await;
    let advisories = waiting[0]["advisories"].as_array().unwrap();
    assert_eq!(advisories[0]["subscription"]["id"], winner.to_string());
    assert_eq!(advisories[1]["subscription"]["id"], other);
}

#[tokio::test]
async fn a_list_that_cannot_be_read_changes_nothing() {
    let ctx = context("mod_lists_failed").await;
    let (url, _list) = list_server("#domain,#severity\nbad.example,limit\n").await;
    let id: i64 = subscribe(&ctx, &url, json!({})).await.parse().unwrap();
    lists::run_scheduled(&ctx.state).await.unwrap();
    let before: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moderation_subscription_advisories WHERE moderation_subscription_id = $1",
    )
    .bind(id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(before, 1);
    sqlx::query("UPDATE moderation_subscriptions SET url = $1 WHERE id = $2")
        .bind(format!("{url}/missing"))
        .bind(id)
        .execute(&ctx.db)
        .await
        .unwrap();
    let subscription = lists::find(&ctx.state, id).await.unwrap().unwrap();
    assert!(!lists::sync(&ctx.state, &subscription).await);
    let after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moderation_subscription_advisories WHERE moderation_subscription_id = $1",
    )
    .bind(id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(after, 1);
}
