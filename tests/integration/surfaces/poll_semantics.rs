//! What a poll says about the account reading it.
//!
//! Three fields on a poll depend on the viewer rather than the poll, and all
//! three were wrong in ways a shape comparison cannot see — the fields were
//! present, or absent in a way that looked permitted, and held the wrong thing.
//! Mastodon's rules are in `Poll` and `REST::PollSerializer`.

use crate::helpers::TestContext;

async fn poll_of(ctx: &TestContext, token: &str, status_id: &str) -> serde_json::Value {
    let status: serde_json::Value = ctx
        .api
        .get(&format!("/api/v1/statuses/{status_id}"), Some(token))
        .await
        .json()
        .await
        .unwrap();
    status["poll"].clone()
}

async fn post_poll(ctx: &TestContext, token: &str) -> serde_json::Value {
    let response = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(token),
            &serde_json::json!({
                "status": "which one?",
                "poll": {"options": ["this", "that"], "expires_in": 3600},
            }),
        )
        .await;
    assert_eq!(response.status().as_u16(), 200, "posting a poll");
    response.json().await.unwrap()
}

/// An author has, in effect, already answered their own poll.
///
/// `Poll#voted?` is `account.id == account_id || votes.exists?`. A client reads
/// this to decide whether to offer the choices, so an author who sees `false`
/// is offered a vote on their own poll that they cannot cast.
#[tokio::test]
async fn test_a_poll_reads_as_voted_to_its_author() {
    let ctx = TestContext::new("poll-author-voted").await;

    let status = post_poll(&ctx, &ctx.alice_token).await;
    let id = status["id"].as_str().unwrap();

    let mine = poll_of(&ctx, &ctx.alice_token, id).await;
    assert_eq!(
        mine["voted"].as_bool(),
        Some(true),
        "the author of a poll counts as having voted: {mine}"
    );

    // And someone else, who has not voted, sees false.
    let theirs = poll_of(&ctx, &ctx.bob_token, id).await;
    assert_eq!(
        theirs["voted"].as_bool(),
        Some(false),
        "another account has not voted: {theirs}"
    );
}

/// `own_votes` accompanies `voted`, and is an empty list rather than absent.
///
/// Both are `if: :current_user?` in the serializer — present together for any
/// authenticated request. eunha omitted `own_votes` until a vote existed, so a
/// client reading `poll.own_votes.length` found nothing to read.
#[tokio::test]
async fn test_own_votes_is_present_and_empty_before_voting() {
    let ctx = TestContext::new("poll-own-votes").await;

    let status = post_poll(&ctx, &ctx.alice_token).await;
    let id = status["id"].as_str().unwrap();

    let theirs = poll_of(&ctx, &ctx.bob_token, id).await;
    assert_eq!(
        theirs["own_votes"].as_array().map(Vec::len),
        Some(0),
        "own_votes should be an empty list, not absent: {theirs}"
    );

    // After voting it holds the choice.
    let voted = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{}/votes", theirs["id"].as_str().unwrap()),
            Some(&ctx.bob_token),
            &serde_json::json!({"choices": [1]}),
        )
        .await;
    assert_eq!(voted.status().as_u16(), 200, "voting");

    let after = poll_of(&ctx, &ctx.bob_token, id).await;
    assert_eq!(after["own_votes"].as_array().map(|v| v.len()), Some(1));
    assert_eq!(after["voted"].as_bool(), Some(true));
}

/// `voters_count` is a number on every poll, not only multiple-choice ones.
///
/// Mastodon's documentation says null when `multiple` is false; its `Poll`
/// initialises the column to 0 for every poll and the serializer emits it as it
/// stands. Clients are written against what the server sends.
#[tokio::test]
async fn test_voters_count_is_a_number_on_a_single_choice_poll() {
    let ctx = TestContext::new("poll-voters-count").await;

    let status = post_poll(&ctx, &ctx.alice_token).await;
    let poll = poll_of(&ctx, &ctx.alice_token, status["id"].as_str().unwrap()).await;

    assert_eq!(poll["multiple"].as_bool(), Some(false));
    assert_eq!(
        poll["voters_count"].as_i64(),
        Some(0),
        "a fresh single-choice poll should report zero voters, not null: {poll}"
    );
}

/// A vote is accepted whether its choices are numbers or strings.
///
/// Rails coerces `"1"` to `1` without comment, so Mastodon takes both — checked
/// against a running 4.7.0, which answers 200 either way. A client sending
/// form-encoded parameters has only strings to send, and eunha answered those
/// with 422: a vote Mastodon would have counted, refused.
#[tokio::test]
async fn test_a_vote_accepts_string_or_numeric_choices() {
    let ctx = TestContext::new("poll-vote-formats").await;

    for choices in [serde_json::json!([1]), serde_json::json!(["1"])] {
        let status = post_poll(&ctx, &ctx.alice_token).await;
        let poll_id = status["poll"]["id"].as_str().unwrap().to_string();

        let response = ctx
            .api
            .post_json(
                &format!("/api/v1/polls/{poll_id}/votes"),
                Some(&ctx.bob_token),
                &serde_json::json!({"choices": choices}),
            )
            .await;
        assert_eq!(
            response.status().as_u16(),
            200,
            "choices as {choices} should be accepted"
        );

        let poll: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            poll["own_votes"].as_array().map(Vec::len),
            Some(1),
            "the vote should be recorded whichever way it was sent: {poll}"
        );
    }
}

/// An account cannot vote in a poll it posted, as Mastodon refuses with
/// "You cannot vote in your own polls".
#[tokio::test]
async fn test_an_author_cannot_vote_in_their_own_poll() {
    let ctx = TestContext::new("poll-author-vote").await;

    let status = post_poll(&ctx, &ctx.alice_token).await;
    let poll_id = status["poll"]["id"].as_str().unwrap();

    let response = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{poll_id}/votes"),
            Some(&ctx.alice_token),
            &serde_json::json!({"choices": [0]}),
        )
        .await;
    assert_eq!(
        response.status().as_u16(),
        422,
        "an author voting in their own poll is refused"
    );
}
