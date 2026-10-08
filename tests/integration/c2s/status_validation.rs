//! What `POST /api/v1/statuses` refuses, and with what, as
//! `Api::V1::StatusesController`, `PostStatusService` and `Status`'s
//! validations do.

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn post(ctx: &TestContext, token: &str, body: Value) -> (StatusCode, Value) {
    let resp = ctx
        .api
        .post_json("/api/v1/statuses", Some(token), &body)
        .await;
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `validates :text, presence: true, unless: -> { with_media? || reblog? ||
/// with_quote? }`: text that is only whitespace is no text, and a poll does
/// not stand in for it.
#[tokio::test]
async fn test_a_post_needs_text_unless_it_has_media_or_quotes() {
    let ctx = TestContext::new("validate-text").await;
    let poll = json!({"options": ["a", "b"], "expires_in": 3600});
    for body in [
        json!({"status": "   \n"}),
        json!({"status": "", "poll": poll}),
        json!({"poll": poll}),
    ] {
        let (status, error) = post(&ctx, &ctx.alice_token, body.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(
            error["error"], "Validation failed: Text can't be blank",
            "{body}"
        );
    }

    // A lone content warning is the text (`preprocess_attributes!`).
    let (status, posted) = post(&ctx, &ctx.alice_token, json!({"spoiler_text": "cw"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(posted["content"], "<p>cw</p>");
    assert_eq!(posted["spoiler_text"], "");
    assert_eq!(posted["sensitive"], true);

    // A quote needs no text.
    let quoted = ctx
        .api
        .post_status(&ctx.bob_token, "quote me", "public")
        .await;
    let (status, _) = post(
        &ctx,
        &ctx.alice_token,
        json!({"status": " ", "quoted_status_id": quoted["id"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// `set_thread`: replying to a post that is not there, or that the poster
/// may not see, is a 404 with `statuses.errors.in_reply_not_found`.
#[tokio::test]
async fn test_a_reply_to_a_post_one_cannot_see_is_not_found() {
    let ctx = TestContext::new("validate-thread").await;
    let private = ctx
        .api
        .post_status(&ctx.bob_token, "followers only", "private")
        .await;
    for parent in [private["id"].clone(), json!("1"), json!("nope")] {
        let (status, error) = post(
            &ctx,
            &ctx.alice_token,
            json!({"status": "reply", "in_reply_to_id": parent}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{parent}");
        assert_eq!(
            error["error"],
            "The post you are trying to reply to does not appear to exist."
        );
    }

    // Once she follows him, she may.
    ctx.api.follow(&ctx.alice_token, &ctx.bob_id).await;
    let (status, reply) = post(
        &ctx,
        &ctx.alice_token,
        json!({"status": "reply", "in_reply_to_id": private["id"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reply["in_reply_to_id"], private["id"]);
}

/// `DisallowedHashtagsValidator`: a hashtag a moderator made unusable
/// refuses the post, named as the tag is stored.
#[tokio::test]
async fn test_a_disallowed_hashtag_refuses_the_post() {
    let ctx = TestContext::new("validate-hashtags").await;
    for name in ["Banned", "forbidden"] {
        sqlx::query(
            "INSERT INTO tags (name, usable, created_at, updated_at) VALUES ($1, false, now(), now())",
        )
        .bind(name)
        .execute(&ctx.db)
        .await
        .unwrap();
    }
    let (status, error) = post(&ctx, &ctx.alice_token, json!({"status": "#banned #fine"})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        error["error"],
        "Validation failed: Text contained a disallowed hashtag: Banned"
    );
    let (status, error) = post(
        &ctx,
        &ctx.alice_token,
        json!({"status": "#BANNED #Forbidden"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        error["error"],
        "Validation failed: Text contained the disallowed hashtags: Banned, forbidden"
    );
    let (status, _) = post(&ctx, &ctx.alice_token, json!({"status": "#fine"})).await;
    assert_eq!(status, StatusCode::OK);
}

/// `@options[:scheduled_at]&.to_datetime`: a time with no zone is UTC, a
/// blank one schedules nothing, and one that does not parse is a bare
/// `RecordInvalid`. A post that would not be valid is not scheduled either.
#[tokio::test]
async fn test_scheduled_at_is_read_as_to_datetime_reads_it() {
    let ctx = TestContext::new("validate-scheduled-at").await;
    let (status, scheduled) = post(
        &ctx,
        &ctx.alice_token,
        json!({"status": "later", "scheduled_at": "2099-10-01T12:00:00"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(scheduled["scheduled_at"], "2099-10-01T12:00:00.000Z");

    let (status, posted) = post(
        &ctx,
        &ctx.alice_token,
        json!({"status": "now", "scheduled_at": ""}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(posted["content"], "<p>now</p>");

    for body in [
        json!({"status": "when?", "scheduled_at": "soon"}),
        json!({"status": "", "scheduled_at": "2099-10-01T12:00:00Z"}),
    ] {
        let (status, error) = post(&ctx, &ctx.alice_token, body.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(error["error"], "Record invalid", "{body}");
    }
}

/// `Poll#prepare_options` strips each option and drops the blank ones before
/// the poll is validated; what is still wrong is named as a status's
/// `poll_attributes` name it.
#[tokio::test]
async fn test_poll_options_are_prepared_then_validated() {
    let ctx = TestContext::new("validate-poll").await;
    let (status, posted) = post(
        &ctx,
        &ctx.alice_token,
        json!({"status": "which?", "poll": {"options": [" a ", "", "b", "\u{3000}"], "expires_in": 3600}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let titles: Vec<&str> = posted["poll"]["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["a", "b"]);

    for (poll, message) in [
        (
            json!({"options": ["a", " "], "expires_in": 3600}),
            "Validation failed: Poll options must have more than one item",
        ),
        (
            json!({"options": [], "expires_in": 60}),
            "Validation failed: Poll options can't be blank, Poll options must have more than one item, Poll expires at is too soon",
        ),
        (
            json!({"options": ["a", "a", "b", "c", "d"]}),
            "Validation failed: Poll expires at can't be blank, Poll options can't contain more than 4 items, Poll options contain duplicate items",
        ),
        (
            json!({"options": ["a", "b"], "expires_in": 99_999_999}),
            "Validation failed: Poll expires at is too far into the future",
        ),
    ] {
        let (status, error) = post(
            &ctx,
            &ctx.alice_token,
            json!({"status": "which?", "poll": poll}),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{poll}");
        assert_eq!(error["error"], message, "{poll}");
    }
}

// ── PUT /api/v1/statuses/:id ──────────────────────────────────────────────

async fn edit(ctx: &TestContext, id: &str, body: Value) -> (StatusCode, Value) {
    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/statuses/{id}"),
            Some(&ctx.alice_token),
            &body,
        )
        .await;
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// An upload of alice's, not yet on a status.
async fn upload(ctx: &TestContext, description: &str) -> i64 {
    let id = eunha::snowflake::next_id();
    sqlx::query(
        "INSERT INTO media_attachments (id, account_id, remote_url, type, description, created_at, updated_at)
         VALUES ($1, $2, $3, 0, $4, now(), now())",
    )
    .bind(id)
    .bind(ctx.alice_id.parse::<i64>().unwrap())
    .bind(format!("https://files.example/{id}.png"))
    .bind(description)
    .execute(&ctx.db)
    .await
    .unwrap();
    id
}

/// `UpdateStatusService#update_immediate_attributes!`: a blank text becomes
/// the content warning given, which then leaves the post's content warning
/// as it was and does not mark it sensitive; a post left with no text and no
/// media is refused as `Status` refuses it, and so is a disallowed hashtag.
#[tokio::test]
async fn test_an_edit_validates_its_text_as_mastodon_does() {
    let ctx = TestContext::new("validate-edit-text").await;
    let (_, posted) = post(
        &ctx,
        &ctx.alice_token,
        json!({"status": "body", "spoiler_text": "old warning"}),
    )
    .await;
    let id = posted["id"].as_str().unwrap();

    let (status, edited) = edit(
        &ctx,
        id,
        json!({"status": " ", "spoiler_text": "now the text"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["content"], "<p>now the text</p>");
    assert_eq!(edited["spoiler_text"], "old warning");
    assert_eq!(edited["sensitive"], false);

    for body in [json!({"status": ""}), json!({})] {
        let (status, error) = edit(&ctx, id, body.clone()).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(
            error["error"], "Validation failed: Text can't be blank",
            "{body}"
        );
    }

    sqlx::query(
        "INSERT INTO tags (name, usable, created_at, updated_at) VALUES ('nope', false, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let (status, error) = edit(&ctx, id, json!({"status": "#nope"})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        error["error"],
        "Validation failed: Text contained a disallowed hashtag: nope"
    );
    // Nothing of a refused edit is kept.
    let history: i64 = sqlx::query_scalar("SELECT count(*) FROM status_edits WHERE status_id = $1")
        .bind(id.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(history, 2);
}

/// The controller hands `UpdateStatusService` every field, `nil` for those
/// the request leaves out: an edit that does not give the attachments or
/// the poll takes them off, and one that gives `media_attributes` changes
/// the attachments' descriptions and focus.
#[tokio::test]
async fn test_an_edit_says_what_the_post_now_is() {
    let ctx = TestContext::new("validate-edit-whole").await;
    let picture = upload(&ctx, "before").await;
    let (_, posted) = post(
        &ctx,
        &ctx.alice_token,
        json!({"status": "look", "media_ids": [picture.to_string()],
               "poll": {"options": ["yes", "no"], "expires_in": 3600}}),
    )
    .await;
    let id = posted["id"].as_str().unwrap();

    let (status, edited) = edit(
        &ctx,
        id,
        json!({"status": "look", "media_ids": [picture.to_string()],
               "media_attributes": [{"id": picture.to_string(), "description": "after",
                                     "focus": "0.5,-0.25"}],
               "poll": {"options": ["yes", "no"], "expires_in": 3600}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["media_attachments"][0]["description"], "after");
    assert_eq!(
        edited["media_attachments"][0]["meta"]["focus"],
        json!({"x": 0.5, "y": -0.25})
    );
    assert_eq!(edited["poll"]["options"][0]["title"], "yes");

    let (status, edited) = edit(&ctx, id, json!({"status": "look"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(edited["media_attachments"], json!([]));
    assert!(edited["poll"].is_null());
    let polls: i64 = sqlx::query_scalar("SELECT count(*) FROM polls WHERE status_id = $1")
        .bind(id.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(polls, 0);

    let history: Vec<Value> = ctx
        .api
        .get(&format!("/api/v1/statuses/{id}/history"), None)
        .await
        .json()
        .await
        .unwrap();
    let descriptions: Vec<Vec<&str>> = history
        .iter()
        .map(|v| {
            v["media_attachments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["description"].as_str().unwrap())
                .collect()
        })
        .collect();
    assert_eq!(descriptions, [vec!["before"], vec!["after"], vec![]]);
}
