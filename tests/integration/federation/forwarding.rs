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
    let post_uri = post["uri"]
        .as_str()
        .expect("the status has a uri")
        .to_owned();
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

/// Every local profile can be sent again to the servers that know it, with
/// the avatar where it is now: other servers keep the URL they last saw, and
/// after an instance's media has moved, that URL is gone.
#[tokio::test]
async fn test_profiles_are_updated_in_batch_with_their_avatar() {
    let ctx = TestContext::new("distribute-profiles").await;
    let (priv_pem, pub_pem) = eunha::crypto::generate_rsa_keypair().unwrap();
    sqlx::query(
        "UPDATE accounts SET private_key = $1, public_key = $2,
             avatar_file_name = 'face.png', avatar_content_type = 'image/png',
             avatar_storage_schema_version = 1
         WHERE username = 'alice' AND domain IS NULL",
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

    let dry = eunha::accounts::update_profiles(&ctx.state, &eunha::accounts::Selection::All, true)
        .await
        .unwrap();
    assert_eq!(dry.sent, vec![("alice".to_owned(), 1)]);
    assert!(
        queued_for(&ctx, &format!("{nina}/inbox")).await.is_empty(),
        "a dry run sends nothing"
    );
    assert!(
        dry.skipped.iter().any(|(name, _)| name == "bob"),
        "bob cannot sign"
    );

    let report =
        eunha::accounts::update_profiles(&ctx.state, &eunha::accounts::Selection::All, false)
            .await
            .unwrap();
    assert_eq!(
        report.sent,
        vec![("alice".to_owned(), 1)],
        "alice alone can sign, to nina"
    );

    let queued = queued_for(&ctx, &format!("{nina}/inbox")).await;
    let update = queued.last().expect("an Update is queued for nina");
    assert_eq!(update["type"], "Update");
    let icon = update["object"]["icon"]["url"].as_str().unwrap_or_default();
    assert!(
        icon.ends_with("/face.png"),
        "the avatar where it is now: {icon}"
    );

    // Sent twice, it is two activities: a server drops an id it has seen.
    let report = eunha::accounts::update_profiles(
        &ctx.state,
        &eunha::accounts::Selection::Usernames(vec!["alice".into(), "nobody".into()]),
        false,
    )
    .await
    .unwrap();
    assert_eq!(report.unknown, vec!["nobody".to_owned()]);
    let queued = queued_for(&ctx, &format!("{nina}/inbox")).await;
    assert_ne!(
        queued[queued.len() - 1]["id"],
        queued[queued.len() - 2]["id"]
    );
}

/// After a domain change, followers are moved from each account's actor
/// under the old domain to the new one: a Move signed as the old actor,
/// whose key the followers' servers already hold, to the new actor, which
/// lists the old one in `alsoKnownAs`. Nothing needs serving on the old
/// domain.
#[tokio::test]
async fn test_followers_are_moved_from_a_previous_domain() {
    let ctx = TestContext::new("move-domain").await;
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

    let old_domain = format!("old-{}", ctx.domain);
    let old = format!("https://{old_domain}/users/alice");
    let new = format!("https://{}/users/alice", ctx.domain);

    // The new actor names the old one, which is what a server checks.
    let actor: Value = ctx
        .api
        .ap_get("/users/alice", None)
        .await
        .json()
        .await
        .unwrap();
    assert!(
        actor["alsoKnownAs"]
            .as_array()
            .is_some_and(|aka| aka.iter().any(|a| a == old.as_str())),
        "{}",
        actor["alsoKnownAs"]
    );

    // A domain the instance never had is refused before anything is sent.
    let selection = eunha::accounts::Selection::All;
    assert!(
        eunha::accounts::move_followers(&ctx.state, &selection, "elsewhere.invalid", false)
            .await
            .is_err()
    );

    let dry = eunha::accounts::move_followers(&ctx.state, &selection, &old_domain, true)
        .await
        .unwrap();
    assert_eq!(dry.sent, vec![("alice".to_owned(), 1)]);
    assert!(queued_for(&ctx, &format!("{nina}/inbox")).await.is_empty());

    eunha::accounts::move_followers(&ctx.state, &selection, &old_domain, false)
        .await
        .unwrap();
    let queued: Vec<(Value, String)> = sqlx::query_as(
        "SELECT payload->'activity', payload->>'sender' FROM eunha.feder_queue
         WHERE queue = 'delivery' AND payload->>'inbox' = $1",
    )
    .bind(format!("{nina}/inbox"))
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    let (activity, sender) = queued.last().expect("a Move is queued for nina");
    assert_eq!(activity["type"], "Move");
    assert_eq!(activity["actor"], old.as_str());
    assert_eq!(activity["object"], old.as_str());
    assert_eq!(activity["target"], new.as_str());
    assert!(
        activity.get("proof").is_none(),
        "no proof naming a dead domain"
    );
    assert_eq!(
        sender,
        &format!("{old}#main-key"),
        "signed as the old actor"
    );
}
