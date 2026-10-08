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
    let viewer = auth.as_ref().map(|Extension(a)| a.account_id);
    let mut poll = fetch_shown_poll(&state, id, viewer).await?;
    // `refresh_poll`: `FetchRemotePollService` for a signed-in user when the
    // poll is `possibly_stale?`.
    let signed_in = auth
        .as_ref()
        .is_some_and(|Extension(a)| a.user_id.is_some());
    if signed_in && possibly_stale(&state, &poll).await? {
        let viewer = auth.as_ref().map(|Extension(a)| a.account_id);
        if let Err(error) =
            crate::api::ap::inbox::fetch_remote_poll(&state, poll.status_id, viewer).await
        {
            // `rescue_from(*Mastodon::HTTP_CONNECTION_ERRORS)`.
            if crate::api::ap::inbox::unanswered(&error) {
                return Err(AppError::ServiceUnavailable(
                    "Remote data could not be fetched".into(),
                ));
            }
            return Err(error);
        }
        poll = fetch_poll(&state, id).await?;
    }
    let viewer_id = auth.map(|Extension(a)| a.account_id);
    poll_from_db(&state, &poll, viewer_id).await.map(Json)
}

/// `Poll::MAKE_FETCH_HAPPEN`.
const MAKE_FETCH_HAPPEN: chrono::Duration = chrono::Duration::minutes(1);

/// `Poll#possibly_stale?`: a remote poll not fetched since it closed, nor in
/// the last minute.
async fn possibly_stale(state: &AppState, poll: &models::Poll) -> AppResult<bool> {
    let remote = sqlx::query_scalar!(
        r#"SELECT (domain IS NOT NULL) AS "remote!" FROM accounts WHERE id = $1"#,
        poll.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    let last_fetched_before_expiration = match (poll.last_fetched_at, poll.expires_at) {
        (Some(fetched), Some(expires)) => fetched < expires,
        _ => true,
    };
    let time_passed_since_last_fetch = poll
        .last_fetched_at
        .is_none_or(|fetched| fetched < chrono::Utc::now().naive_utc() - MAKE_FETCH_HAPPEN);
    Ok(remote && last_fetched_before_expiration && time_passed_since_last_fetch)
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
    super::extractors::Params(form): super::extractors::Params<PollVoteForm>,
) -> AppResult<Json<Poll>> {
    auth.require_scope("write:statuses")?;
    let poll = fetch_shown_poll(&state, id, Some(auth.account_id)).await?;
    // `PollPolicy#vote?`: neither the voter nor the poll's author blocking
    // the other.
    let blocked = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM blocks
             WHERE (account_id = $1 AND target_account_id = $2)
                OR (account_id = $2 AND target_account_id = $1)
           ) AS "e!""#,
        auth.account_id,
        poll.account_id,
    )
    .fetch_one(&state.db)
    .await?;
    if blocked {
        return Err(AppError::Forbidden);
    }

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

    if !poll.multiple && form.choices.len() > 1 {
        return Err(AppError::Unprocessable(
            "Multiple choices not allowed".into(),
        ));
    }

    // `VoteService`: the votes made under `with_redis_lock("vote:<poll>:<account>")`,
    // which, held by another, raises `RaceConditionError`.
    let lock = crate::redis_lock::try_acquire_lockable(
        &state,
        &format!("vote:{id}:{}", auth.account_id),
        crate::redis_lock::DEFAULT_TTL_MS,
    )
    .await
    .ok_or_else(|| {
        AppError::ServiceUnavailable(
            "There was a temporary problem serving your request, please try again".into(),
        )
    })?;
    let created_votes = Box::pin(cast_votes(&state, &poll, auth.account_id, &form.choices)).await;
    lock.release().await;
    let created_votes = created_votes?;

    // `VoteService`: `ActivityTracker.increment('activity:interactions')`.
    if !form.choices.is_empty() {
        crate::activity_tracker::increment(&state, crate::activity_tracker::INTERACTIONS).await;
    }

    // `if @poll.account.local?` `distribute_poll!`, else `deliver_votes!`.
    let poll_is_local = sqlx::query_scalar!(
        r#"SELECT (domain IS NULL) AS "local!" FROM accounts WHERE id = $1"#,
        poll.account_id,
    )
    .fetch_one(&state.db)
    .await?;
    if poll_is_local {
        // `distribute_poll!`: `return if @poll.hide_totals?`, then
        // `DistributePollUpdateWorker.perform_in(3.minutes, …)`.
        if !poll.hide_totals {
            crate::jobs::push_in(
                &state,
                std::time::Duration::from_secs(3 * 60),
                crate::api::ap::inbox::create::DistributePollUpdateWorker {
                    status_id: poll.status_id,
                },
            )
            .await;
        }
    } else {
        if let Err(e) = federate_poll_votes(&state, &poll, auth.account_id, &created_votes).await {
            tracing::warn!(poll_id = id, error = %e, "failed to enqueue ActivityPub poll vote");
        }
        // `queue_final_poll_check!`: `PollExpirationNotifyWorker
        // .perform_at(@poll.expires_at + 5.minutes, @poll.id) if @poll.expires?`.
        if let Some(expires_at) = poll.expires_at {
            notify_expiration_at(&state, poll.id, expires_at + chrono::Duration::minutes(5)).await;
        }
    }

    let poll = fetch_poll(&state, id).await?;
    poll_from_db(&state, &poll, Some(auth.account_id))
        .await
        .map(Json)
}

/// What `VoteService` does under its lock: whether the account voted
/// already, then a vote for each choice. The votes made, by id and choice.
async fn cast_votes(
    state: &AppState,
    poll: &models::Poll,
    account_id: i64,
    choices: &[i32],
) -> AppResult<Vec<(i64, i32)>> {
    let id = poll.id;
    let option_count = poll.options.len() as i32;
    // Single-choice: block re-voting entirely. Multi-choice: only block same choice (ON CONFLICT).
    if !poll.multiple {
        let already_voted = sqlx::query_scalar!(
            "SELECT EXISTS(SELECT 1 FROM poll_votes WHERE poll_id = $1 AND account_id = $2)",
            id,
            account_id,
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
        account_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(true);

    let mut created_votes: Vec<(i64, i32)> = Vec::new();
    let mut new_voter = was_first_vote;
    for choice in choices {
        if *choice < 0 || *choice >= option_count {
            return Err(AppError::Unprocessable("Invalid choice index".into()));
        }
        if let Some(vote) = sqlx::query!(
            r#"INSERT INTO poll_votes (poll_id, account_id, choice, created_at, updated_at)
               VALUES ($1, $2, $3, now(), now())
               ON CONFLICT DO NOTHING
               RETURNING id, choice"#,
            id,
            account_id,
            choice,
        )
        .fetch_optional(&state.db)
        .await?
        {
            count_vote(&state.db, id, vote.choice, std::mem::take(&mut new_voter)).await?;
            created_votes.push((vote.id, vote.choice));
        }
    }

    Ok(created_votes)
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// `set_poll`: `Poll.find` and `authorize @poll.status, :show?`, either
/// failing as a 404.
async fn fetch_shown_poll(
    state: &AppState,
    id: i64,
    viewer: Option<i64>,
) -> AppResult<models::Poll> {
    let poll = fetch_poll(state, id).await?;
    let status = sqlx::query_as!(
        models::Status,
        "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        poll.status_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    match viewer {
        Some(viewer) => super::statuses::check_status_visible(state, &status, viewer).await?,
        None => super::statuses::check_status_public(state, &status).await?,
    }
    Ok(poll)
}

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
        r#"SELECT owner.uri AS owner_uri, owner.inbox_url,
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

    // `@poll.account.inbox_url`, its own inbox.
    let inbox = remote.inbox_url;
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
        // `VoteSerializer`: a local vote has no `uri` of its own, and is named
        // under its voter.
        let vote_uri = format!("{actor}#votes/{vote_id}");

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

/// `REST::PollSerializer` for `polls`, by poll id: the tallies Mastodon keeps
/// (`cached_tallies`, `votes_count`, `voters_count`) — a remote poll's as its
/// server reported them, a local one's as each vote raised them — shown when
/// `show_totals_now?`, the custom emojis its options use, and, for a viewer,
/// `voted` and `own_votes`.
pub(crate) async fn serialize_many(
    state: &AppState,
    polls: &[models::Poll],
    viewer_id: Option<i64>,
) -> AppResult<std::collections::HashMap<i64, Poll>> {
    use std::collections::HashMap;

    if polls.is_empty() {
        return Ok(HashMap::new());
    }
    let poll_ids: Vec<i64> = polls.iter().map(|p| p.id).collect();
    let mut own: HashMap<i64, Vec<i32>> = HashMap::new();
    if let Some(viewer) = viewer_id {
        // `votes.where(account:).pluck(:choice)`.
        for vote in sqlx::query!(
            "SELECT poll_id, choice FROM poll_votes
             WHERE poll_id = ANY($1::bigint[]) AND account_id = $2 ORDER BY id",
            &poll_ids,
            viewer,
        )
        .fetch_all(&state.db)
        .await?
        {
            own.entry(vote.poll_id).or_default().push(vote.choice);
        }
    }
    // `CustomEmoji.from_text(options.join(' '), account.domain)`.
    let account_ids: Vec<i64> = polls.iter().map(|p| p.account_id).collect();
    let domains: HashMap<i64, Option<String>> = sqlx::query!(
        "SELECT id, domain FROM accounts WHERE id = ANY($1)",
        &account_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| (r.id, r.domain))
    .collect();
    let texts: Vec<(i64, Option<String>, String)> = polls
        .iter()
        .map(|p| {
            (
                p.id,
                domains.get(&p.account_id).cloned().flatten(),
                p.options.join(" "),
            )
        })
        .collect();
    let mut emojis = super::convert::emojis_from_texts(state, &texts).await;

    let now = chrono::Utc::now().naive_utc();
    Ok(polls
        .iter()
        .map(|poll| {
            // `Expireable#expired?`.
            let expired = poll.expires_at.is_some_and(|at| at < now);
            let show_totals_now = expired || !poll.hide_totals;
            let options = poll
                .options
                .iter()
                .enumerate()
                .map(|(i, title)| PollOption {
                    title: title.clone(),
                    votes_count: show_totals_now
                        .then(|| poll.cached_tallies.get(i).copied().unwrap_or(0)),
                })
                .collect();
            let (voted, own_votes) = match viewer_id {
                Some(viewer) => {
                    let choices = own.remove(&poll.id).unwrap_or_default();
                    // `Poll#voted?`: the author counts as having voted.
                    (
                        Some(viewer == poll.account_id || !choices.is_empty()),
                        Some(choices),
                    )
                }
                None => (None, None),
            };
            (
                poll.id,
                Poll {
                    id: poll.id.to_string(),
                    expires_at: poll.expires_at.map(super::convert::mastodon_date),
                    expired,
                    multiple: poll.multiple,
                    votes_count: poll.votes_count,
                    voters_count: poll.voters_count,
                    options,
                    emojis: emojis.remove(&poll.id).unwrap_or_default(),
                    voted,
                    own_votes,
                },
            )
        })
        .collect())
}

/// `PollVote#increment_counter_cache` and `Poll#prepare_votes_count`: one
/// more for `choice`, and the total their sum; with `new_voter`,
/// `increment_voters_count!` too, unless the poll does not count its voters.
pub(crate) async fn count_vote(
    db: &sqlx::PgPool,
    poll_id: i64,
    choice: i32,
    new_voter: bool,
) -> sqlx::Result<()> {
    sqlx::query!(
        r#"UPDATE polls
           SET cached_tallies = tallied.tallies,
               votes_count = (SELECT COALESCE(SUM(t), 0)::bigint FROM unnest(tallied.tallies) t),
               voters_count = CASE WHEN $3 THEN voters_count + 1 ELSE voters_count END,
               lock_version = lock_version + 1,
               updated_at = now()
           FROM (
               SELECT p.id, ARRAY(
                   SELECT COALESCE(p.cached_tallies[i], 0)
                          + CASE WHEN i = $2 + 1 THEN 1 ELSE 0 END
                   FROM generate_series(
                       1, GREATEST(COALESCE(array_length(p.cached_tallies, 1), 0), $2 + 1)
                   ) AS i
                   ORDER BY i
               )::bigint[] AS tallies
               FROM polls p WHERE p.id = $1
           ) AS tallied
           WHERE polls.id = tallied.id"#,
        poll_id,
        choice,
        new_voter,
    )
    .execute(db)
    .await?;
    Ok(())
}

async fn poll_from_db(
    state: &AppState,
    poll: &models::Poll,
    viewer_id: Option<i64>,
) -> AppResult<Poll> {
    serialize_many(state, std::slice::from_ref(poll), viewer_id)
        .await?
        .remove(&poll.id)
        .ok_or(AppError::NotFound)
}

// ── PollExpirationNotifyWorker ────────────────────────────────────────────

/// `PollExpirationNotifyWorker`: once a poll has ended, its tallies are sent
/// out and its author told when it is local, and the local accounts that
/// voted in it are told. Queued for when a local poll ends
/// (`PostStatusService`), five minutes after a local poll edited to end
/// (`UpdateStatusService`), and five minutes after a remote one ends when a
/// local account votes in it (`VoteService`) or an update of one with votes
/// sets when it ends (`ProcessStatusUpdateService`). Run early, it puts
/// itself back until five minutes after the poll ends.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PollExpirationNotifyWorker {
    pub poll_id: i64,
}

impl crate::jobs::Job for PollExpirationNotifyWorker {
    const KIND: &'static str = "PollExpirationNotifyWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT.lock(
        crate::jobs::Lock::UntilExecuting(crate::jobs::DEFAULT_LOCK_TTL),
    );

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        // `Poll.find`, `RecordNotFound` rescued.
        let Some(poll) = sqlx::query!(
            r#"SELECT p.id, p.status_id, p.account_id, p.expires_at,
                      (a.domain IS NULL) AS "local!"
               FROM polls p JOIN accounts a ON a.id = p.account_id
               WHERE p.id = $1"#,
            self.poll_id,
        )
        .fetch_optional(&state.db)
        .await?
        else {
            return Ok(());
        };
        // `missing_expiration?`.
        let Some(expires_at) = poll.expires_at else {
            return Ok(());
        };
        // `requeue! && return if not_due_yet?`.
        let expires_at = expires_at.and_utc();
        if expires_at > chrono::Utc::now() {
            crate::jobs::requeue_at(
                state,
                expires_at + chrono::Duration::minutes(5),
                PollExpirationNotifyWorker { poll_id: poll.id },
            )
            .await?;
            return Ok(());
        }
        let status_id = poll.status_id;
        let author = poll.account_id;
        let mut recipients = Vec::new();
        // `notify_remote_voters_and_owner! if @poll.local?`.
        if poll.local {
            crate::jobs::push(
                state,
                crate::api::ap::inbox::create::DistributePollUpdateWorker { status_id },
            )
            .await;
            recipients.push(author);
        }
        // `notify_local_voters!`: `@poll.voters.merge(Account.local)`.
        recipients.extend(
            sqlx::query_scalar!(
                r#"SELECT DISTINCT v.account_id FROM poll_votes v
                   JOIN accounts a ON a.id = v.account_id
                   WHERE v.poll_id = $1 AND a.domain IS NULL
                   ORDER BY v.account_id"#,
                poll.id,
            )
            .fetch_all(&state.db)
            .await?,
        );
        for recipient in recipients {
            crate::push::create_and_push(state, recipient, author, "poll", Some(status_id)).await;
        }
        Ok(())
    }
}

/// `PollExpirationNotifyWorker.perform_at(at, poll.id)`.
pub(crate) async fn notify_expiration_at(
    state: &AppState,
    poll_id: i64,
    at: chrono::NaiveDateTime,
) {
    if let Err(error) =
        crate::jobs::perform_at(state, at.and_utc(), PollExpirationNotifyWorker { poll_id }).await
    {
        tracing::error!(poll_id, %error, "could not queue a poll's expiration notice");
    }
}

/// `queue_poll_notifications!`, as `UpdateStatusService` and
/// `ProcessStatusUpdateService` have it: a notice five minutes after the
/// poll now ends, the one queued for a later end taken back. A remote poll
/// is noticed only when someone voted in it, and not again once it had
/// already ended.
pub(crate) async fn queue_poll_notifications(
    state: &AppState,
    status_id: i64,
    previous_expires_at: Option<chrono::NaiveDateTime>,
    remote: bool,
) -> anyhow::Result<()> {
    let Some(poll) = sqlx::query!(
        r#"SELECT p.id, p.expires_at,
                  EXISTS (SELECT 1 FROM poll_votes v WHERE v.poll_id = p.id) AS "voted!"
           FROM polls p WHERE p.status_id = $1"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let Some(expires_at) = poll.expires_at else {
        return Ok(());
    };
    if remote {
        if !poll.voted {
            return Ok(());
        }
        // `return if @previous_expires_at&.past?`.
        if previous_expires_at.is_some_and(|previous| previous < chrono::Utc::now().naive_utc()) {
            return Ok(());
        }
    }
    let job = PollExpirationNotifyWorker { poll_id: poll.id };
    if previous_expires_at.is_some_and(|previous| previous > expires_at) {
        crate::jobs::remove_scheduled(state, &job).await?;
    }
    crate::jobs::perform_at(
        state,
        (expires_at + chrono::Duration::minutes(5)).and_utc(),
        job,
    )
    .await?;
    Ok(())
}
