//! Inbound moderation-related activities: `Block` (remote actor blocks a local
//! account), `Flag` (a remote report against local accounts/statuses), and
//! `Move` (an actor migrating to a new account).

use ojak_vocab::json_ld_helper::value_or_id;
use serde_json::Value;

use crate::{error::AppResult, state::AppState};

use super::{delete_arrived_first, resolve_or_fetch_remote_account};

/// `ActivityPub::Activity::Block#perform`: the sender blocks a local
/// account, which ends the follows between them as `UnfollowService` ends
/// them, telling the sender, and withdraws the local account's request to
/// follow it.
pub(super) async fn handle_block(state: &AppState, activity: &Value) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let activity_uri = activity
        .get("id")
        .and_then(|i| i.as_str())
        .filter(|id| !id.is_empty());
    let object_uri = activity
        .get("object")
        .and_then(value_or_id)
        .filter(|uri| !uri.is_empty());

    // `account_from_uri(object_uri)`, when it is local. A local account
    // Mastodon made has no `uri` of its own, so it is found by its path.
    let Some(target_id) = (match object_uri {
        Some(uri) => crate::federation::local_uri::account(state, uri).await,
        None => None,
    }) else {
        return Ok(());
    };
    let target_is_local = sqlx::query_scalar!(
        r#"SELECT domain IS NULL AS "local!" FROM accounts WHERE id = $1"#,
        target_id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    if !target_is_local {
        return Ok(());
    }

    let blocker_id = resolve_or_fetch_remote_account(state, actor_uri).await?;

    // A block already made only takes the activity's id.
    if let Some(block_id) = sqlx::query_scalar!(
        "SELECT id FROM blocks WHERE account_id = $1 AND target_account_id = $2",
        blocker_id,
        target_id,
    )
    .fetch_optional(&state.db)
    .await?
    {
        if let Some(uri) = activity_uri {
            sqlx::query!(
                "UPDATE blocks SET uri = $2, updated_at = now() WHERE id = $1",
                block_id,
                uri,
            )
            .execute(&state.db)
            .await?;
        }
        return Ok(());
    }

    let follows = |a: i64, b: i64| async move {
        sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM follows
                              WHERE account_id = $1 AND target_account_id = $2) AS "e!""#,
            a,
            b,
        )
        .fetch_one(&state.db)
        .await
    };
    if follows(blocker_id, target_id).await? {
        crate::api::mastodon::accounts::unfollow(state, blocker_id, target_id, false).await?;
    }
    if follows(target_id, blocker_id).await? {
        crate::api::mastodon::accounts::unfollow(state, target_id, blocker_id, false).await?;
    }
    // `RejectFollowService.new.call(target_account, @account) if
    // target_account.requested?(@account)`.
    crate::api::mastodon::accounts::reject_follow(state, target_id, blocker_id).await?;

    // Skip a Block whose Undo already arrived out of order.
    if !delete_arrived_first(state, actor_uri, activity_uri.unwrap_or("")).await {
        crate::api::mastodon::accounts::queue_block_worker(state, blocker_id, target_id).await;
        // `@account.block!(target_account, uri: @json['id'])`, whose
        // `set_uri` names a block that came without an id.
        let uri = activity_uri.map(str::to_owned).unwrap_or_else(|| {
            crate::federation::relationships::generate_uri(&state.instance.domain)
        });
        sqlx::query!(
            r#"INSERT INTO blocks (account_id, target_account_id, uri, created_at, updated_at)
               VALUES ($1, $2, $3, now(), now()) ON CONFLICT DO NOTHING"#,
            blocker_id,
            target_id,
            uri,
        )
        .execute(&state.db)
        .await?;
    }

    Ok(())
}

/// `ActivityPub::Activity::Flag`'s `COMMENT_SIZE_LIMIT`.
const FLAG_COMMENT_SIZE_LIMIT: usize = 5000;

/// `ActivityPub::Activity::Flag#perform`: a remote server reporting accounts
/// here, or remote accounts that replied to accounts here.
pub(super) async fn handle_flag(state: &AppState, activity: &Value) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let reporter_id = match resolve_or_fetch_remote_account(state, actor_uri).await {
        Ok(id) => id,
        Err(_) => return Ok(()),
    };
    let Some(reporter) = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        reporter_id
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };

    // `skip_reports?`
    if let Some(domain) = reporter.domain.as_deref() {
        if crate::federation::moderation::lookup(state, domain)
            .await
            .is_some_and(|block| block.reject_reports)
        {
            return Ok(());
        }
    }

    // `object_uris`: a string, an object with an id, or an array of either.
    let object_uris: Vec<String> = ojak_vocab::json_ld_helper::ids(activity.get("object"))
        .into_iter()
        .map(str::to_owned)
        .collect();

    let mut target_accounts: Vec<i64> = vec![];
    let mut statuses: Vec<i64> = vec![];
    let mut collections: Vec<i64> = vec![];
    for uri in &object_uris {
        if let Some(id) = crate::federation::local_uri::account(state, uri).await {
            target_accounts.push(id);
        }
        if let Some(id) = crate::federation::local_uri::status(state, uri).await {
            statuses.push(id);
        }
        if let Some(id) = crate::federation::local_uri::collection(state, uri).await {
            collections.push(id);
        }
    }

    // `report_uri`: the Flag's id, unless it is on another host than its actor.
    let report_uri = activity
        .get("id")
        .and_then(|i| i.as_str())
        .filter(|id| {
            ojak::origin::host_of(id) == reporter.stored_uri().and_then(ojak::origin::host_of)
        })
        .map(str::to_owned);
    // `report_comment`
    let comment: String = activity
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .chars()
        .take(FLAG_COMMENT_SIZE_LIMIT)
        .collect();

    for target_id in target_accounts {
        let Some(target) = sqlx::query_as!(
            crate::db::models::Account,
            "SELECT * FROM accounts WHERE id = $1",
            target_id
        )
        .fetch_optional(&state.db)
        .await?
        else {
            continue;
        };
        // `target_statuses_by_account[target_account.id]`
        let target_statuses: Vec<i64> = sqlx::query_scalar!(
            "SELECT id FROM statuses WHERE id = ANY($1) AND account_id = $2 ORDER BY id",
            &statuses,
            target.id,
        )
        .fetch_all(&state.db)
        .await?;
        let target_collections: Vec<i64> = sqlx::query_scalar!(
            "SELECT id FROM collections WHERE id = ANY($1) AND account_id = $2 ORDER BY id",
            &collections,
            target.id,
        )
        .fetch_all(&state.db)
        .await?;
        // `replied_to_accounts`: local accounts the reported posts reply to.
        let replied_to_local = sqlx::query_scalar!(
            r#"SELECT EXISTS (
                 SELECT 1 FROM statuses s JOIN accounts a ON a.id = s.in_reply_to_account_id
                 WHERE s.id = ANY($1) AND a.domain IS NULL
               ) AS "e!""#,
            &target_statuses,
        )
        .fetch_one(&state.db)
        .await?;
        // A suspended or deleted target is skipped, and so is a remote one that
        // did not reply to anyone here.
        if target.suspended_at.is_some()
            || target.is_deleted()
            || (!target.is_local() && !replied_to_local)
        {
            continue;
        }
        if let Err(error) = crate::moderation::report_service::call(
            state,
            &reporter,
            &target,
            crate::moderation::report_service::Options {
                status_ids: target_statuses,
                collection_ids: target_collections,
                comment: comment.clone(),
                uri: report_uri.clone(),
                ..Default::default()
            },
        )
        .await
        {
            tracing::debug!(?error, "could not record a remote report");
        }
    }

    Ok(())
}

/// `ActivityPub::Activity::Move::PROCESSING_COOLDOWN`: how long one Move
/// from an account keeps others from it out.
const MOVE_PROCESSING_COOLDOWN_SECS: u64 = 7 * 24 * 60 * 60;

fn move_in_progress_key(state: &AppState, account_id: i64) -> String {
    state
        .redis_keys
        .key(format!("move_in_progress:{account_id}"))
}

/// `ActivityPub::Activity::Move`: the sender says it has moved to `target`.
/// Believed when the Move is about the sender itself and the target, fetched
/// fresh, is available and names the sender in `alsoKnownAs`; then the
/// sender redirects there and its local followers, notes, blocks and mutes
/// follow it (`MoveWorker`). One Move per account per week is processed.
pub(super) async fn handle_move(state: &AppState, activity: &Value) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    if actor_uri.is_empty() {
        return Ok(());
    }
    // `return if origin_account.uri != object_uri`.
    let object_uri = activity.get("object").and_then(value_or_id);
    if object_uri != Some(actor_uri) {
        return Ok(());
    }
    let origin_id = resolve_or_fetch_remote_account(state, actor_uri).await?;

    // `mark_as_processing!`.
    let key = move_in_progress_key(state, origin_id);
    let mut redis = state.redis.clone();
    let marked: Option<String> = redis::cmd("SET")
        .arg(&key)
        .arg("true")
        .arg("NX")
        .arg("EX")
        .arg(MOVE_PROCESSING_COOLDOWN_SECS)
        .query_async(&mut redis)
        .await
        .map_err(|e| crate::error::AppError::Internal(e.into()))?;
    if marked.is_none() {
        tracing::debug!(actor_uri, "Move ignored: another is being processed");
        return Ok(());
    }

    let result = process_move(state, activity, actor_uri, origin_id).await;
    if !matches!(result, Ok(true)) {
        // `unmark_as_processing!`, on refusal and on error alike.
        let _: redis::RedisResult<i64> = redis::cmd("DEL").arg(&key).query_async(&mut redis).await;
    }
    result.map(|_| ())
}

/// The part of [`handle_move`] after the lock: true when the Move was
/// accepted.
async fn process_move(
    state: &AppState,
    activity: &Value,
    origin_uri: &str,
    origin_id: i64,
) -> AppResult<bool> {
    let Some(target_uri) = activity.get("target").and_then(value_or_id) else {
        return Ok(false);
    };
    // `ActivityPub::FetchRemoteAccountService`, which returns nil when the
    // target cannot be fetched.
    let Ok(target_id) = super::fetch_remote_account(state, target_uri).await else {
        return Ok(false);
    };
    let Some(target) = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        target_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(false);
    };
    let also_known_as = target.also_known_as.clone().unwrap_or_default();
    if target.is_unavailable() || !also_known_as.iter().any(|uri| uri == origin_uri) {
        tracing::info!(
            origin_uri,
            target_uri,
            "Move refused: the target is unavailable or does not name the origin"
        );
        return Ok(false);
    }

    // "In case for some reason we didn't have a redirect for the profile
    // already, set it."
    sqlx::query!(
        "UPDATE accounts SET moved_to_account_id = $1, updated_at = now() WHERE id = $2",
        target_id,
        origin_id,
    )
    .execute(&state.db)
    .await?;

    crate::moves::queue_move_worker(state, origin_id, target_id).await;
    Ok(true)
}
