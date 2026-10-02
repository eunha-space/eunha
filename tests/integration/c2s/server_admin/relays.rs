//! `Admin::RelaysController` and a relay's answer.

use super::*;

const MANAGE_FEDERATION: i64 = 1 << 5;

async fn queued(ctx: &TestContext, kind: &str) -> Vec<(Value, String)> {
    sqlx::query_as(
        r#"SELECT payload->'activity', payload->>'inbox' FROM eunha.ojak_queue
           WHERE queue IN ('delivery', 'delivery-priority')
             AND payload->'activity'->>'type' = $1
           ORDER BY id"#,
    )
    .bind(kind)
    .fetch_all(&ctx.db)
    .await
    .unwrap()
}

/// Relays: added and enabled with a `Follow` from the instance actor, answered
/// by the relay's `Accept`, disabled with an `Undo`, and logged.
#[tokio::test]
async fn test_relays() {
    let ctx = TestContext::new("srv-relays").await;
    give_role(&ctx, &ctx.bob_id, 10, MANAGE_FEDERATION).await;
    let post = |path: String, body: Value| {
        let api = &ctx.api;
        let token = ctx.bob_token.clone();
        async move { api.post_json(&path, Some(&token), &body).await }
    };
    assert_eq!(
        error_of(
            post("/api/v1/admin/relays".into(), json!({"inbox_url": " "})).await,
            StatusCode::UNPROCESSABLE_ENTITY
        )
        .await,
        "Validation failed: Inbox url can't be blank, Inbox url is invalid"
    );

    let inbox = "https://relay.invalid/inbox";
    let relay =
        json_ok(post("/api/v1/admin/relays".into(), json!({"inbox_url": inbox})).await).await;
    assert_eq!(relay["state"], "pending");
    assert_eq!(relay["enabled"], false);
    let follow_id = relay["follow_activity_id"].as_str().unwrap().to_owned();
    let follows = queued(&ctx, "Follow").await;
    assert_eq!(follows.len(), 1);
    let (follow, to) = &follows[0];
    assert_eq!(to, inbox);
    assert_eq!(follow["id"], follow_id.as_str());
    assert_eq!(follow["actor"], format!("https://{}/actor", ctx.domain));
    assert_eq!(
        follow["object"],
        "https://www.w3.org/ns/activitystreams#Public"
    );
    assert_eq!(
        error_of(
            post("/api/v1/admin/relays".into(), json!({"inbox_url": inbox})).await,
            StatusCode::UNPROCESSABLE_ENTITY
        )
        .await,
        "Validation failed: Inbox url has already been taken"
    );

    // The relay accepts: `Accept` of the `Follow`, by its id.
    eunha::api::ap::inbox::received(
        &ctx.state,
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": "https://relay.invalid/activities/1",
            "type": "Accept",
            "actor": "https://relay.invalid/actor",
            "object": follow_id,
        }),
    )
    .await
    .unwrap();
    let list = json_ok(
        ctx.api
            .get("/api/v1/admin/relays", Some(&ctx.bob_token))
            .await,
    )
    .await;
    assert_eq!(list[0]["state"], "accepted");
    assert_eq!(list[0]["enabled"], true);

    let id = relay["id"].as_str().unwrap();
    let disabled =
        json_ok(post(format!("/api/v1/admin/relays/{id}/disable"), json!({})).await).await;
    assert_eq!(disabled["state"], "idle");
    let undos = queued(&ctx, "Undo").await;
    assert_eq!(undos[0].0["object"]["id"], follow_id.as_str());
    let enabled = json_ok(post(format!("/api/v1/admin/relays/{id}/enable"), json!({})).await).await;
    assert_eq!(enabled["state"], "pending");
    json_ok(
        ctx.api
            .delete(&format!("/api/v1/admin/relays/{id}"), &ctx.bob_token)
            .await,
    )
    .await;
    assert_eq!(
        logs(&ctx).await,
        [
            ("create".to_owned(), "Relay".to_owned()),
            ("disable".to_owned(), "Relay".to_owned()),
            ("enable".to_owned(), "Relay".to_owned()),
            ("destroy".to_owned(), "Relay".to_owned()),
        ]
    );
}
