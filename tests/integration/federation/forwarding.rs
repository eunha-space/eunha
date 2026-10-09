//! What eunha passes on of other servers' activities, and sends again of its
//! own: a reply to a local post goes on to its author's followers
//! (`Create#forward_for_reply`), who would otherwise see half of the
//! conversation.

use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn seed_remote(ctx: &TestContext, username: &str, domain: &str) -> (i64, String, String) {
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key, inbox_url, outbox_url, followers_url, protocol, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $5, $4 || '/inbox', $4 || '/outbox', $4 || '/followers', 1, now(), now())"#,
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
        "SELECT payload->'activity' FROM eunha.ojak_queue WHERE queue IN ('delivery', 'delivery-priority') AND payload->>'inbox' = $1 ORDER BY id",
    )
    .bind(inbox)
    .fetch_all(&ctx.db)
    .await
    .unwrap()
}

async fn follow(ctx: &TestContext, follower: i64, followed: i64) {
    sqlx::query(
        "INSERT INTO follows (id, account_id, target_account_id, created_at, updated_at)
         VALUES ($1, $2, $3, now(), now())",
    )
    .bind(eunha::snowflake::next_id())
    .bind(follower)
    .bind(followed)
    .execute(&ctx.db)
    .await
    .unwrap();
}

/// `activity` with `author`'s Linked Data signature, as Mastodon signs a
/// public post.
fn ld_sign(activity: &Value, author: &str, key: &str) -> Value {
    let now = chrono::Utc::now().timestamp();
    ojak::sig::linked_data::sign(
        &ojak_jsonld::Registry::bundled(),
        activity,
        &format!("{author}#main-key"),
        &ojak::sig::PrivateKey::from_pem(key).unwrap(),
        now,
        now + ojak::sig::linked_data::DEFAULT_LIFETIME_SECONDS,
    )
    .unwrap()
}

/// A reply of `author`'s, numbered `n`, to `in_reply_to`, addressed `to` and
/// `cc`.
fn reply(author: &str, n: u32, in_reply_to: &str, to: &[String], cc: &[String]) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{author}/statuses/{n}/activity"),
        "type": "Create",
        "actor": author,
        "to": to,
        "cc": cc,
        "object": {
            "id": format!("{author}/statuses/{n}"),
            "type": "Note",
            "attributedTo": author,
            "inReplyTo": in_reply_to,
            "to": to,
            "cc": cc,
            "content": "a reply",
            "published": "2026-10-01T12:00:00Z",
        },
    })
}

/// A public or unlisted reply to a local post that carries its author's
/// Linked Data signature goes on to the local author's followers, however
/// it is addressed, signed by the local author and as it arrived, so that
/// the signature still verifies; the inbox of the server that sent it is
/// left out. Unsigned, followers-only, from a suspended account, or to a
/// post that is not ours, it does not.
#[tokio::test]
async fn test_a_signed_reply_to_a_local_post_reaches_its_authors_followers() {
    let ctx = TestContext::new("forward-reply").await;
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
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

    // Nina and bob follow alice; bob, and carol, on other servers, reply to
    // her.
    let (nina_id, nina, _) = seed_remote(&ctx, "nina", "nina.invalid").await;
    follow(&ctx, nina_id, alice_id).await;
    let (bob_id, bob, bob_key) = seed_remote(&ctx, "bob", "bob.invalid").await;
    follow(&ctx, bob_id, alice_id).await;
    let (carol_id, carol, carol_key) = seed_remote(&ctx, "carol", "carol.invalid").await;
    let (_, relay, relay_key) = seed_remote(&ctx, "relay", "relay.invalid").await;

    let post = ctx
        .api
        .post_status(&ctx.alice_token, "a post to reply to", "public")
        .await;
    let post_uri = post["uri"]
        .as_str()
        .expect("the status has a uri")
        .to_owned();
    let nina_inbox = format!("{nina}/inbox");
    let bob_inbox = format!("{bob}/inbox");
    let before = queued_for(&ctx, &nina_inbox).await.len();
    let bob_before = queued_for(&ctx, &bob_inbox).await.len();

    let alice = format!("https://{}/users/alice", ctx.domain);
    let public = vec!["https://www.w3.org/ns/activitystreams#Public".to_owned()];
    let bob_followers = vec![format!("{bob}/followers")];
    let deliver = |activity: Value, sender: &str, key: &str| {
        let api = &ctx.api;
        let key_id = format!("{sender}#main-key");
        let key = key.to_owned();
        async move {
            let resp = api.post_signed("/inbox", &activity, &key_id, &key).await;
            assert!(resp.status().is_success(), "{}", resp.status());
        }
    };
    let stored = |author: &str, n: u32| {
        let uri = format!("{author}/statuses/{n}");
        let db = ctx.db.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT id FROM statuses WHERE uri = $1")
                .bind(uri)
                .fetch_optional(&db)
                .await
                .unwrap()
                .is_some()
        }
    };
    let bob_public_key: String =
        sqlx::query_scalar("SELECT public_key FROM accounts WHERE id = $1")
            .bind(bob_id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();

    // Public, signed, and addressed to nobody's followers: forwarded.
    let signed = ld_sign(&reply(&bob, 1, &post_uri, &public, &[]), &bob, &bob_key);
    deliver(signed.clone(), &bob, &bob_key).await;
    assert!(stored(&bob, 1).await);
    let queued = queued_for(&ctx, &nina_inbox).await;
    assert_eq!(queued.len(), before + 1, "forwarded to alice's follower");
    let forwarded = queued.last().unwrap();
    assert_eq!(forwarded, &signed, "as it arrived");
    assert!(
        forwarded.get("proof").is_none(),
        "no proof of ours on bob's activity"
    );
    ojak::sig::linked_data::verify(
        &ojak_jsonld::Registry::bundled(),
        forwarded,
        &bob_public_key,
        chrono::Utc::now().timestamp(),
    )
    .expect("bob's signature still verifies");
    let signer: String = sqlx::query_scalar(
        "SELECT payload->>'sender' FROM eunha.ojak_queue
         WHERE payload->'activity'->>'id' = $1 AND payload->>'inbox' = $2",
    )
    .bind(signed["id"].as_str())
    .bind(&nina_inbox)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(signer.contains("alice"), "signed by alice: {signer}");
    assert_eq!(
        queued_for(&ctx, &bob_inbox).await.len(),
        bob_before,
        "not back to bob's own server"
    );

    // Delivered again: the status is known, and is not forwarded again.
    deliver(signed, &bob, &bob_key).await;
    assert_eq!(queued_for(&ctx, &nina_inbox).await.len(), before + 1);

    // Unlisted, and passed on by another server on bob's signature:
    // forwarded as bob signed it, not as eunha read it.
    let relayed = ld_sign(
        &reply(&bob, 2, &post_uri, &bob_followers, &public),
        &bob,
        &bob_key,
    );
    deliver(relayed.clone(), &relay, &relay_key).await;
    assert!(stored(&bob, 2).await, "taken on bob's signature");
    let queued = queued_for(&ctx, &nina_inbox).await;
    assert_eq!(queued.len(), before + 2, "a relayed reply is forwarded");
    assert_eq!(queued.last().unwrap(), &relayed, "as bob signed it");

    // Unsigned, though addressed to alice's followers: they could not tell
    // who wrote it.
    let to_alices_followers = vec![format!("{alice}/followers")];
    deliver(
        reply(&bob, 3, &post_uri, &public, &to_alices_followers),
        &bob,
        &bob_key,
    )
    .await;
    assert!(stored(&bob, 3).await);
    // Followers-only, though it names alice.
    let to_alice = vec![alice.clone()];
    deliver(
        ld_sign(
            &reply(&bob, 4, &post_uri, &bob_followers, &to_alice),
            &bob,
            &bob_key,
        ),
        &bob,
        &bob_key,
    )
    .await;
    assert!(stored(&bob, 4).await);
    // To something that is not ours.
    deliver(
        ld_sign(
            &reply(
                &bob,
                5,
                "https://elsewhere.invalid/notes/1",
                &public,
                &to_alice,
            ),
            &bob,
            &bob_key,
        ),
        &bob,
        &bob_key,
    )
    .await;
    assert!(stored(&bob, 5).await);
    // From a suspended account: not heard at all.
    sqlx::query("UPDATE accounts SET suspended_at = now() WHERE id = $1")
        .bind(carol_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    deliver(
        ld_sign(
            &reply(&carol, 1, &post_uri, &public, &[]),
            &carol,
            &carol_key,
        ),
        &carol,
        &carol_key,
    )
    .await;
    assert!(!stored(&carol, 1).await, "a suspended account is not heard");
    assert_eq!(queued_for(&ctx, &nina_inbox).await.len(), before + 2);
}

/// Every local profile can be sent again to the servers that know it, with
/// the avatar where it is now: other servers keep the URL they last saw, and
/// after an instance's media has moved, that URL is gone.
#[tokio::test]
async fn test_profiles_are_updated_in_batch_with_their_avatar() {
    let ctx = TestContext::new("distribute-profiles").await;
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
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

    let dry = eunha::accounts::update_profiles(
        &ctx.state,
        &eunha::accounts::Selection::All,
        &Default::default(),
        true,
    )
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

    let report = eunha::accounts::update_profiles(
        &ctx.state,
        &eunha::accounts::Selection::All,
        &Default::default(),
        false,
    )
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
        &Default::default(),
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
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
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

    // Alice follows rob, on another server, which delivers his posts to her
    // old actor until she follows him again from the new one.
    let (rob_id, rob, _) = seed_remote(&ctx, "rob", "rob.invalid").await;
    sqlx::query(
        "INSERT INTO follows (id, account_id, target_account_id, uri, created_at, updated_at)
         VALUES ($1, $2, $3, $4, now(), now())",
    )
    .bind(eunha::snowflake::next_id())
    .bind(alice_id)
    .bind(rob_id)
    .bind(format!("https://old-{}/users/alice#follows/1", ctx.domain))
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
    assert!(eunha::accounts::move_followers(
        &ctx.state,
        &selection,
        "elsewhere.invalid",
        &Default::default(),
        false
    )
    .await
    .is_err());

    let dry = eunha::accounts::move_followers(
        &ctx.state,
        &selection,
        &old_domain,
        &Default::default(),
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        dry.sent,
        vec![("alice".to_owned(), 2)],
        "nina's Move, rob's Follow"
    );
    assert!(queued_for(&ctx, &format!("{nina}/inbox")).await.is_empty());

    let batch = ojak::deliverer::Batch {
        tag: Some("move:test".into()),
        deadline: Some(std::time::SystemTime::now() + std::time::Duration::from_secs(3600)),
        ..ojak::deliverer::Batch::default()
    };
    eunha::accounts::move_followers(&ctx.state, &selection, &old_domain, &batch, false)
        .await
        .unwrap();

    let queued: Vec<(Value, String)> = sqlx::query_as(
        "SELECT payload->'activity', payload->>'sender' FROM eunha.ojak_queue
         WHERE queue IN ('delivery', 'delivery-priority') AND payload->>'inbox' = $1",
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

    let follows = queued_for(&ctx, &format!("{rob}/inbox")).await;
    let follow = follows.last().expect("a Follow is queued for rob");
    assert_eq!(follow["type"], "Follow");
    assert_eq!(follow["actor"], new.as_str());
    assert_eq!(follow["object"], rob.as_str());
    let uri: String = sqlx::query_scalar(
        "SELECT uri FROM follows WHERE account_id = $1 AND target_account_id = $2",
    )
    .bind(alice_id)
    .bind(rob_id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(follow["id"], uri.as_str(), "an Undo names the new Follow");
    // Named as `generate_uri_for` names a follow made here.
    assert!(
        uri.strip_prefix(&format!("https://{}/", ctx.domain))
            .is_some_and(|rest| uuid::Uuid::parse_str(rest).is_ok()),
        "{uri}"
    );
    let status = eunha::accounts::batch_status(&ctx.db, "move:test")
        .await
        .unwrap();
    assert_eq!(status.pending, 2, "the Move and the Follow, in one batch");
}

/// The addresses a domain change leaves stale are rewritten, and a second
/// run changes nothing.
#[tokio::test]
async fn test_a_rename_moves_local_addresses_to_the_new_domain() {
    let ctx = TestContext::new("rename-domain").await;
    let post = ctx
        .api
        .post_status(&ctx.alice_token, "before the move", "public")
        .await;
    let before = post["uri"].as_str().unwrap().to_owned();
    assert!(before.starts_with(&format!("https://{}/", ctx.domain)));
    let alice_id: i64 =
        sqlx::query_scalar("SELECT id FROM accounts WHERE username = 'alice' AND domain IS NULL")
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    let (rob_id, _, _) = seed_remote(&ctx, "rob", "rob.invalid").await;
    sqlx::query(
        "INSERT INTO follows (id, account_id, target_account_id, uri, created_at, updated_at)
         VALUES ($1, $2, $3, $4, now(), now())",
    )
    .bind(eunha::snowflake::next_id())
    .bind(alice_id)
    .bind(rob_id)
    .bind(format!("https://{}/users/alice#follows/1", ctx.domain))
    .execute(&ctx.db)
    .await
    .unwrap();

    let new = format!("new-{}", ctx.domain);
    for _ in 0..2 {
        eunha::import::rename(&ctx.db, &ctx.domain, &new)
            .await
            .unwrap();
    }
    let uri: String = sqlx::query_scalar("SELECT uri FROM statuses WHERE uri LIKE $1")
        .bind(format!("https://{new}/%"))
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(uri, before.replace(&ctx.domain, &new));
    let follow: String = sqlx::query_scalar("SELECT uri FROM follows WHERE account_id = $1")
        .bind(alice_id)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(follow, format!("https://{new}/users/alice#follows/1"));
}
