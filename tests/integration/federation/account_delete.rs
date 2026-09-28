//! Account deletion over the wire: the `Delete(actor)` we send when a local
//! account is deleted, and the one we act on when a remote account is.

use reqwest::StatusCode;
use serde_json::json;

use crate::helpers::TestContext;

/// Seed a remote ActivityPub account with a real keypair so it can sign.
async fn seed_remote(ctx: &TestContext, username: &str, domain: &str) -> (i64, String, String) {
    let (priv_pem, pub_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $5, $4 || '/inbox', $4 || '/outbox', now(), now())"#,
    )
    .bind(id)
    .bind(username)
    .bind(domain)
    .bind(&uri)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();
    (id, uri, priv_pem)
}

/// Deleting a local account enqueues a `Delete(actor)` for the inboxes we know
/// of, addressed to the public collection (`ActivityPub::DeleteActorSerializer`).
#[tokio::test]
async fn test_self_deletion_federates_delete_actor() {
    let ctx = TestContext::new("del-federate").await;
    seed_remote(&ctx, "carol", "remote.invalid").await;
    // Only an account with a signing key federates.
    sqlx::query("UPDATE accounts SET private_key = 'test-private-key' WHERE id = $1")
        .bind(ctx.alice_id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();

    let resp = ctx
        .api
        .http
        .delete(ctx.api.url("/api/v1/accounts"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .json(&json!({"password": "testpassword123"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let job: (serde_json::Value, String) = sqlx::query_as(
        r#"SELECT payload->'activity', payload->>'inbox' FROM eunha.ojak_queue
           WHERE queue IN ('delivery', 'delivery-priority') AND payload->'activity'->>'type' = 'Delete'"#,
    )
    .fetch_one(&ctx.db)
    .await
    .expect("a Delete(actor) delivery should be enqueued");

    let actor = format!("https://{}/users/alice", ctx.domain);
    assert_eq!(job.0["actor"].as_str(), Some(actor.as_str()));
    assert_eq!(job.0["object"].as_str(), Some(actor.as_str()));
    assert_eq!(
        job.0["id"].as_str(),
        Some(format!("{actor}#delete").as_str())
    );
    assert_eq!(
        job.0["to"][0].as_str(),
        Some("https://www.w3.org/ns/activitystreams#Public"),
    );
    assert_eq!(job.1, "https://remote.invalid/users/carol/inbox");
}

/// An inbound `Delete(actor)` purges the remote account outright
/// (`ActivityPub::Activity::Delete#delete_person`).
#[tokio::test]
async fn test_inbound_delete_actor_purges_remote_account() {
    let ctx = TestContext::new("del-inbound").await;
    let (remote_id, remote_uri, priv_pem) = seed_remote(&ctx, "dave", "remote.invalid").await;

    // A status by the remote account, so we can see its content go too.
    sqlx::query(
        r#"INSERT INTO statuses (id, account_id, text, spoiler_text, visibility, uri, url, local, created_at, updated_at)
           VALUES ($1, $2, 'remote post', '', 0, $3, $3, false, now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(remote_id)
    .bind(format!("{remote_uri}/statuses/1"))
    .execute(&ctx.db)
    .await
    .unwrap();

    let delete = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{remote_uri}#delete"),
        "type": "Delete",
        "actor": remote_uri,
        "object": remote_uri,
    });
    let resp = ctx
        .api
        .post_signed(
            "/inbox",
            &delete,
            &format!("{remote_uri}#main-key"),
            &priv_pem,
        )
        .await;
    assert!(
        resp.status().is_success(),
        "Delete(actor) should be accepted, got {}",
        resp.status(),
    );

    let account_exists: Option<i64> = sqlx::query_scalar("SELECT id FROM accounts WHERE id = $1")
        .bind(remote_id)
        .fetch_optional(&ctx.db)
        .await
        .unwrap();
    assert!(
        account_exists.is_none(),
        "the remote account record should be purged, not just suspended"
    );
    let statuses: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses WHERE account_id = $1")
        .bind(remote_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(statuses, 0, "its statuses should be gone");
}
