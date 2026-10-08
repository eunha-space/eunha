use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// Bob follows alice, so that her direct messages to him are not filed away
/// by his notification policy (`for_private_mentions`, which filters a
/// private mention from someone he does not follow), and reach his
/// conversations as Mastodon adds them, with the notification.
async fn accept_private_mentions(ctx: &TestContext) {
    sqlx::query(
        "INSERT INTO follows (account_id, target_account_id, created_at, updated_at) VALUES ($1, $2, now(), now())",
    )
    .bind(ctx.bob_id.parse::<i64>().unwrap())
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .execute(&ctx.db)
    .await
    .unwrap();
}

/// Direct message creates a conversation visible to both sender and recipient.
#[tokio::test]
async fn test_conversations_lifecycle() {
    let ctx = TestContext::new("conv").await;
    accept_private_mentions(&ctx).await;

    let dm_resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "@bob hello in private",
                "visibility": "direct"
            }),
        )
        .await;
    assert_eq!(dm_resp.status(), StatusCode::OK);

    let convs: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!convs.is_empty(), "conversation not created for recipient");
    let conv_id = convs[0]["id"].as_str().unwrap().to_string();
    assert_eq!(convs[0]["unread"].as_bool(), Some(true));

    let read_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/conversations/{conv_id}/read"),
            Some(&ctx.bob_token),
            &json!({}),
        )
        .await;
    assert_eq!(read_resp.status(), StatusCode::OK);
    let conv: Value = read_resp.json().await.unwrap();
    assert_eq!(conv["unread"].as_bool(), Some(false));

    let del_resp = ctx
        .api
        .delete(&format!("/api/v1/conversations/{conv_id}"), &ctx.bob_token)
        .await;
    assert_eq!(del_resp.status(), StatusCode::OK);

    let after: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!after
        .iter()
        .any(|c| c["id"].as_str() == Some(conv_id.as_str())));
}

/// GET /api/v1/conversations respects limit parameter.
#[tokio::test]
async fn test_conversations_limit_param() {
    let ctx = TestContext::new("conv-limit").await;
    accept_private_mentions(&ctx).await;

    for i in 0..3 {
        ctx.api
            .post_json(
                "/api/v1/statuses",
                Some(&ctx.alice_token),
                &json!({
                    "status": format!("@bob conv limit {i}"),
                    "visibility": "direct"
                }),
            )
            .await;
    }

    let convs: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations?limit=2", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        convs.len() <= 2,
        "limit=2 returned {} conversations",
        convs.len()
    );
}

/// GET /api/v1/conversations since_id returns only conversations newer than the given id.
#[tokio::test]
async fn test_conversations_since_id() {
    let ctx = TestContext::new("conv-since").await;
    accept_private_mentions(&ctx).await;

    // First DM — older.
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "@bob conv since old", "visibility": "direct"}),
        )
        .await;

    let convs_all: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!convs_all.is_empty());
    let old_conv_id = convs_all[0]["id"].as_str().unwrap().to_string();

    // Delete it so we can send a fresh DM and get a new conversation.
    ctx.api
        .delete(
            &format!("/api/v1/conversations/{old_conv_id}"),
            &ctx.bob_token,
        )
        .await;

    // Second DM — newer (fresh conversation).
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "@bob conv since new", "visibility": "direct"}),
        )
        .await;

    let after: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/conversations?since_id={old_conv_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    // The new conversation id should be > old_conv_id.
    let new_id: i64 = after[0]["id"].as_str().unwrap().parse().unwrap();
    let old_id: i64 = old_conv_id.parse().unwrap();
    assert!(
        new_id > old_id,
        "since_id should only return conversations newer than anchor"
    );
}

/// GET /api/v1/conversations max_id returns only conversations older than the given id.
#[tokio::test]
async fn test_conversations_max_id() {
    let ctx = TestContext::new("conv-maxid").await;
    accept_private_mentions(&ctx).await;

    // First DM — older conversation.
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "@bob conv maxid old", "visibility": "direct"}),
        )
        .await;

    let convs_first: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!convs_first.is_empty());
    let old_id = convs_first[0]["id"].as_str().unwrap().to_string();

    // Delete so next DM is a new conversation with a higher id.
    ctx.api
        .delete(&format!("/api/v1/conversations/{old_id}"), &ctx.bob_token)
        .await;

    // Second DM — newer conversation.
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "@bob conv maxid new", "visibility": "direct"}),
        )
        .await;

    let convs_all: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!convs_all.is_empty());
    let new_id = convs_all[0]["id"].as_str().unwrap().to_string();

    // max_id=new_id should exclude new_id and return only older ones.
    let with_max: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/conversations?max_id={new_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        !with_max
            .iter()
            .any(|c| c["id"].as_str() == Some(new_id.as_str())),
        "max_id conversation itself should be excluded",
    );
}

/// GET /api/v1/conversations?min_id= returns conversations newer than the anchor.
#[tokio::test]
async fn test_conversations_min_id() {
    let ctx = TestContext::new("conv-minid").await;
    accept_private_mentions(&ctx).await;

    // First DM — creates conversation c1.
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "@bob conv minid first", "visibility": "direct"}),
        )
        .await;

    let convs_first: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!convs_first.is_empty());
    let c1_id = convs_first[0]["id"].as_str().unwrap().to_string();

    // Delete c1 so the next DM creates a new conversation.
    ctx.api
        .delete(&format!("/api/v1/conversations/{c1_id}"), &ctx.bob_token)
        .await;

    // Second DM — creates conversation c2 with a higher id.
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "@bob conv minid second", "visibility": "direct"}),
        )
        .await;

    let convs_all: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!convs_all.is_empty());
    let c2_id = convs_all[0]["id"].as_str().unwrap().to_string();

    // min_id=c1_id should return only c2 (newer than c1).
    let with_min: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/conversations?min_id={c1_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();

    assert!(
        with_min
            .iter()
            .any(|c| c["id"].as_str() == Some(c2_id.as_str())),
        "c2 should appear with min_id=c1_id",
    );
    assert!(
        !with_min
            .iter()
            .any(|c| c["id"].as_str() == Some(c1_id.as_str())),
        "c1 should be excluded by min_id",
    );
}

/// Sender also sees their own conversation.
#[tokio::test]
async fn test_conversations_visible_to_sender() {
    let ctx = TestContext::new("conv-sender").await;
    accept_private_mentions(&ctx).await;

    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "@bob a note to you",
                "visibility": "direct"
            }),
        )
        .await;

    let convs: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!convs.is_empty(), "sender should also see the conversation");
}

/// Conversations are ordered by id DESC so the pagination cursor is consistent with ordering.
#[tokio::test]
async fn test_conversations_ordered_by_id_desc() {
    let ctx = TestContext::new("conv-order").await;
    accept_private_mentions(&ctx).await;

    // Create two separate conversations by creating and deleting between each.
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "@bob conv order first", "visibility": "direct"}),
        )
        .await;

    let convs_first: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    let c1_id = convs_first[0]["id"].as_str().unwrap().to_string();
    ctx.api
        .delete(&format!("/api/v1/conversations/{c1_id}"), &ctx.bob_token)
        .await;

    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "@bob conv order second", "visibility": "direct"}),
        )
        .await;

    let convs: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();

    // c2 is newer (higher id) and should appear first.
    let c2_id = convs[0]["id"].as_str().unwrap().to_string();
    assert!(
        c2_id.parse::<i64>().unwrap() > c1_id.parse::<i64>().unwrap(),
        "newer conversation (higher id) should appear first"
    );

    // Adding c1 back by paginating: max_id=c2_id should not include c2.
    let paged: Vec<Value> = ctx
        .api
        .get(
            &format!("/api/v1/conversations?max_id={c2_id}"),
            Some(&ctx.bob_token),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !paged
            .iter()
            .any(|c| c["id"].as_str() == Some(c2_id.as_str())),
        "max_id=c2 should exclude c2 itself",
    );
}

/// Conversation response includes `accounts` (participant list) and `last_status`.
/// This covers the account_conversations.participant_account_ids and last_status_id
/// columns added when replacing conversation_participants with account_conversations.
#[tokio::test]
async fn test_conversation_includes_accounts_and_last_status() {
    let ctx = TestContext::new("conv-fields").await;
    accept_private_mentions(&ctx).await;

    let dm = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &serde_json::json!({
                "status": "@bob test conversation fields",
                "visibility": "direct"
            }),
        )
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    let dm_id = dm["id"].as_str().unwrap();

    // Bob's conversation list should include the DM with alice as participant and last_status set.
    let convs: Vec<serde_json::Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(
        !convs.is_empty(),
        "bob should have at least one conversation"
    );

    let conv = &convs[0];

    // accounts field must contain alice as a participant.
    let accounts = conv["accounts"]
        .as_array()
        .expect("conversation must have accounts array");
    assert!(
        accounts
            .iter()
            .any(|a| a["id"].as_str() == Some(ctx.alice_id.as_str())),
        "alice should be listed as a participant in bob's conversation",
    );

    // last_status must be the DM.
    let last_status = &conv["last_status"];
    assert!(
        last_status.is_object(),
        "conversation must have a last_status object"
    );
    assert_eq!(
        last_status["id"].as_str(),
        Some(dm_id),
        "last_status id must match the sent DM",
    );
}

/// POST /api/v1/conversations/:id/unread marks conversation unread and returns updated conversation.
#[tokio::test]
async fn test_mark_conversation_unread() {
    let ctx = TestContext::new("conv-unread").await;
    accept_private_mentions(&ctx).await;

    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &serde_json::json!({"status": "@bob mark unread", "visibility": "direct"}),
        )
        .await;

    let convs: Vec<serde_json::Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert!(!convs.is_empty());
    let conv_id = convs[0]["id"].as_str().unwrap().to_string();

    // Mark as read first.
    ctx.api
        .post_json(
            &format!("/api/v1/conversations/{conv_id}/read"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;

    // Mark as unread.
    let unread_resp = ctx
        .api
        .post_json(
            &format!("/api/v1/conversations/{conv_id}/unread"),
            Some(&ctx.bob_token),
            &serde_json::json!({}),
        )
        .await;
    assert_eq!(unread_resp.status(), StatusCode::OK);
    let conv: serde_json::Value = unread_resp.json().await.unwrap();
    assert_eq!(
        conv["unread"].as_bool(),
        Some(true),
        "conversation should be unread after marking unread"
    );
}

/// Two threads at once: deleting one leaves the other whole.
///
/// Every other test here keeps a single conversation alive, deleting it before
/// creating the next, so nothing covered what a list of several does.
#[tokio::test]
async fn test_delete_one_of_several_conversations() {
    let ctx = TestContext::new("conv-del-many").await;
    accept_private_mentions(&ctx).await;

    // Three separate top-level DMs — each one starts its own conversation.
    for i in 0..3 {
        ctx.api
            .post_json(
                "/api/v1/statuses",
                Some(&ctx.alice_token),
                &json!({"status": format!("@bob thread {i}"), "visibility": "direct"}),
            )
            .await;
    }

    let before: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        before.len(),
        3,
        "three top-level DMs are three conversations"
    );

    let gone = before[0]["id"].as_str().unwrap().to_string();
    let del = ctx
        .api
        .delete(&format!("/api/v1/conversations/{gone}"), &ctx.bob_token)
        .await;
    assert_eq!(del.status(), StatusCode::OK);

    let after: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(after.len(), 2, "deleting one of three leaves two");
    for c in &after {
        assert!(
            c["last_status"].is_object(),
            "surviving conversation {} lost its last_status",
            c["id"]
        );
    }
}

/// One conversation, two rows: the ids stay distinct and deleting one keeps the other.
///
/// `account_conversations` is unique on
/// (account_id, conversation_id, participant_account_ids), so a thread whose
/// participant set changes gives an account a second row for the same
/// conversation — production has several. Serving `conversation_id` as the API
/// id made those two rows share an id, and a client keying its list by id then
/// rendered and deleted the wrong ones.
#[tokio::test]
async fn test_conversation_ids_are_per_row_not_per_conversation() {
    let ctx = TestContext::new("conv-dup-id").await;
    accept_private_mentions(&ctx).await;
    let (_carol_id, _carol_token) = crate::helpers::seed_account_and_token(
        &ctx.db,
        &ctx.domain,
        "carol",
        "carol@conv-dup-id.invalid",
    )
    .await;

    // A DM to bob alone, then a reply in the same thread that adds carol. Bob
    // ends up with two rows for one conversation, differing in participants.
    let first: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({"status": "@bob just you", "visibility": "direct"}),
        )
        .await
        .json()
        .await
        .unwrap();
    let first_id = first["id"].as_str().unwrap().to_string();

    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "@bob @carol and you too",
                "visibility": "direct",
                "in_reply_to_id": first_id,
            }),
        )
        .await;

    let rows = sqlx::query!(
        "SELECT count(*) AS n FROM account_conversations WHERE account_id = $1",
        ctx.bob_id.parse::<i64>().unwrap(),
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(
        rows.n,
        Some(2),
        "adding a participant should give bob a second row for the same conversation",
    );

    let convs: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(convs.len(), 2);

    let ids: Vec<&str> = convs.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_ne!(
        ids[0], ids[1],
        "two rows of one conversation must not share an id",
    );

    // Each row carries its own last status, rather than one of them taking it.
    for c in &convs {
        assert!(
            c["last_status"].is_object(),
            "conversation {} has no last_status",
            c["id"],
        );
    }

    // Deleting one leaves the other, which is what a client acting on an id
    // is entitled to expect.
    let gone = ids[0].to_string();
    let kept = ids[1].to_string();
    let del = ctx
        .api
        .delete(&format!("/api/v1/conversations/{gone}"), &ctx.bob_token)
        .await;
    assert_eq!(del.status(), StatusCode::OK);

    let after: Vec<Value> = ctx
        .api
        .get("/api/v1/conversations", Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    let after_ids: Vec<&str> = after.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(
        after_ids,
        vec![kept.as_str()],
        "deleting one row should leave exactly the other",
    );
}
