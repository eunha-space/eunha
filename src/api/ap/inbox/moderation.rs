//! Inbound moderation-related activities: `Block` (remote actor blocks a local
//! account), `Flag` (a remote report against local accounts/statuses), and
//! `Move` (an actor migrating to a new account).

use serde_json::Value;

use crate::{error::AppResult, state::AppState};

use super::{delete_arrived_first, resolve_or_fetch_remote_account};

pub(super) async fn handle_block(state: &AppState, activity: &Value) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let activity_uri = activity.get("id").and_then(|i| i.as_str()).unwrap_or("");

    // Skip a Block whose Undo already arrived out of order.
    if delete_arrived_first(state, actor_uri, activity_uri).await {
        return Ok(());
    }

    let object_uri = activity
        .get("object")
        .and_then(|o| {
            if o.is_string() {
                o.as_str()
            } else {
                o.get("id").and_then(|i| i.as_str())
            }
        })
        .unwrap_or("");

    // Only process if the blocked account is local
    let Some(target_id) = sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE uri = $1 AND domain IS NULL",
        object_uri
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };

    let blocker_id = resolve_or_fetch_remote_account(state, actor_uri).await?;

    sqlx::query!(
        "INSERT INTO blocks (account_id, target_account_id, created_at, updated_at) VALUES ($1,$2, now(), now()) ON CONFLICT DO NOTHING",
        blocker_id, target_id,
    ).execute(&state.db).await?;

    // Remove mutual follows
    let deleted = sqlx::query!(
        "DELETE FROM follows WHERE (account_id=$1 AND target_account_id=$2) OR (account_id=$2 AND target_account_id=$1) RETURNING account_id, target_account_id",
        blocker_id, target_id,
    ).fetch_all(&state.db).await?;
    for row in &deleted {
        let _ =
            crate::counters::on_follow_removed(&state.db, row.account_id, row.target_account_id)
                .await;
    }
    sqlx::query!(
        "DELETE FROM follow_requests WHERE (account_id=$1 AND target_account_id=$2) OR (account_id=$2 AND target_account_id=$1)",
        blocker_id, target_id,
    ).execute(&state.db).await?;
    // Mirror Mastodon's FollowRequest dependent: :destroy — clear the local
    // target's follow_request notification from the remote blocker.
    sqlx::query!(
        "DELETE FROM notifications WHERE account_id = $1 AND from_account_id = $2 AND type = 'follow_request'",
        target_id, blocker_id,
    ).execute(&state.db).await?;

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
    let object_uris: Vec<String> = match activity.get("object") {
        Some(Value::Array(items)) => items.iter().filter_map(value_or_id).collect(),
        Some(item) => value_or_id(item).into_iter().collect(),
        None => vec![],
    };

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
            crate::federation::moderation::domain_of(id)
                == reporter
                    .stored_uri()
                    .and_then(crate::federation::moderation::domain_of)
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

/// `value_or_id`.
fn value_or_id(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("id").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

pub(super) async fn handle_move(state: &AppState, activity: &Value) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let target_uri = activity
        .get("object")
        .and_then(|o| {
            if o.is_string() {
                o.as_str()
            } else {
                o.get("id").and_then(|i| i.as_str())
            }
        })
        .unwrap_or("");

    if actor_uri.is_empty() || target_uri.is_empty() {
        return Ok(());
    }

    // Fetch the new account to verify also_known_as contains the old actor URI
    let new_account_id = match resolve_or_fetch_remote_account(state, target_uri).await {
        Ok(id) => id,
        Err(_) => return Ok(()),
    };

    // Fetch the target actor to verify also_known_as
    let also_known_as: Vec<String> = sqlx::query_scalar!(
        "SELECT also_known_as FROM accounts WHERE id = $1",
        new_account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .flatten()
    .unwrap_or_default();

    if !also_known_as.iter().any(|u| u == actor_uri) {
        tracing::warn!(
            actor_uri,
            target_uri,
            "Move rejected: target alsoKnownAs does not include actor"
        );
        return Ok(());
    }

    // Set moved_to_account_id on the old account
    sqlx::query!(
        "UPDATE accounts SET moved_to_account_id = $1 WHERE uri = $2 AND domain IS NOT NULL",
        new_account_id,
        actor_uri,
    )
    .execute(&state.db)
    .await?;

    tracing::debug!(
        actor_uri,
        target_uri,
        "processed Move: updated moved_to_account_id"
    );
    Ok(())
}
