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
