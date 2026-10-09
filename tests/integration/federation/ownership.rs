//! A signed activity may act only on what its sender owns. A signature says
//! who sent an activity, not whose objects it may change: each of these used
//! to be accepted from any server.

use serde_json::{json, Value};

use crate::helpers::TestContext;

/// Seed a remote account with a real keypair so it can sign.
async fn seed_remote(ctx: &TestContext, username: &str, domain: &str) -> (i64, String, String) {
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, protocol, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $5, $4 || '/inbox', $4 || '/outbox', 1, now(), now())"#,
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

async fn send(ctx: &TestContext, sender: &str, key: &str, activity: &Value) {
    let resp = ctx
        .api
        .post_signed("/inbox", activity, &format!("{sender}#main-key"), key)
        .await;
    assert!(resp.status().is_success(), "{}", resp.status());
}

async fn display_name_and_key(ctx: &TestContext, id: i64) -> (String, String) {
    sqlx::query_as("SELECT display_name, public_key FROM accounts WHERE id = $1")
        .bind(id)
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

/// Rewriting another account's key was a takeover: the new key would then
/// verify activities signed as that account.
#[tokio::test]
async fn test_an_actor_updates_only_itself() {
    let ctx = TestContext::new("own-update-actor").await;
    let (_, mallory, mallory_key) = seed_remote(&ctx, "mallory", "evil.invalid").await;
    let (victim_id, victim, _) = seed_remote(&ctx, "victim", "victim.invalid").await;
    let before = display_name_and_key(&ctx, victim_id).await;
    let (_, forged_key) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();

    send(
        &ctx,
        &mallory,
        &mallory_key,
        &json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{mallory}#updates/1"),
            "type": "Update",
            "actor": mallory,
            "object": {
                "id": victim,
                "type": "Person",
                "name": "taken over",
                "inbox": format!("{victim}/inbox"),
                "publicKey": {"id": format!("{victim}#main-key"), "owner": victim, "publicKeyPem": forged_key},
            },
        }),
    )
    .await;
    assert_eq!(display_name_and_key(&ctx, victim_id).await, before);
}

/// Editing a status was allowed to whoever named it.
#[tokio::test]
async fn test_a_status_is_edited_only_by_its_author() {
    let ctx = TestContext::new("own-update-note").await;
    let (_, mallory, mallory_key) = seed_remote(&ctx, "mallory", "evil.invalid").await;
    let (victim_id, victim, _) = seed_remote(&ctx, "victim", "victim.invalid").await;
    let status = format!("{victim}/statuses/1");
    sqlx::query(
        r#"INSERT INTO statuses (id, account_id, text, spoiler_text, visibility, uri, url, local, created_at, updated_at)
           VALUES ($1, $2, 'what victim said', '', 0, $3, $3, false, now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(victim_id)
    .bind(&status)
    .execute(&ctx.db)
    .await
    .unwrap();

    send(
        &ctx,
        &mallory,
        &mallory_key,
        &json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{mallory}#updates/2"),
            "type": "Update",
            "actor": mallory,
            "object": {"id": status, "type": "Note", "content": "what mallory says victim said"},
        }),
    )
    .await;
    let text: String = sqlx::query_scalar("SELECT text FROM statuses WHERE uri = $1")
        .bind(&status)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(text, "what victim said");
}

/// A note embedded under another server's URI was stored as the sender's,
/// and the real one could then never be.
#[tokio::test]
async fn test_an_embedded_note_from_elsewhere_is_not_taken_as_given() {
    let ctx = TestContext::new("own-create").await;
    let (_, mallory, mallory_key) = seed_remote(&ctx, "mallory", "evil.invalid").await;
    let (_, victim, _) = seed_remote(&ctx, "victim", "victim.invalid").await;
    let alice: Value = ctx
        .api
        .ap_get("/users/alice", None)
        .await
        .json()
        .await
        .unwrap();
    let alice = alice["id"].as_str().unwrap().to_owned();

    let note = |id: &str, author: &str| {
        json!({
            "id": id,
            "type": "Note",
            "attributedTo": author,
            "content": "hello alice",
            "to": [alice],
        })
    };
    let create = |id: &str, object: Value| {
        json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": id,
            "type": "Create",
            "actor": mallory,
            "to": [alice],
            "object": object,
        })
    };
    let stored = |uri: String| {
        let db = ctx.db.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM statuses WHERE uri = $1")
                .bind(uri)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };

    // Under the victim's URI, attributed to the victim or to mallory.
    let squatted = format!("{victim}/statuses/99");
    for (n, author) in [&victim, &mallory].into_iter().enumerate() {
        send(
            &ctx,
            &mallory,
            &mallory_key,
            &create(
                &format!("{mallory}/activities/{n}"),
                note(&squatted, author),
            ),
        )
        .await;
    }
    assert_eq!(stored(squatted).await, 0);

    // Under mallory's own URI but attributed to the victim.
    let misattributed = format!("{mallory}/statuses/98");
    send(
        &ctx,
        &mallory,
        &mallory_key,
        &create(
            &format!("{mallory}/activities/3"),
            note(&misattributed, &victim),
        ),
    )
    .await;
    assert_eq!(stored(misattributed).await, 0);

    // Mallory's own note, as mallory's, still arrives.
    let own = format!("{mallory}/statuses/1");
    send(
        &ctx,
        &mallory,
        &mallory_key,
        &create(&format!("{mallory}/activities/4"), note(&own, &mallory)),
    )
    .await;
    assert_eq!(stored(own).await, 1);
}
