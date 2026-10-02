//! The model callbacks that tell providers what changed:
//! `Status::FaspConcern`, `Account::FaspConcern` and
//! `Favourite::FaspConcern`. Each does nothing unless the `fasp` feature is
//! on, decides from the row whether there is anything to announce, and
//! leaves the announcing to a worker.
//!
//! A status is announced when its author is indexable and it is public,
//! boosts included; an account when it is discoverable, or when an update
//! changed whether it is. A boost, a reply or a favourite also makes the
//! status it is about a trend candidate. A deletion is announced by calling
//! the hook before the row goes, or after a status is only marked deleted.

use super::workers::{self, TrendSource};
use crate::state::AppState;

/// What a status hook reads off the row.
struct StatusFacts {
    indexable: bool,
    public: bool,
    reblog_of_id: Option<i64>,
    in_reply_to_id: Option<i64>,
}

async fn status_facts(state: &AppState, status_id: i64) -> Option<StatusFacts> {
    let row = sqlx::query!(
        r#"SELECT a.indexable, s.visibility, s.reblog_of_id, s.in_reply_to_id
             FROM statuses s JOIN accounts a ON a.id = s.account_id
            WHERE s.id = $1"#,
        status_id
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|error| tracing::warn!(%error, status_id, "could not read a status for FASP"))
    .ok()??;
    Some(StatusFacts {
        indexable: row.indexable,
        public: row.visibility == crate::db::models::vis::PUBLIC,
        reblog_of_id: row.reblog_of_id,
        in_reply_to_id: row.in_reply_to_id,
    })
}

async fn announce_status(
    state: &AppState,
    status_id: i64,
    event_type: &'static str,
) -> Option<StatusFacts> {
    let facts = status_facts(state, status_id).await?;
    if facts.indexable && facts.public {
        if let Ok(Some(uri)) = super::status_uri(state, status_id).await {
            workers::announce_content_lifecycle_event(state, uri, event_type).await;
        }
    }
    Some(facts)
}

/// `after_commit :announce_new_content_to_subscribed_fasp, on: :create` and
/// `:announce_trends_to_subscribed_fasp`.
pub async fn status_created(state: &AppState, status_id: i64) {
    if !super::enabled(state) {
        return;
    }
    let Some(facts) = announce_status(state, status_id, "new").await else {
        return;
    };
    if !facts.indexable {
        return;
    }
    if let Some(original) = facts.reblog_of_id {
        workers::announce_trend(state, original, TrendSource::Reblog).await;
    } else if let Some(parent) = facts.in_reply_to_id {
        workers::announce_trend(state, parent, TrendSource::Reply).await;
    }
}

/// `after_commit :announce_updated_content_to_subscribed_fasp, on: :update`.
pub async fn status_updated(state: &AppState, status_id: i64) {
    if !super::enabled(state) {
        return;
    }
    announce_status(state, status_id, "update").await;
}

/// `after_commit :announce_deleted_content_to_subscribed_fasp, on: :destroy`.
pub async fn status_deleted(state: &AppState, status_id: i64) {
    if !super::enabled(state) {
        return;
    }
    announce_status(state, status_id, "delete").await;
}

/// `Favourite::FaspConcern`: every favourite makes its status a candidate.
pub async fn favourite_created(state: &AppState, status_id: i64) {
    if !super::enabled(state) {
        return;
    }
    workers::announce_trend(state, status_id, TrendSource::Favourite).await;
}

async fn announce_account(
    state: &AppState,
    account_id: i64,
    event_type: &'static str,
    discoverable_changed: bool,
) {
    let discoverable = sqlx::query_scalar!(
        "SELECT discoverable FROM accounts WHERE id = $1",
        account_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .flatten()
    .unwrap_or(false);
    if !(discoverable || discoverable_changed) {
        return;
    }
    if let Ok(Some(uri)) = super::account_uri(state, account_id).await {
        workers::announce_account_lifecycle_event(state, uri, event_type).await;
    }
}

/// `after_commit :announce_new_account_to_subscribed_fasp, on: :create`.
pub async fn account_created(state: &AppState, account_id: i64) {
    if !super::enabled(state) {
        return;
    }
    announce_account(state, account_id, "new", false).await;
}

/// `after_commit :announce_updated_account_to_subscribed_fasp, on: :update`:
/// announced while the account is discoverable, or when this update changed
/// whether it is.
pub async fn account_updated(state: &AppState, account_id: i64, discoverable_changed: bool) {
    if !super::enabled(state) {
        return;
    }
    announce_account(state, account_id, "update", discoverable_changed).await;
}

/// `after_commit :announce_deleted_account_to_subscribed_fasp, on: :destroy`.
pub async fn account_deleted(state: &AppState, account_id: i64) {
    if !super::enabled(state) {
        return;
    }
    announce_account(state, account_id, "delete", false).await;
}
