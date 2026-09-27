//! A reply to a local post, addressed to its author's followers, is passed
//! on to them (ActivityPub §7.1.2): they would otherwise see half of the
//! conversation.

use serde_json::{json, Value};

use crate::helpers::TestContext;

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

async fn queued_for(ctx: &TestContext, inbox: &str) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT payload->'activity' FROM eunha.feder_queue WHERE queue = 'delivery' AND payload->>'inbox' = $1",
    )
    .bind(inbox)
    .fetch_all(&ctx.db)
    .await
    .unwrap()
}

#[tokio::test]
async fn test_a_reply_to_a_local_post_reaches_its_authors_followers() {
    let ctx = TestContext::new("forward-reply").await;
    let (priv_pem, pub_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    sqlx::query(
        "UPDATE accounts SET private_key = $1, public_key = $2 WHERE username = 'alice' AND domain IS NULL",
    )
    .bind(&priv_pem)
    .bind(&pub_pem)
    .execute(&ctx.db)
    .await
    .unwrap();
    let alice_id: i64 =
        sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'alice' AND domain IS NULL")
            .fetch_one(&ctx.db)
            .await
            .unwrap();

    // Nina follows alice; bob, on another server, replies to her.
    let (nina_id, nina, _) = seed_remote(&ctx, "nina", "nina.invalid").await;
    sqlx::query(
        "INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(eunha::snowflake::next_id())
    .bind(nina_id)
    .bind(alice_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    let (_, bob, bob_key) = seed_remote(&ctx, "bob", "bob.invalid").await;

    let post = ctx
        .api
        .post_status(&ctx.alice_token, "a post to reply to", "public")
        .await;
    let post_uri = post["uri"].as_str().expect("the status has a uri").to_owned();
    let nina_inbox = format!("{nina}/inbox");
    let before = queued_for(&ctx, &nina_inbox).await.len();

    let alice = format!("https://{}/users/alice", ctx.domain);
    let reply = |n: u32, in_reply_to: &str| {
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{bob}/statuses/{n}/activity"),
            "type": "Create",
            "actor": bob,
            "to": ["https://www.w3.org/ns/activitystreams#Public"],
            "cc": [format!("{alice}/followers")],
            "object": {
                "id": format!("{bob}/statuses/{n}"),
                "type": "Note",
                "attributedTo": bob,
                "inReplyTo": in_reply_to,
                "to": ["https://www.w3.org/ns/activitystreams#Public"],
                "cc": [format!("{alice}/followers")],
                "content": "a reply",
            },
        })
    };
    let activity = reply(1, &post_uri);
    let resp = ctx
        .api
        .post_signed("/inbox", &activity, &format!("{bob}#main-key"), &bob_key)
        .await;
    assert!(resp.status().is_success(), "{}", resp.status());

    let queued = queued_for(&ctx, &nina_inbox).await;
    assert_eq!(queued.len(), before + 1, "forwarded to alice's follower");
    let forwarded = queued.last().unwrap();
    assert_eq!(forwarded["id"], activity["id"]);
    assert!(
        forwarded.get("proof").is_none(),
        "no proof of ours on bob's activity"
    );

    // A reply to something that is not ours is not forwarded.
    let resp = ctx
        .api
        .post_signed(
            "/inbox",
            &reply(2, "https://elsewhere.invalid/notes/1"),
            &format!("{bob}#main-key"),
            &bob_key,
        )
        .await;
    assert!(resp.status().is_success());
    assert_eq!(queued_for(&ctx, &nina_inbox).await.len(), before + 1);
}
