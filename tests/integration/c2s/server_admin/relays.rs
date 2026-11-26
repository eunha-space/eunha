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

/// A remote actor known by `username@relay.invalid`, of `actor_type`.
async fn remote_actor(ctx: &TestContext, username: &str, actor_type: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO accounts (username, domain, uri, inbox_url, actor_type, display_name,
                               created_at, updated_at)
         VALUES ($1, 'relay.invalid', $2, $3, $4, 'The relay', now(), now()) RETURNING id",
    )
    .bind(username)
    .bind(format!("https://relay.invalid/{username}"))
    .bind(format!("https://relay.invalid/{username}/inbox"))
    .bind(actor_type)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

/// Relays: added by their actor's handle and enabled with a `Follow` from the
/// instance actor, answered by the relay's `Accept`, disabled with an `Undo`,
/// and logged.
#[tokio::test]
async fn test_relays() {
    let ctx = TestContext::new("srv-relays").await;
    give_role(&ctx, &ctx.bob_id, 10, MANAGE_FEDERATION).await;
    let post = |path: String, body: Value| {
        let api = &ctx.api;
        let token = ctx.bob_token.clone();
        async move { api.post_json(&path, Some(&token), &body).await }
    };
    let refused = |body: Value| {
        let post = &post;
        async move {
            error_of(
                post("/api/v1/admin/relays".into(), body).await,
                StatusCode::UNPROCESSABLE_ENTITY,
            )
            .await
        }
    };
    assert_eq!(
        refused(json!({"acct": " @"})).await,
        "Validation failed: Acct can't be blank"
    );
    assert_eq!(
        refused(json!({"acct": "nobody@relay.invalid"})).await,
        "Validation failed: Acct Could not resolve relay actor from handle"
    );
    remote_actor(&ctx, "person", "Person").await;
    assert_eq!(
        refused(json!({"acct": "person@relay.invalid"})).await,
        "Validation failed: Acct Unsupported actor type, must be one of: Application, Service"
    );
    let actor = remote_actor(&ctx, "relay", "Application").await;
    assert_eq!(
        refused(json!({"acct": "relay@relay.invalid", "direction": "sideways"})).await,
        "Validation failed: Direction is not included in the list"
    );

    let inbox = "https://relay.invalid/relay/inbox";
    let relay = json_ok(
        post(
            "/api/v1/admin/relays".into(),
            json!({"acct": " @relay@relay.invalid "}),
        )
        .await,
    )
    .await;
    assert_eq!(relay["state"], "pending");
    assert_eq!(relay["enabled"], false);
    assert_eq!(relay["uri"], "https://relay.invalid/relay");
    assert_eq!(relay["inbox_url"], inbox);
    assert_eq!(relay["account_id"], actor.to_string());
    assert_eq!(relay["display_name"], "The relay");
    assert_eq!(relay["direction"], "bidirectional");
    assert_eq!(relay["requires_opt_in"], false);
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
        refused(json!({"acct": "relay@relay.invalid"})).await,
        "Validation failed: Uri has already been taken"
    );

    // The relay accepts: `Accept` of the `Follow`, by its id.
    eunha::api::ap::inbox::received(
        &ctx.state,
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": "https://relay.invalid/activities/1",
            "type": "Accept",
            "actor": "https://relay.invalid/relay",
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
    // A bidirectional relay passes others' posts on.
    assert!(eunha::relays::forwards_to_us(&ctx.state, actor).await);

    let id = relay["id"].as_str().unwrap();
    let disabled =
        json_ok(post(format!("/api/v1/admin/relays/{id}/disable"), json!({})).await).await;
    assert_eq!(disabled["state"], "idle");
    assert!(!eunha::relays::forwards_to_us(&ctx.state, actor).await);
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
    let human: Vec<String> =
        sqlx::query_scalar("SELECT human_identifier FROM admin_action_logs ORDER BY id")
            .fetch_all(&ctx.db)
            .await
            .unwrap();
    assert!(human.iter().all(|h| h == "https://relay.invalid/relay"));
}

/// A broadcast relay that needs members to opt in: it is sent only the posts
/// of members who did, passes nothing on, and opting out of it blocks it.
#[tokio::test]
async fn test_opt_in_broadcast_relay() {
    let ctx = TestContext::new("srv-relays-opt-in").await;
    give_role(&ctx, &ctx.bob_id, 10, MANAGE_FEDERATION).await;
    let actor = remote_actor(&ctx, "relay", "Service").await;
    let relay = json_ok(
        ctx.api
            .post_json(
                "/api/v1/admin/relays",
                Some(&ctx.bob_token),
                &json!({"acct": "relay@relay.invalid", "direction": "broadcast", "requires_opt_in": "1"}),
            )
            .await,
    )
    .await;
    assert_eq!(relay["direction"], "broadcast");
    assert_eq!(relay["requires_opt_in"], true);
    let relay_id = relay["id"].as_str().unwrap().to_owned();
    sqlx::query("UPDATE relays SET state = 2")
        .execute(&ctx.db)
        .await
        .unwrap();
    // A broadcast relay passes nothing on to this server.
    assert!(!eunha::relays::forwards_to_us(&ctx.state, actor).await);

    let alice = id(&ctx.alice_id);
    let inbox = "https://relay.invalid/relay/inbox".to_owned();
    let reach = || eunha::federation::delivery::account_reach_inboxes(&ctx.state, alice);
    assert!(!reach().await.unwrap().contains(&inbox));

    let path = "/api/eunha/v1/relay_preferences";
    let shown = json_ok(ctx.api.get(path, Some(&ctx.alice_token)).await).await;
    assert_eq!(shown.as_array().unwrap().len(), 1);
    assert_eq!(shown[0]["id"], relay_id.as_str());
    assert_eq!(shown[0]["display_name"], "The relay");
    assert_eq!(shown[0]["enabled"], false);

    // Opting in.
    let saved = json_ok(
        ctx.api
            .patch_json(
                path,
                Some(&ctx.alice_token),
                &json!({"relay_ids": ["0", relay_id]}),
            )
            .await,
    )
    .await;
    assert_eq!(saved[0]["enabled"], true);
    assert!(reach().await.unwrap().contains(&inbox));

    let blocked = || async {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2)",
        )
        .bind(alice)
        .bind(actor)
        .fetch_one(&ctx.db)
        .await
        .unwrap()
    };

    // Opting out blocks the relay's actor.
    let saved = json_ok(
        ctx.api
            .patch_json(path, Some(&ctx.alice_token), &json!({"relay_ids": []}))
            .await,
    )
    .await;
    assert_eq!(saved[0]["enabled"], false);
    assert!(blocked().await);
    assert!(!reach().await.unwrap().contains(&inbox));

    // And opting in again unblocks it.
    json_ok(
        ctx.api
            .patch_json(
                path,
                Some(&ctx.alice_token),
                &json!({"relay_ids": [relay_id]}),
            )
            .await,
    )
    .await;
    assert!(!blocked().await);
    assert!(reach().await.unwrap().contains(&inbox));

    // Blocking the actor is opting out.
    json_ok(
        ctx.api
            .post_json(
                &format!("/api/v1/accounts/{actor}/block"),
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await,
    )
    .await;
    let shown = json_ok(ctx.api.get(path, Some(&ctx.alice_token)).await).await;
    assert_eq!(shown[0]["enabled"], false);
    assert!(!reach().await.unwrap().contains(&inbox));
}
