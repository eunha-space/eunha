use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

/// Helper: post a status with a poll and return the status JSON.
async fn post_poll_status(ctx: &TestContext) -> Value {
    ctx.api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Which do you prefer?",
                "visibility": "public",
                "poll": {
                    "options": ["Cats", "Dogs"],
                    "expires_in": 86400,
                    "multiple": false
                }
            }),
        )
        .await
        .json()
        .await
        .unwrap()
}

/// `VoteService` votes under `with_redis_lock("vote:<poll>:<account>")`, and
/// `ReblogsController#create` boosts under `"reblog:<account>:<status>"`:
/// held by another, either is a 503; released, it goes through.
#[tokio::test]
async fn test_votes_and_boosts_take_their_locks() {
    let ctx = TestContext::new("poll-vote-lock").await;
    let status: Value = post_poll_status(&ctx).await;
    let poll_id = status["poll"]["id"].as_str().unwrap().to_owned();
    let status_id = status["id"].as_str().unwrap().to_owned();

    let held = eunha::redis_lock::try_acquire(
        &ctx.state,
        &format!("lock:vote:{poll_id}:{}", ctx.bob_id),
        60_000,
    )
    .await
    .unwrap();
    let vote_path = format!("/api/v1/polls/{poll_id}/votes");
    let choices = json!({ "choices": [0] });
    let busy = ctx
        .api
        .post_json(&vote_path, Some(&ctx.bob_token), &choices)
        .await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    held.release().await;
    let voted = ctx
        .api
        .post_json(&vote_path, Some(&ctx.bob_token), &choices)
        .await;
    assert_eq!(voted.status(), StatusCode::OK);

    let held = eunha::redis_lock::try_acquire(
        &ctx.state,
        &format!("lock:reblog:{}:{status_id}", ctx.bob_id),
        60_000,
    )
    .await
    .unwrap();
    let reblog_path = format!("/api/v1/statuses/{status_id}/reblog");
    let busy = ctx
        .api
        .post_json(&reblog_path, Some(&ctx.bob_token), &json!({}))
        .await;
    assert_eq!(busy.status(), StatusCode::SERVICE_UNAVAILABLE);
    held.release().await;
    let boosted = ctx
        .api
        .post_json(&reblog_path, Some(&ctx.bob_token), &json!({}))
        .await;
    assert_eq!(boosted.status(), StatusCode::OK);
}

/// GET /api/v1/polls/:id returns the poll data.
#[tokio::test]
async fn test_poll_get() {
    let ctx = TestContext::new("poll-get").await;
    let status: Value = post_poll_status(&ctx).await;
    let poll_id = status["poll"]["id"].as_str().expect("poll.id missing");

    let resp = ctx
        .api
        .get(
            &format!("/api/v1/polls/{}", poll_id),
            Some(&ctx.alice_token),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let poll: Value = resp.json().await.unwrap();
    assert_eq!(poll["id"].as_str(), Some(poll_id));
    let options = poll["options"].as_array().unwrap();
    assert_eq!(options.len(), 2);
    assert_eq!(options[0]["title"].as_str(), Some("Cats"));
    assert_eq!(options[1]["title"].as_str(), Some("Dogs"));
}

/// GET /api/v1/polls/:id for nonexistent id returns 404.
#[tokio::test]
async fn test_poll_get_not_found() {
    let ctx = TestContext::new("poll-get-404").await;

    let resp = ctx
        .api
        .get("/api/v1/polls/999999999", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// POST /api/v1/polls/:id/votes casts a vote and reflects it in the poll.
#[tokio::test]
async fn test_poll_vote() {
    let ctx = TestContext::new("poll-vote").await;
    let status: Value = post_poll_status(&ctx).await;
    let poll_id = status["poll"]["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{}/votes", poll_id),
            Some(&ctx.bob_token),
            &json!({ "choices": [0] }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "vote should succeed");
    let poll: Value = resp.json().await.unwrap();
    assert_eq!(poll["votes_count"].as_i64(), Some(1));
    let options = poll["options"].as_array().unwrap();
    assert_eq!(options[0]["votes_count"].as_i64(), Some(1));
    assert_eq!(options[1]["votes_count"].as_i64(), Some(0));
}

/// Voting on a poll you already voted in returns 422.
#[tokio::test]
async fn test_poll_vote_duplicate() {
    let ctx = TestContext::new("poll-vote-dup").await;
    let status: Value = post_poll_status(&ctx).await;
    let poll_id = status["poll"]["id"].as_str().unwrap();

    ctx.api
        .post_json(
            &format!("/api/v1/polls/{}/votes", poll_id),
            Some(&ctx.bob_token),
            &json!({ "choices": [0] }),
        )
        .await;

    let second = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{}/votes", poll_id),
            Some(&ctx.bob_token),
            &json!({ "choices": [1] }),
        )
        .await;
    assert_eq!(
        second.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "duplicate vote should be 422"
    );
}

/// Voting with an out-of-range choice index returns 422.
#[tokio::test]
async fn test_poll_vote_invalid_choice() {
    let ctx = TestContext::new("poll-vote-invalid").await;
    let status: Value = post_poll_status(&ctx).await;
    let poll_id = status["poll"]["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{}/votes", poll_id),
            Some(&ctx.bob_token),
            &json!({ "choices": [99] }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid choice should be 422"
    );
}

/// The poll author cannot vote on their own poll.
#[tokio::test]
async fn test_poll_owner_cannot_vote() {
    let ctx = TestContext::new("poll-owner-vote").await;
    let status: Value = post_poll_status(&ctx).await;
    let poll_id = status["poll"]["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{}/votes", poll_id),
            Some(&ctx.alice_token),
            &json!({ "choices": [0] }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "owner voting on own poll should be 422"
    );
}

/// A multiple-choice poll accepts multiple selections.
#[tokio::test]
async fn test_poll_multiple_choice_vote() {
    let ctx = TestContext::new("poll-multi").await;

    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Pick all that apply",
                "visibility": "public",
                "poll": {
                    "options": ["Red", "Green", "Blue"],
                    "expires_in": 86400,
                    "multiple": true
                }
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let poll_id = status["poll"]["id"].as_str().unwrap();

    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{}/votes", poll_id),
            Some(&ctx.bob_token),
            &json!({ "choices": [0, 2] }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "multi-choice vote should succeed"
    );
    let poll: Value = resp.json().await.unwrap();
    assert_eq!(poll["votes_count"].as_i64(), Some(2));
}

/// Creating a status with a poll requires at least 2 options.
#[tokio::test]
async fn test_poll_create_requires_two_options() {
    let ctx = TestContext::new("poll-min-opts").await;

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Bad poll",
                "poll": {
                    "options": ["Only one"],
                    "expires_in": 86400
                }
            }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "single-option poll should be 422"
    );
}

/// Poll `voted` field reflects whether the authenticated user has voted.
#[tokio::test]
async fn test_poll_voted_field() {
    let ctx = TestContext::new("poll-voted").await;
    let status: Value = post_poll_status(&ctx).await;
    let poll_id = status["poll"]["id"].as_str().unwrap();

    // Before voting: voted should be false.
    let before: Value = ctx
        .api
        .get(&format!("/api/v1/polls/{}", poll_id), Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(before["voted"].as_bool(), Some(false));

    // Cast vote.
    ctx.api
        .post_json(
            &format!("/api/v1/polls/{}/votes", poll_id),
            Some(&ctx.bob_token),
            &json!({ "choices": [1] }),
        )
        .await;

    // After voting: voted should be true and own_votes should list choice.
    let after: Value = ctx
        .api
        .get(&format!("/api/v1/polls/{}", poll_id), Some(&ctx.bob_token))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(after["voted"].as_bool(), Some(true));
    let own = after["own_votes"].as_array().unwrap();
    assert!(own.iter().any(|v| v.as_i64() == Some(1)));
}

/// A poll on a post the viewer may not see is not there for them
/// (`authorize @poll.status, :show?`), to read or to vote in.
#[tokio::test]
async fn test_poll_of_a_hidden_status_is_not_found() {
    let ctx = TestContext::new("poll-hidden").await;
    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Followers only",
                "visibility": "private",
                "poll": {"options": ["Yes", "No"], "expires_in": 86400}
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    let poll_id = status["poll"]["id"].as_str().unwrap();
    let path = format!("/api/v1/polls/{poll_id}");

    assert_eq!(
        ctx.api.get(&path, Some(&ctx.alice_token)).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        ctx.api.get(&path, None).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        ctx.api.get(&path, Some(&ctx.bob_token)).await.status(),
        StatusCode::NOT_FOUND
    );
    let vote = ctx
        .api
        .post_json(
            &format!("{path}/votes"),
            Some(&ctx.bob_token),
            &json!({ "choices": [0] }),
        )
        .await;
    assert_eq!(vote.status(), StatusCode::NOT_FOUND);

    // A follower sees it.
    ctx.api.follow(&ctx.bob_token, &ctx.alice_id).await;
    assert_eq!(
        ctx.api.get(&path, Some(&ctx.bob_token)).await.status(),
        StatusCode::OK
    );
}

/// The tallies are those the poll keeps, raised by each vote, the voters
/// counted once each; with `hide_totals` the options' counts are hidden
/// until it closes.
#[tokio::test]
async fn test_poll_tallies_are_kept_as_mastodon_keeps_them() {
    let ctx = TestContext::new("poll-tallies").await;
    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Pick",
                "visibility": "public",
                "poll": {"options": ["A", "B", "C"], "expires_in": 86400, "multiple": true}
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(status["poll"]["voters_count"], 0);
    let poll_id = status["poll"]["id"].as_str().unwrap();
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{poll_id}/votes"),
            Some(&ctx.bob_token),
            &json!({ "choices": [0, 2] }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let poll: Value = resp.json().await.unwrap();
    assert_eq!(poll["votes_count"], 2);
    assert_eq!(poll["voters_count"], 1);
    let counts: Vec<i64> = poll["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["votes_count"].as_i64().unwrap())
        .collect();
    assert_eq!(counts, vec![1, 0, 1]);
    let (tallies,): (Vec<i64>,) = sqlx::query_as("SELECT cached_tallies FROM polls WHERE id = $1")
        .bind(poll_id.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(tallies, vec![1, 0, 1]);

    let hidden: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &json!({
                "status": "Secret ballot",
                "visibility": "public",
                "poll": {"options": ["X", "Y"], "expires_in": 86400, "hide_totals": true}
            }),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(hidden["poll"]["options"][0]["votes_count"].is_null());
}

/// `VoteService#distribute_poll!`: a vote on a local poll sends its tallies
/// three minutes later, by `ActivityPub::DistributePollUpdateWorker`.
#[tokio::test]
async fn test_a_local_vote_schedules_the_poll_update() {
    let ctx = TestContext::new("poll-vote-update").await;
    let status = post_poll_status(&ctx).await;
    // Queued, not run at once, so that the job is still there to look at.
    ctx.state.jobs.set_mode(eunha::jobs::Mode::Durable);
    let poll_id = status["poll"]["id"].as_str().unwrap();
    let resp = ctx
        .api
        .post_json(
            &format!("/api/v1/polls/{poll_id}/votes"),
            Some(&ctx.bob_token),
            &json!({"choices": [0]}),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let (count, later): (i64, bool) = sqlx::query_as(
        r#"SELECT count(*), bool_and(run_at > now() + interval '2 minutes') FROM eunha.jobs
           WHERE kind = 'ActivityPub::DistributePollUpdateWorker'
             AND (args->>'status_id')::bigint = $1"#,
    )
    .bind(status["id"].as_str().unwrap().parse::<i64>().unwrap())
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(count, 1);
    assert!(later);
}
