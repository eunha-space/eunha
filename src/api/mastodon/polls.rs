use axum::extract::{Extension, Json, Path};
use serde::Deserialize;

use super::types::{Poll, PollOption};
use crate::{
    db::models,
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

// ── GET /api/v1/polls/:id ─────────────────────────────────────────────────

pub async fn get_poll(
    state: AppState,
    Path(id): Path<i64>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Poll>> {
    let poll = fetch_poll(&state, id).await?;
    let viewer_id = auth.map(|Extension(a)| a.account_id);
    poll_from_db(&state, &poll, viewer_id).await.map(Json)
}

// ── POST /api/v1/polls/:id/votes ──────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PollVoteForm {
    /// Option indices. Accepted as numbers or as strings, because Mastodon
    /// accepts both — Rails coerces `"1"` to `1` without comment, and a client
    /// that sends form-encoded parameters has only strings to send. Requiring
    /// numbers turned a vote a Mastodon server would have counted into a 422.
    #[serde(deserialize_with = "deserialize_choices")]
    pub choices: Vec<i32>,
}

fn deserialize_choices<'de, D>(deserializer: D) -> Result<Vec<i32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;

    let raw = Vec::<serde_json::Value>::deserialize(deserializer)?;
    raw.into_iter()
        .map(|value| match value {
            serde_json::Value::Number(n) => n
                .as_i64()
                .and_then(|i| i32::try_from(i).ok())
                .ok_or_else(|| D::Error::custom("choice out of range")),
            serde_json::Value::String(s) => s
                .parse::<i32>()
                .map_err(|_| D::Error::custom("choice is not a number")),
            _ => Err(D::Error::custom("choice is not a number")),
        })
        .collect()
}

pub async fn vote_poll(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<PollVoteForm>,
) -> AppResult<Json<Poll>> {
    auth.require_scope("write:statuses")?;
    let poll = fetch_poll(&state, id).await?;

    let expired = poll
        .expires_at
        .map(|e| e < chrono::Utc::now().naive_utc())
        .unwrap_or(false);
    if expired {
        return Err(AppError::Unprocessable("Poll has expired".into()));
    }

    if poll.account_id == auth.account_id {
        return Err(AppError::Unprocessable(
            "You cannot vote on your own poll".into(),
        ));
    }

    let option_count = poll.options.len() as i32;
    if !poll.multiple && form.choices.len() > 1 {
        return Err(AppError::Unprocessable(
            "Multiple choices not allowed".into(),
        ));
    }

    // Single-choice: block re-voting entirely. Multi-choice: only block same choice (ON CONFLICT).
    if !poll.multiple {
        let already_voted = sqlx::query_scalar!(
            "SELECT EXISTS(SELECT 1 FROM poll_votes WHERE poll_id = $1 AND account_id = $2)",
            id,
            auth.account_id,
        )
        .fetch_one(&state.db)
        .await?
        .unwrap_or(false);
        if already_voted {
            return Err(AppError::Unprocessable("Already voted".into()));
        }
    }

    let was_first_vote = sqlx::query_scalar!(
        "SELECT NOT EXISTS(SELECT 1 FROM poll_votes WHERE poll_id = $1 AND account_id = $2)",
        id,
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(true);

    let mut created_votes: Vec<(i64, i32)> = Vec::new();
    for choice in &form.choices {
        if *choice < 0 || *choice >= option_count {
            return Err(AppError::Unprocessable("Invalid choice index".into()));
        }
        if let Some(vote) = sqlx::query!(
            r#"INSERT INTO poll_votes (poll_id, account_id, choice, created_at, updated_at)
               VALUES ($1, $2, $3, now(), now())
               ON CONFLICT DO NOTHING
               RETURNING id, choice"#,
            id,
            auth.account_id,
            choice,
        )
        .fetch_optional(&state.db)
        .await?
        {
            created_votes.push((vote.id, vote.choice));
        }
    }

    // Recount votes_count; increment voters_count for multi-choice if this is a new voter.
    sqlx::query!(
        "UPDATE polls SET votes_count = (SELECT COUNT(*) FROM poll_votes WHERE poll_id = $1), updated_at = now() WHERE id = $1",
        id,
    )
    .execute(&state.db)
    .await?;
    if poll.multiple && was_first_vote {
        sqlx::query!(
            "UPDATE polls SET voters_count = COALESCE(voters_count, 0) + 1, updated_at = now() WHERE id = $1",
            id,
        )
        .execute(&state.db)
        .await?;
    }

    // `VoteService`: `ActivityTracker.increment('activity:interactions')`.
    if !form.choices.is_empty() {
        crate::activity_tracker::increment(&state, crate::activity_tracker::INTERACTIONS).await;
    }

    if let Err(e) = federate_poll_votes(&state, &poll, auth.account_id, &created_votes).await {
        tracing::warn!(poll_id = id, error = %e, "failed to enqueue ActivityPub poll vote");
    }

    let poll = fetch_poll(&state, id).await?;
    if !poll.hide_totals {
        if let Err(e) = federate_poll_update(&state, poll.status_id).await {
            tracing::warn!(poll_id = id, error = %e, "failed to enqueue ActivityPub poll update");
        }
    }
    poll_from_db(&state, &poll, Some(auth.account_id))
        .await
        .map(Json)
}

// ── Helpers ────────────────────────────────────────────────────────────────

async fn fetch_poll(state: &AppState, id: i64) -> AppResult<models::Poll> {
    sqlx::query_as!(models::Poll, "SELECT * FROM polls WHERE id = $1", id,)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

async fn federate_poll_votes(
    state: &AppState,
    poll: &models::Poll,
    voter_id: i64,
    votes: &[(i64, i32)],
) -> anyhow::Result<()> {
    if votes.is_empty() {
        return Ok(());
    }

    let voter = sqlx::query!(
        "SELECT username, id_scheme FROM accounts WHERE id = $1 AND domain IS NULL",
        voter_id,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(voter) = voter else {
        return Ok(());
    };
    if !crate::federation::keypair::has_signing_key(state, voter_id)
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }

    let remote = sqlx::query!(
        r#"SELECT owner.uri AS owner_uri, owner.inbox_url, owner.shared_inbox_url,
                  s.uri AS "status_uri?"
           FROM accounts owner
           JOIN statuses s ON s.id = $1
           WHERE owner.id = $2 AND owner.domain IS NOT NULL"#,
        poll.status_id,
        poll.account_id,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(remote) = remote else {
        return Ok(());
    };
    let Some(status_uri) = remote.status_uri.filter(|s| !s.is_empty()) else {
        return Ok(());
    };

    let inbox = if !remote.shared_inbox_url.is_empty() {
        remote.shared_inbox_url
    } else {
        remote.inbox_url
    };
    if inbox.is_empty() {
        return Ok(());
    }

    let actor = crate::federation::tag::account_uri(
        &state.instance.domain,
        voter_id,
        voter.id_scheme,
        &voter.username,
    );
    let key_id = format!("{actor}#main-key");
    let owner_uri = remote.owner_uri;

    for (vote_id, choice) in votes {
        let Some(option_name) = poll.options.get(*choice as usize) else {
            continue;
        };
        let vote_uri = format!("{actor}#votes/{vote_id}");
        sqlx::query!(
            "UPDATE poll_votes SET uri = $2, updated_at = now() WHERE id = $1",
            vote_id,
            vote_uri,
        )
        .execute(&state.db)
        .await?;

        let activity = serde_json::json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{vote_uri}/activity"),
            "type": "Create",
            "actor": actor,
            "to": owner_uri,
            "object": {
                "id": vote_uri,
                "type": "Note",
                "name": option_name,
                "attributedTo": actor,
                "inReplyTo": status_uri,
                "to": owner_uri,
            }
        });

        crate::federation::delivery::deliver_to_inboxes(
            state,
            activity,
            vec![inbox.clone()],
            key_id.clone(),
        )
        .await?;
    }

    Ok(())
}

pub(crate) async fn federate_poll_update(state: &AppState, status_id: i64) -> anyhow::Result<()> {
    let status = sqlx::query!(
        r#"SELECT s.account_id, s.visibility, a.username, a.uri AS account_uri, a.id_scheme,
                  p.updated_at AS poll_updated_at
           FROM statuses s
           JOIN accounts a ON a.id = s.account_id
           JOIN polls p ON p.status_id = s.id
           WHERE s.id = $1
             AND s.deleted_at IS NULL
             AND s.reblog_of_id IS NULL
             AND a.domain IS NULL"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(status) = status else {
        return Ok(());
    };
    if !crate::federation::keypair::has_signing_key(state, status.account_id)
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }

    let Some(bundle) =
        crate::api::ap::note::build_note(state, &state.instance.domain, status_id).await?
    else {
        return Ok(());
    };
    let update_id = format!(
        "{}#updates/{}",
        bundle.note_uri,
        status.poll_updated_at.and_utc().timestamp(),
    );
    let activity = serde_json::json!({
        "@context": crate::api::ap::note::note_context(),
        "id": update_id,
        "type": "Update",
        "actor": bundle.actor_url,
        "to": bundle.to,
        "cc": bundle.cc,
        "object": bundle.note,
    });
    let actor_url = crate::federation::tag::account_uri(
        &state.instance.domain,
        status.account_id,
        status.id_scheme,
        &status.username,
    );
    let key_id = format!("{actor_url}#main-key");

    let mut inboxes: Vec<String> = sqlx::query!(
        r#"SELECT DISTINCT inbox FROM (
             SELECT CASE WHEN a.shared_inbox_url IS NOT NULL AND a.shared_inbox_url <> ''
                         THEN a.shared_inbox_url ELSE a.inbox_url END AS inbox
               FROM mentions m
               JOIN accounts a ON a.id = m.account_id
              WHERE m.status_id = $1 AND a.domain IS NOT NULL AND a.inbox_url <> ''
             UNION
             SELECT CASE WHEN a.shared_inbox_url IS NOT NULL AND a.shared_inbox_url <> ''
                         THEN a.shared_inbox_url ELSE a.inbox_url END AS inbox
               FROM statuses b
               JOIN accounts a ON a.id = b.account_id
              WHERE b.reblog_of_id = $1 AND b.deleted_at IS NULL AND a.domain IS NOT NULL AND a.inbox_url <> ''
             UNION
             SELECT CASE WHEN a.shared_inbox_url IS NOT NULL AND a.shared_inbox_url <> ''
                         THEN a.shared_inbox_url ELSE a.inbox_url END AS inbox
               FROM poll_votes pv
               JOIN polls p ON p.id = pv.poll_id
               JOIN accounts a ON a.id = pv.account_id
              WHERE p.status_id = $1 AND a.domain IS NOT NULL AND a.inbox_url <> ''
           ) recipients
           WHERE inbox IS NOT NULL AND inbox <> ''"#,
        status_id,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .filter_map(|r| r.inbox)
    .collect();

    if status.visibility != crate::db::models::vis::DIRECT {
        let follower_inboxes = sqlx::query!(
            r#"SELECT DISTINCT
                 CASE WHEN a.shared_inbox_url IS NOT NULL AND a.shared_inbox_url <> ''
                      THEN a.shared_inbox_url
                      ELSE a.inbox_url
                 END AS inbox
               FROM follows f
               JOIN accounts a ON a.id = f.account_id
               WHERE f.target_account_id = $1
                 AND a.domain IS NOT NULL
                 AND a.inbox_url <> ''"#,
            status.account_id,
        )
        .fetch_all(&state.db)
        .await?;
        inboxes.extend(follower_inboxes.into_iter().filter_map(|r| r.inbox));
    }

    // `Status#sign?` (`DistributePollUpdateWorker`).
    let signed = crate::federation::delivery::LinkedData::for_status(
        matches!(
            status.visibility,
            crate::db::models::vis::PUBLIC | crate::db::models::vis::UNLISTED
        ),
        crate::federation::delivery::LinkedData::UnlessAuthorizedFetch,
    );
    crate::federation::delivery::deliver_to_inboxes_signed(
        state, activity, inboxes, key_id, signed,
    )
    .await?;
    Ok(())
}

async fn poll_from_db(
    state: &AppState,
    poll: &models::Poll,
    viewer_id: Option<i64>,
) -> AppResult<Poll> {
    let option_titles: Vec<String> = poll.options.clone();

    let expired = poll
        .expires_at
        .map(|e| e < chrono::Utc::now().naive_utc())
        .unwrap_or(false);
    let viewer_is_owner = viewer_id.map(|vid| vid == poll.account_id).unwrap_or(false);

    let (voted, own_votes) = if let Some(vid) = viewer_id {
        let votes = sqlx::query!(
            "SELECT choice FROM poll_votes WHERE poll_id = $1 AND account_id = $2 ORDER BY choice",
            poll.id,
            vid,
        )
        .fetch_all(&state.db)
        .await?;
        let choices: Vec<i32> = votes.iter().map(|v| v.choice).collect();
        // `voted` and `own_votes` are both `if: :current_user?`, so they appear
        // together for any authenticated request — an empty list when nothing
        // was voted for, rather than nothing at all. The author counts as
        // having voted, matching `Poll#voted?`.
        (Some(viewer_is_owner || !choices.is_empty()), Some(choices))
    } else {
        (None, None)
    };

    // Per Mastodon show_totals_now?: expired? || !hide_totals?
    let show_results = expired || !poll.hide_totals;

    let per_option_counts = if show_results {
        sqlx::query!(
            "SELECT choice, COUNT(*) as cnt FROM poll_votes WHERE poll_id = $1 GROUP BY choice",
            poll.id,
        )
        .fetch_all(&state.db)
        .await?
    } else {
        vec![]
    };

    let options: Vec<PollOption> = option_titles
        .iter()
        .enumerate()
        .map(|(i, title)| {
            let votes_count = if show_results {
                let cnt = per_option_counts
                    .iter()
                    .find(|r| r.choice == i as i32)
                    .and_then(|r| r.cnt)
                    .unwrap_or(0);
                Some(cnt)
            } else {
                None
            };
            PollOption {
                title: title.clone(),
                votes_count,
            }
        })
        .collect();

    let voters_count = sqlx::query_scalar!(
        "SELECT COUNT(DISTINCT account_id) FROM poll_votes WHERE poll_id = $1",
        poll.id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    let votes_count = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM poll_votes WHERE poll_id = $1",
        poll.id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    Ok(Poll {
        id: poll.id.to_string(),
        expires_at: poll.expires_at.map(super::convert::mastodon_date),
        expired,
        multiple: poll.multiple,
        votes_count,
        voters_count: if poll.multiple {
            Some(voters_count)
        } else {
            None
        },
        options,
        emojis: vec![],
        voted,
        own_votes,
    })
}
