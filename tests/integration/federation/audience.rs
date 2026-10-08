//! What an inbound status's audience and thread make of it, as
//! `ActivityPub::Activity::Create` has it: silent mentions for whoever is
//! addressed but not tagged, a direct message so addressed becoming limited,
//! the conversation every status is in, and what the sender may take back.

use serde_json::{json, Value};

use crate::helpers::{seed_user, TestContext};

const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

async fn seed_remote(ctx: &TestContext, username: &str, domain: &str) -> (i64, String, String) {
    let (priv_pem, pub_pem) =
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng).unwrap();
    let uri = format!("https://{domain}/users/{username}");
    let id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO accounts (id, username, domain, display_name, note, url, uri, public_key,
                                 inbox_url, outbox_url, followers_url, created_at, updated_at)
           VALUES ($1, $2, $3, $2, '', $4, $4, $5, $4 || '/inbox', $4 || '/outbox',
                   $4 || '/followers', now(), now())"#,
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
    send_to(ctx, "/inbox", sender, key, activity).await;
}

async fn send_to(ctx: &TestContext, inbox: &str, sender: &str, key: &str, activity: &Value) {
    let resp = ctx
        .api
        .post_signed(inbox, activity, &format!("{sender}#main-key"), key)
        .await;
    assert!(resp.status().is_success(), "{}", resp.status());
}

fn create(actor: &str, note: Value) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{}/activity", note["id"].as_str().unwrap()),
        "type": "Create",
        "actor": actor,
        "object": note,
    })
}

fn local(ctx: &TestContext, username: &str) -> String {
    format!("https://{}/users/{username}", ctx.domain)
}

async fn status_row(ctx: &TestContext, uri: &str) -> (i64, i32, Option<i64>) {
    sqlx::query_as("SELECT id, visibility, conversation_id FROM statuses WHERE uri = $1")
        .bind(uri)
        .fetch_one(&ctx.db)
        .await
        .unwrap()
}

async fn mentions(ctx: &TestContext, status_id: i64) -> Vec<(i64, bool)> {
    sqlx::query_as(
        "SELECT account_id, silent FROM mentions WHERE status_id = $1 ORDER BY account_id",
    )
    .bind(status_id)
    .fetch_all(&ctx.db)
    .await
    .unwrap()
}

async fn mention_notifications(ctx: &TestContext, account_id: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM notifications WHERE account_id = $1 AND type = 'mention'",
    )
    .bind(account_id.parse::<i64>().unwrap())
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

/// A direct message tagging alice and addressed to bob too: bob is
/// mentioned silently, which makes it limited; he can read it but is not
/// told, and someone else can do neither.
#[tokio::test]
async fn test_an_untagged_addressee_is_mentioned_silently_and_limits_the_status() {
    let ctx = TestContext::new("audience-silent").await;
    let (_, remy, key) = seed_remote(&ctx, "remy", "remote.invalid").await;
    let (_, carol_token) = seed_user(&ctx.db, &ctx.domain, "carol", "carol@test.invalid").await;
    let note = format!("{remy}/statuses/1");
    send(
        &ctx,
        &remy,
        &key,
        &create(
            &remy,
            json!({
                "id": note, "type": "Note", "attributedTo": remy,
                "content": "<p>@alice psst</p>",
                "to": [local(&ctx, "alice"), local(&ctx, "bob")],
                "tag": [{"type": "Mention", "href": local(&ctx, "alice")}],
            }),
        ),
    )
    .await;

    let (id, visibility, _) = status_row(&ctx, &note).await;
    assert_eq!(
        visibility, 4,
        "a direct message with a silent mention is limited"
    );
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    let mut expected = vec![(alice, false), (bob, true)];
    expected.sort();
    assert_eq!(mentions(&ctx, id).await, expected);
    assert_eq!(mention_notifications(&ctx, &ctx.alice_id).await, 1);
    assert_eq!(mention_notifications(&ctx, &ctx.bob_id).await, 0);

    let path = format!("/api/v1/statuses/{id}");
    let seen = ctx.api.get(&path, Some(&ctx.bob_token)).await;
    assert_eq!(seen.status(), 200);
    let body: Value = seen.json().await.unwrap();
    assert_eq!(body["visibility"], "private", "limited is shown as private");
    assert_eq!(
        ctx.api.get(&path, Some(&carol_token)).await.status(),
        404,
        "a limited status is for those it mentions"
    );
    assert_eq!(ctx.api.get(&path, None).await.status(), 404);
}

/// Delivered to bob's own inbox, a status that does not address him is
/// his to read, by a silent mention.
#[tokio::test]
async fn test_the_owner_of_the_inbox_it_reached_is_mentioned_silently() {
    let ctx = TestContext::new("audience-delivered").await;
    let (_, remy, key) = seed_remote(&ctx, "remy", "remote.invalid").await;
    let note = format!("{remy}/statuses/2");
    send_to(
        &ctx,
        "/users/bob/inbox",
        &remy,
        &key,
        &create(
            &remy,
            json!({
                "id": note, "type": "Note", "attributedTo": remy,
                "content": "<p>for someone</p>",
                "to": ["https://elsewhere.invalid/users/x"],
            }),
        ),
    )
    .await;
    let (id, visibility, _) = status_row(&ctx, &note).await;
    assert_eq!(visibility, 4);
    assert_eq!(
        mentions(&ctx, id).await,
        vec![(ctx.bob_id.parse::<i64>().unwrap(), true)]
    );
}

/// Every remote status is in a conversation: the one it names, recorded by
/// its URI, which a reply joins, and whose root is the first post.
#[tokio::test]
async fn test_a_remote_thread_has_a_conversation_that_can_be_muted() {
    let ctx = TestContext::new("audience-conversation").await;
    let (_, remy, key) = seed_remote(&ctx, "remy", "remote.invalid").await;
    let context = "https://remote.invalid/contexts/1";
    let root = format!("{remy}/statuses/10");
    let reply = format!("{remy}/statuses/11");
    for (uri, in_reply_to) in [(&root, None), (&reply, Some(&root))] {
        send(
            &ctx,
            &remy,
            &key,
            &create(
                &remy,
                json!({
                    "id": uri, "type": "Note", "attributedTo": remy,
                    "content": "<p>@alice hello</p>",
                    "inReplyTo": in_reply_to,
                    "conversation": context,
                    "to": [PUBLIC], "cc": [local(&ctx, "alice")],
                    "tag": [{"type": "Mention", "href": local(&ctx, "alice")}],
                }),
            ),
        )
        .await;
    }
    let (root_id, visibility, root_conversation) = status_row(&ctx, &root).await;
    assert_eq!(visibility, 0);
    let (reply_id, _, reply_conversation) = status_row(&ctx, &reply).await;
    let conversation = root_conversation.expect("a remote status has a conversation");
    assert_eq!(reply_conversation, Some(conversation));
    let (uri, parent_status_id): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT uri, parent_status_id FROM conversations WHERE id = $1")
            .bind(conversation)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
    assert_eq!(uri.as_deref(), Some(context));
    assert_eq!(parent_status_id, Some(root_id));

    let muted = ctx
        .api
        .post_json(
            &format!("/api/v1/statuses/{reply_id}/mute"),
            Some(&ctx.alice_token),
            &json!({}),
        )
        .await;
    assert_eq!(muted.status(), 200);
    let body: Value = muted.json().await.unwrap();
    assert_eq!(body["muted"], true);
}

/// A thread without a `conversation` gets one of ours, the reply joining
/// its parent's.
#[tokio::test]
async fn test_a_reply_without_a_conversation_joins_its_parents() {
    let ctx = TestContext::new("audience-thread").await;
    let (_, remy, key) = seed_remote(&ctx, "remy", "remote.invalid").await;
    let root = format!("{remy}/statuses/20");
    let reply = format!("{remy}/statuses/21");
    for (uri, in_reply_to) in [(&root, None), (&reply, Some(&root))] {
        send(
            &ctx,
            &remy,
            &key,
            &create(
                &remy,
                json!({
                    "id": uri, "type": "Note", "attributedTo": remy,
                    "content": "<p>hi</p>",
                    "inReplyTo": in_reply_to,
                    "to": [PUBLIC], "cc": [local(&ctx, "alice")],
                }),
            ),
        )
        .await;
    }
    let (_, _, root_conversation) = status_row(&ctx, &root).await;
    let (_, _, reply_conversation) = status_row(&ctx, &reply).await;
    assert!(root_conversation.is_some());
    assert_eq!(root_conversation, reply_conversation);
}

/// One server cannot undo another's follow by naming its id.
#[tokio::test]
async fn test_an_undo_takes_back_only_the_senders_follow() {
    let ctx = TestContext::new("audience-undo-follow").await;
    let (victim_id, victim, _) = seed_remote(&ctx, "victim", "victim.invalid").await;
    let (_, mallory, mallory_key) = seed_remote(&ctx, "mallory", "evil.invalid").await;
    let follow_uri = format!("{victim}#follows/1");
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, uri, created_at, updated_at) VALUES ($1, $2, $3, now(), now())",
    )
    .bind(victim_id)
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .bind(&follow_uri)
    .execute(&ctx.db)
    .await
    .unwrap();

    for object in [
        json!({"id": follow_uri, "type": "Follow", "actor": victim, "object": local(&ctx, "alice")}),
        json!(follow_uri),
    ] {
        send(
            &ctx,
            &mallory,
            &mallory_key,
            &json!({
                "@context": "https://www.w3.org/ns/activitystreams",
                "id": format!("{mallory}#undo/{}", eunha::snowflake::next_id()),
                "type": "Undo",
                "actor": mallory,
                "object": object,
            }),
        )
        .await;
    }
    let follows: i64 = sqlx::query_scalar("SELECT count(*) FROM follows WHERE uri = $1")
        .bind(&follow_uri)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(follows, 1);
}

/// A `Delete` naming a status of someone else's on the sender's own host
/// leaves it.
#[tokio::test]
async fn test_a_delete_removes_only_the_senders_status() {
    let ctx = TestContext::new("audience-delete-owner").await;
    let (victim_id, _, _) = seed_remote(&ctx, "victim", "shared.invalid").await;
    let (_, mallory, mallory_key) = seed_remote(&ctx, "mallory", "shared.invalid").await;
    let status = "https://shared.invalid/users/victim/statuses/1";
    sqlx::query(
        r#"INSERT INTO statuses (id, account_id, text, spoiler_text, visibility, uri, url, local, created_at, updated_at)
           VALUES ($1, $2, 'mine', '', 0, $3, $3, false, now(), now())"#,
    )
    .bind(eunha::snowflake::next_id())
    .bind(victim_id)
    .bind(status)
    .execute(&ctx.db)
    .await
    .unwrap();
    send(
        &ctx,
        &mallory,
        &mallory_key,
        &json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{mallory}#delete/1"),
            "type": "Delete",
            "actor": mallory,
            "object": {"id": status, "type": "Tombstone"},
        }),
    )
    .await;
    let kept: bool = sqlx::query_scalar("SELECT deleted_at IS NULL FROM statuses WHERE uri = $1")
        .bind(status)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert!(kept);
}

/// A mentioned account whose server does not answer is tried again later
/// (`MentionResolveWorker`) rather than dropped.
#[tokio::test]
async fn test_an_unreachable_mention_is_retried() {
    let ctx = TestContext::new("audience-mention-retry").await;
    let (_, remy, key) = seed_remote(&ctx, "remy", "remote.invalid").await;
    let note = format!("{remy}/statuses/30");
    let unreachable = "https://unreachable.invalid/users/nobody";
    send(
        &ctx,
        &remy,
        &key,
        &create(
            &remy,
            json!({
                "id": note, "type": "Note", "attributedTo": remy,
                "content": "<p>hi</p>",
                "to": [PUBLIC], "cc": [local(&ctx, "alice")],
                "tag": [{"type": "Mention", "href": unreachable}],
            }),
        ),
    )
    .await;
    let (id, _, _) = status_row(&ctx, &note).await;
    let queued: i64 = sqlx::query_scalar(
        r#"SELECT count(*) FROM eunha.jobs
           WHERE kind = 'MentionResolveWorker' AND args->>'uri' = $1
             AND (args->>'status_id')::bigint = $2"#,
    )
    .bind(unreachable)
    .bind(id)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(queued, 1);
}

/// An edit that no longer tags someone leaves them mentioned, silently, and
/// tags whoever it now names.
#[tokio::test]
async fn test_an_edit_silences_the_mentions_it_drops() {
    let ctx = TestContext::new("audience-edit-mentions").await;
    let (_, remy, key) = seed_remote(&ctx, "remy", "remote.invalid").await;
    let note = format!("{remy}/statuses/40");
    let published = chrono::Utc::now();
    let tagging = |who: &str, updated: Option<String>| {
        json!({
            "id": note, "type": "Note", "attributedTo": remy,
            "content": format!("<p>@{who}</p>"),
            "published": published.to_rfc3339(),
            "updated": updated,
            "to": [PUBLIC], "cc": [local(&ctx, "alice"), local(&ctx, "bob")],
            "tag": [{"type": "Mention", "href": local(&ctx, who)}],
        })
    };
    send(&ctx, &remy, &key, &create(&remy, tagging("alice", None))).await;
    let edited = (published + chrono::Duration::minutes(5)).to_rfc3339();
    send(
        &ctx,
        &remy,
        &key,
        &json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{note}#updates/1"),
            "type": "Update",
            "actor": remy,
            "object": tagging("bob", Some(edited)),
        }),
    )
    .await;
    let (id, _, _) = status_row(&ctx, &note).await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let bob: i64 = ctx.bob_id.parse().unwrap();
    let mut expected = vec![(alice, true), (bob, false)];
    expected.sort();
    assert_eq!(mentions(&ctx, id).await, expected);
}

async fn conversation_rows(ctx: &TestContext, account_id: i64) -> Vec<(Vec<i64>, bool)> {
    sqlx::query_as(
        "SELECT participant_account_ids, unread FROM account_conversations WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_all(&ctx.db)
    .await
    .unwrap()
}

/// A direct message's mention notification is about the `Mention`; the
/// message goes into the conversations of the mentioned and of its remote
/// author alike.
#[tokio::test]
async fn test_a_direct_message_notifies_about_its_mention_and_reaches_conversations() {
    let ctx = TestContext::new("audience-dm-rows").await;
    let (remy_id, remy, key) = seed_remote(&ctx, "remy", "remote.invalid").await;
    ctx.api
        .patch_json(
            "/api/v2/notifications/policy",
            Some(&ctx.alice_token),
            &json!({"for_private_mentions": "accept"}),
        )
        .await;
    let note = format!("{remy}/statuses/50");
    send(
        &ctx,
        &remy,
        &key,
        &create(
            &remy,
            json!({
                "id": note, "type": "Note", "attributedTo": remy,
                "content": "<p>@alice psst</p>",
                "to": [local(&ctx, "alice")],
                "tag": [{"type": "Mention", "href": local(&ctx, "alice")}],
            }),
        ),
    )
    .await;
    let (status_id, visibility, _) = status_row(&ctx, &note).await;
    assert_eq!(visibility, 3);
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let (activity_type, mention_status): (String, i64) = sqlx::query_as(
        r#"SELECT n.activity_type, m.status_id FROM notifications n
           JOIN mentions m ON m.id = n.activity_id
           WHERE n.account_id = $1 AND n.type = 'mention'"#,
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(activity_type, "Mention");
    assert_eq!(mention_status, status_id);
    let listed: Value = ctx
        .api
        .get("/api/v1/notifications", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed[0]["status"]["id"], status_id.to_string());

    assert_eq!(
        conversation_rows(&ctx, alice).await,
        vec![(vec![remy_id], true)]
    );
    assert_eq!(
        conversation_rows(&ctx, remy_id).await,
        vec![(vec![alice], false)]
    );
}

/// A direct message whose mention the recipient's policy files away is not
/// added to their conversations.
#[tokio::test]
async fn test_a_filtered_direct_message_is_not_a_conversation() {
    let ctx = TestContext::new("audience-dm-filtered").await;
    let (_, remy, key) = seed_remote(&ctx, "remy", "remote.invalid").await;
    ctx.api
        .patch_json(
            "/api/v2/notifications/policy",
            Some(&ctx.alice_token),
            &json!({"for_private_mentions": "filter"}),
        )
        .await;
    send(
        &ctx,
        &remy,
        &key,
        &create(
            &remy,
            json!({
                "id": format!("{remy}/statuses/60"), "type": "Note", "attributedTo": remy,
                "content": "<p>@alice psst</p>",
                "to": [local(&ctx, "alice")],
                "tag": [{"type": "Mention", "href": local(&ctx, "alice")}],
            }),
        ),
    )
    .await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let filtered: bool = sqlx::query_scalar(
        "SELECT filtered FROM notifications WHERE account_id = $1 AND type = 'mention'",
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(filtered);
    assert!(conversation_rows(&ctx, alice).await.is_empty());
}

/// A reply named after an option is a vote only on a local post's poll; on
/// a remote poll it is a status like any other.
#[tokio::test]
async fn test_a_vote_counts_only_on_a_local_poll() {
    let ctx = TestContext::new("audience-poll-vote").await;
    let (_, remy, key) = seed_remote(&ctx, "remy", "remote.invalid").await;
    let (victim_id, victim, _) = seed_remote(&ctx, "vic", "remote.invalid").await;
    let poll_status = format!("{victim}/statuses/70");
    let status_id = eunha::snowflake::next_id();
    sqlx::query(
        r#"INSERT INTO statuses (id, account_id, text, spoiler_text, visibility, uri, url, local, created_at, updated_at)
           VALUES ($1, $2, 'which?', '', 0, $3, $3, false, now(), now())"#,
    )
    .bind(status_id)
    .bind(victim_id)
    .bind(&poll_status)
    .execute(&ctx.db)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO polls (status_id, account_id, options, cached_tallies, votes_count, multiple, created_at, updated_at)
           VALUES ($1, $2, ARRAY['yes','no'], ARRAY[0,0]::bigint[], 0, false, now(), now())"#,
    )
    .bind(status_id)
    .bind(victim_id)
    .execute(&ctx.db)
    .await
    .unwrap();
    let vote = format!("{remy}/votes/1");
    send(
        &ctx,
        &remy,
        &key,
        &create(
            &remy,
            json!({
                "id": vote, "type": "Note", "attributedTo": remy, "name": "yes",
                "inReplyTo": poll_status,
                "to": [PUBLIC], "cc": [local(&ctx, "alice")],
            }),
        ),
    )
    .await;
    let votes: i64 = sqlx::query_scalar("SELECT count(*) FROM poll_votes")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(votes, 0);
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM statuses WHERE uri = $1")
        .bind(&vote)
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(stored, 1);
}
