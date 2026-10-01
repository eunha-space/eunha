//! `AccountWarning`: a strike against an account, which the account is shown
//! and can appeal.

use serde_json::{json, Value};

use crate::state::AppState;

/// `AccountWarning#action`.
pub mod action {
    pub const NONE: i32 = 0;
    pub const DISABLE: i32 = 1_000;
    pub const MARK_STATUSES_AS_SENSITIVE: i32 = 1_250;
    pub const DELETE_STATUSES: i32 = 1_500;
    pub const SENSITIVE: i32 = 2_000;
    pub const SILENCE: i32 = 3_000;
    pub const SUSPEND: i32 = 4_000;

    pub fn to_str(v: i32) -> &'static str {
        match v {
            DISABLE => "disable",
            MARK_STATUSES_AS_SENSITIVE => "mark_statuses_as_sensitive",
            DELETE_STATUSES => "delete_statuses",
            SENSITIVE => "sensitive",
            SILENCE => "silence",
            SUSPEND => "suspend",
            _ => "none",
        }
    }

    pub fn parse(s: &str) -> Option<i32> {
        Some(match s {
            "none" => NONE,
            "disable" => DISABLE,
            "mark_statuses_as_sensitive" => MARK_STATUSES_AS_SENSITIVE,
            "delete_statuses" => DELETE_STATUSES,
            "sensitive" => SENSITIVE,
            "silence" => SILENCE,
            "suspend" => SUSPEND,
            _ => return None,
        })
    }
}

/// `AccountWarning::APPEAL_WINDOW`.
pub const APPEAL_WINDOW: chrono::TimeDelta = chrono::TimeDelta::days(20);

/// `Appeal` state as `REST::AppealSerializer` names it.
fn appeal_state(approved: bool, rejected: bool) -> &'static str {
    if approved {
        "approved"
    } else if rejected {
        "rejected"
    } else {
        "pending"
    }
}

/// `REST::AccountWarningSerializer`.
pub async fn serialize(state: &AppState, warning_id: i64) -> Option<Value> {
    let row = sqlx::query!(
        r#"SELECT w.id, w.action, w.text, w.status_ids, w.created_at, w.target_account_id,
                  ap.text AS "appeal_text?", ap.approved_at AS "appeal_approved_at?",
                  ap.rejected_at AS "appeal_rejected_at?", (ap.id IS NOT NULL) AS "has_appeal!"
           FROM account_warnings w
           LEFT JOIN appeals ap ON ap.account_warning_id = w.id
           WHERE w.id = $1"#,
        warning_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()?;
    let target_id = row.target_account_id?;
    let target = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        target_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()?;
    let target = crate::api::mastodon::accounts::account_to_api(state, &target).await;
    let appeal = row.has_appeal.then(|| {
        json!({
            "text": row.appeal_text.unwrap_or_default(),
            "state": appeal_state(row.appeal_approved_at.is_some(), row.appeal_rejected_at.is_some()),
        })
    });
    Some(json!({
        "id": row.id.to_string(),
        "action": action::to_str(row.action),
        "text": row.text,
        "status_ids": row.status_ids,
        "created_at": crate::api::mastodon::convert::mastodon_date(row.created_at),
        "target_account": target,
        "appeal": appeal,
    }))
}
