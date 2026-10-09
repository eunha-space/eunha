//! A local post's `Note`, and the activities that carry it, member by member
//! as `ActivityPub::NoteSerializer` and the serializers around it write them.

use serde_json::{json, Value};

use crate::helpers::TestContext;

/// A remote account; its id and URI.
async fn seed_remote(ctx: &TestContext, username: &str, domain: &str) -> (i64, String) {
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key,
                                 inbox_url, outbox_url, protocol, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, '', $4 || '/inbox', $4 || '/outbox', 1, now(), now())"#,
    )
    .bind(id)
    .bind(username)
    .bind(domain)
    .bind(&uri)
    .execute(&ctx.db)
    .await
    .unwrap();
    (id, uri)
}

/// Give alice a key and a remote follower, so that what she does is queued.
async fn alice_with_a_follower(ctx: &TestContext) {
    let alice_id: i64 = ctx.alice_id.parse().unwrap();
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    sqlx::query("UPDATE accounts SET private_key = $2, public_key = $3 WHERE id = $1")
        .bind(alice_id)
        .bind(&priv_pem)
        .bind(&pub_pem)
        .execute(&ctx.db)
        .await
        .unwrap();
    let (nina_id, _) = seed_remote(ctx, "nina", "nina.invalid").await;
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
}

async fn queued(ctx: &TestContext, kind: &str) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT payload->'activity' FROM eunha.ojak_queue
         WHERE queue IN ('delivery', 'delivery-priority') AND payload->'activity'->>'type' = $1
         ORDER BY id",
    )
    .bind(kind)
    .fetch_all(&ctx.db)
    .await
    .unwrap()
}

async fn note(ctx: &TestContext, status_id: &str) -> Value {
    let response = ctx
        .api
        .ap_get(&format!("/users/alice/statuses/{status_id}"), None)
        .await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

async fn post(ctx: &TestContext, body: &Value) -> Value {
    let response = ctx
        .api
        .post_json("/api/v1/statuses", Some(&ctx.alice_token), body)
        .await;
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

/// `Status#emojis`: the content warning, the text and the poll's options,
/// scanned with `CustomEmoji::SCAN_RE`, which takes no shortcode stuck to a
/// letter before or after it; each emoji once, in the order it is first named.
#[tokio::test]
async fn test_a_notes_emoji_come_from_its_poll_too() {
    let ctx = TestContext::new("note-emoji-poll").await;
    for shortcode in ["party", "blobcat", "glued"] {
        sqlx::query(
            "INSERT INTO custom_emojis (id, shortcode, domain, disabled, image_file_name,
                                        image_content_type, created_at, updated_at)
             VALUES (nextval('custom_emojis_id_seq'), $1, NULL, false, $1 || '.png',
                     'image/png', now(), now())",
        )
        .bind(shortcode)
        .execute(&ctx.db)
        .await
        .unwrap();
    }
    let status = post(
        &ctx,
        &json!({
            "status": "x:glued: :blobcat: x:party:",
            "visibility": "public",
            "poll": { "options": [":party:", "two"], "expires_in": 86400 },
        }),
    )
    .await;
    let note = note(&ctx, status["id"].as_str().unwrap()).await;
    let emoji: Vec<&str> = note["tag"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tag| tag["type"] == "Emoji")
        .map(|tag| tag["name"].as_str().unwrap())
        .collect();
    assert_eq!(emoji, [":blobcat:", ":party:"]);
}

/// `NoteSerializer#in_reply_to`: a local post replied to is named by its
/// account's scheme whatever its `uri` holds, one whose URI is not HTTP by
/// its `url`, and one since deleted not at all.
#[tokio::test]
async fn test_in_reply_to_names_the_thread_as_mastodon_does() {
    let ctx = TestContext::new("note-in-reply-to").await;
    let parent = post(&ctx, &json!({ "status": "first", "visibility": "public" })).await;
    let parent_id = parent["id"].as_str().unwrap();
    let parent_uri = parent["uri"].as_str().unwrap().to_owned();
    let reply = post(
        &ctx,
        &json!({ "status": "second", "visibility": "public", "in_reply_to_id": parent_id }),
    )
    .await;
    let reply_id = reply["id"].as_str().unwrap();
    assert_eq!(note(&ctx, reply_id).await["inReplyTo"], parent_uri.as_str());

    sqlx::query("UPDATE statuses SET uri = NULL WHERE id = $1::bigint")
        .bind(parent_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    assert_eq!(note(&ctx, reply_id).await["inReplyTo"], parent_uri.as_str());

    sqlx::query("UPDATE statuses SET deleted_at = now() WHERE id = $1::bigint")
        .bind(parent_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    assert_eq!(note(&ctx, reply_id).await["inReplyTo"], Value::Null);

    // A remote post known by a `tag:` URI is named by its `url`.
    let (rob_id, rob) = seed_remote(&ctx, "rob", "rob.invalid").await;
    let thread_id = eunha::snowflake::next_id();
    let thread_url = format!("{rob}/posts/1");
    sqlx::query(
        r#"INSERT INTO statuses (id, account_id, text, spoiler_text, visibility, uri, url, local, created_at, updated_at)
           VALUES ($1, $2, 'old', '', 0, 'tag:rob.invalid,2017:objectId=1', $3, false, now(), now())"#,
    )
    .bind(thread_id)
    .bind(rob_id)
    .bind(&thread_url)
    .execute(&ctx.db)
    .await
    .unwrap();
    let reply = post(
        &ctx,
        &json!({ "status": "to rob", "visibility": "public", "in_reply_to_id": thread_id.to_string() }),
    )
    .await;
    assert_eq!(
        note(&ctx, reply["id"].as_str().unwrap()).await["inReplyTo"],
        thread_url.as_str()
    );
}

/// `virtual_tags` lists the mentions by their ids, and Mastodon's
/// `interaction_policies` context names `canFeature`.
#[tokio::test]
async fn test_mentions_are_tagged_in_their_order() {
    let ctx = TestContext::new("note-mention-order").await;
    let status = post(
        &ctx,
        &json!({ "status": "hi @bob", "visibility": "public" }),
    )
    .await;
    let status_id: i64 = status["id"].as_str().unwrap().parse().unwrap();
    let (zed_id, zed) = seed_remote(&ctx, "zed", "zed.invalid").await;
    sqlx::query("UPDATE mentions SET id = 1000 WHERE status_id = $1")
        .bind(status_id)
        .execute(&ctx.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO mentions (id, status_id, account_id, silent, created_at, updated_at)
         VALUES (10, $1, $2, false, now(), now())",
    )
    .bind(status_id)
    .bind(zed_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    let note = note(&ctx, &status_id.to_string()).await;
    let mentioned: Vec<&str> = note["tag"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|tag| tag["type"] == "Mention")
        .map(|tag| tag["href"].as_str().unwrap())
        .collect();
    assert_eq!(mentioned[0], zed);
    assert!(mentioned[1].ends_with("/users/bob"), "{mentioned:?}");
    let terms = note["@context"][1].as_object().unwrap();
    assert_eq!(
        terms["canFeature"],
        json!({ "@id": "https://w3id.org/fep/7aa9#canFeature", "@type": "@id" })
    );
}

/// An edit's `Update` is `published` at its `edited_at` in whole seconds
/// (`UpdateNoteSerializer`), and a pin's `Add` and an unpin's `Remove` have
/// no id (`AddNoteSerializer`, `RemoveNoteSerializer`).
#[tokio::test]
async fn test_updates_and_pins_are_written_as_mastodon_writes_them() {
    let ctx = TestContext::new("note-update-pin").await;
    alice_with_a_follower(&ctx).await;
    let status = post(&ctx, &json!({ "status": "before", "visibility": "public" })).await;
    let status_id = status["id"].as_str().unwrap();

    let edited = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{status_id}"),
            Some(&ctx.alice_token),
            &json!({ "status": "after" }),
        )
        .await;
    assert_eq!(edited.status(), 200);
    let update = queued(&ctx, "Update").await.pop().expect("an Update");
    let published = update["published"].as_str().unwrap();
    assert!(
        chrono::NaiveDateTime::parse_from_str(published, "%Y-%m-%dT%H:%M:%SZ").is_ok(),
        "{published}"
    );

    for (path, kind) in [("pin", "Add"), ("unpin", "Remove")] {
        let response = ctx
            .api
            .post_json(
                &format!("/api/v1/statuses/{status_id}/{path}"),
                Some(&ctx.alice_token),
                &json!({}),
            )
            .await;
        assert_eq!(response.status(), 200);
        let activity = queued(&ctx, kind).await.pop().expect("queued");
        assert!(activity.get("id").is_none(), "{activity}");
        assert_eq!(activity["object"], status["uri"]);
        assert!(activity["target"]
            .as_str()
            .unwrap()
            .ends_with("/collections/featured"));
    }
}

/// `UpdatePollSerializer` serializes `id`, `type`, `actor` and `to`, and no
/// `cc`.
#[tokio::test]
async fn test_a_poll_update_has_no_cc() {
    let ctx = TestContext::new("note-poll-update").await;
    alice_with_a_follower(&ctx).await;
    let status = post(
        &ctx,
        &json!({
            "status": "which?",
            "visibility": "public",
            "poll": { "options": ["one", "two"], "expires_in": 86400 },
        }),
    )
    .await;
    let poll_id = status["poll"]["id"].as_str().unwrap();
    // Rob, elsewhere, voted, so the tallies go to him.
    let (rob_id, _) = seed_remote(&ctx, "rob", "rob.invalid").await;
    sqlx::query(
        "INSERT INTO poll_votes (account_id, poll_id, choice, created_at, updated_at)
         VALUES ($1, $2::bigint, 1, now(), now())",
    )
    .bind(rob_id)
    .bind(poll_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    let voted = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{poll_id}/votes"),
            Some(&ctx.bob_token),
            &json!({ "choices": [0] }),
        )
        .await;
    assert_eq!(voted.status(), 200);
    ctx.state.jobs.settle().await;
    eunha::jobs::make_due(&ctx.state).await.unwrap();
    eunha::jobs::drain(&ctx.state).await.unwrap();

    let update = queued(&ctx, "Update").await.pop().expect("an Update");
    assert_eq!(update["object"]["type"], "Question");
    assert!(update.get("to").is_some());
    assert!(update.get("cc").is_none(), "{update}");
}
