//! `Disputes::StrikesController` and `Disputes::AppealsController`: an account
//! reading the strikes against it and appealing one, and staff reading any
//! strike (`Admin::Disputes::StrikesController` is the same controller).
//!
//! Mastodon has these as server-rendered pages; eunha serves them over REST
//! for its web client.

use axum::{
    extract::{Extension, Path},
    Json,
};
use serde::{Deserialize, Serialize};

use super::extractors::Params;
use super::types::{Account as ApiAccount, Status};
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::{
        appeal,
        role::{self, authorize, flag, Role},
        warning::{action, APPEAL_WINDOW},
    },
    state::AppState,
};

/// `REST::AppealSerializer`, with the dates the strike page shows.
#[derive(Debug, Serialize)]
pub struct AppealEntity {
    pub id: String,
    pub text: String,
    /// `pending`, `approved` or `rejected`.
    pub state: &'static str,
    pub created_at: String,
    pub approved_at: Option<String>,
    pub rejected_at: Option<String>,
}

/// `REST::AccountWarningSerializer`, with what the strike page shows besides:
/// whether it was overruled, until when it may be appealed, the posts it
/// cites, and for staff who issued it and the report it came from.
#[derive(Debug, Serialize)]
pub struct Strike {
    pub id: String,
    pub action: &'static str,
    pub text: String,
    pub status_ids: Option<Vec<String>>,
    pub created_at: String,
    pub target_account: Option<ApiAccount>,
    pub appeal: Option<AppealEntity>,
    pub overruled_at: Option<String>,
    /// `AccountWarning#appeal_eligible?`.
    pub appeal_eligible: bool,
    /// When `APPEAL_WINDOW` closes.
    pub appeal_deadline: String,
    /// `AccountWarningPolicy#appeal?` for the viewer.
    pub can_appeal: bool,
    pub statuses: Vec<Status>,
    /// The moderator who issued it; staff only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<ApiAccount>,
    /// The report it came from, when the viewer may see reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_id: Option<String>,
}

struct StrikeRow {
    id: i64,
    account_id: Option<i64>,
    target_account_id: Option<i64>,
    report_id: Option<i64>,
    action: i32,
    text: String,
    status_ids: Option<Vec<String>>,
    overruled_at: Option<chrono::NaiveDateTime>,
    created_at: chrono::NaiveDateTime,
}

async fn find(state: &AppState, id: i64) -> AppResult<StrikeRow> {
    sqlx::query_as!(
        StrikeRow,
        r#"SELECT id, account_id, target_account_id, report_id, action, text, status_ids,
                  overruled_at, created_at
           FROM account_warnings WHERE id = $1"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

/// One strike as `viewer` (the account, holding `role`) sees it.
pub(crate) async fn strike_entity(
    state: &AppState,
    id: i64,
    viewer: i64,
    role: &Role,
) -> AppResult<Strike> {
    let row = find(state, id).await?;
    build(state, row, viewer, role).await
}

async fn build(state: &AppState, row: StrikeRow, viewer: i64, role: &Role) -> AppResult<Strike> {
    use super::convert::mastodon_date;
    let appeal = sqlx::query!(
        r#"SELECT id, text, approved_at, rejected_at, created_at FROM appeals
           WHERE account_warning_id = $1"#,
        row.id
    )
    .fetch_optional(&state.db)
    .await?
    .map(|a| AppealEntity {
        id: a.id.to_string(),
        text: a.text,
        state: if a.approved_at.is_some() {
            "approved"
        } else if a.rejected_at.is_some() {
            "rejected"
        } else {
            "pending"
        },
        created_at: mastodon_date(a.created_at),
        approved_at: a.approved_at.map(mastodon_date),
        rejected_at: a.rejected_at.map(mastodon_date),
    });
    let eligible = appeal::appeal_eligible(row.created_at);
    let target_account = match row.target_account_id {
        Some(id) => super::admin::api_account(state, id).await?,
        None => None,
    };
    let status_ids: Vec<i64> = row
        .status_ids
        .iter()
        .flatten()
        .filter_map(|id| id.parse().ok())
        .collect();
    let staff = role.can(&[flag::MANAGE_APPEALS]);
    Ok(Strike {
        id: row.id.to_string(),
        action: action::to_str(row.action),
        text: row.text,
        status_ids: row.status_ids,
        created_at: mastodon_date(row.created_at),
        target_account,
        can_appeal: row.target_account_id == Some(viewer) && eligible && appeal.is_none(),
        appeal,
        overruled_at: row.overruled_at.map(mastodon_date),
        appeal_eligible: eligible,
        appeal_deadline: mastodon_date(row.created_at + APPEAL_WINDOW),
        // `Status.with_discarded.where(id: status_ids)`.
        statuses: super::admin::report_statuses(state, &status_ids, None).await?,
        account: match (staff, row.account_id) {
            (true, Some(id)) => super::admin::api_account(state, id).await?,
            _ => None,
        },
        report_id: row
            .report_id
            .filter(|_| role.can(&[flag::MANAGE_REPORTS]))
            .map(|id| id.to_string()),
    })
}

// ── GET /api/v1/disputes/strikes ──────────────────────────────────────────

/// `current_account.strikes.latest`.
pub async fn list_strikes(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<Strike>>> {
    auth.require_scope("read:accounts")?;
    let role = role::acting(&state.db, auth.account_id).await?;
    let rows = sqlx::query_as!(
        StrikeRow,
        r#"SELECT id, account_id, target_account_id, report_id, action, text, status_ids,
                  overruled_at, created_at
           FROM account_warnings WHERE target_account_id = $1 ORDER BY id DESC"#,
        auth.account_id
    )
    .fetch_all(&state.db)
    .await?;
    let mut strikes = Vec::with_capacity(rows.len());
    for row in rows {
        strikes.push(build(&state, row, auth.account_id, &role).await?);
    }
    Ok(Json(strikes))
}

// ── GET /api/v1/disputes/strikes/:id ──────────────────────────────────────

/// `AccountWarningPolicy#show?`: the account it was against, or staff who
/// handle appeals.
pub async fn get_strike(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Strike>> {
    auth.require_scope("read:accounts")?;
    let row = find(&state, id).await?;
    let role = role::acting(&state.db, auth.account_id).await?;
    authorize(row.target_account_id == Some(auth.account_id) || role.can(&[flag::MANAGE_APPEALS]))?;
    Ok(Json(build(&state, row, auth.account_id, &role).await?))
}

// ── POST /api/v1/disputes/strikes/:id/appeal ──────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AppealForm {
    pub text: Option<String>,
}

/// `Disputes::AppealsController#create`: only a strike of one's own
/// (`current_account.strikes.find`), and only within the window
/// (`AccountWarningPolicy#appeal?`).
pub async fn appeal_strike(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<AppealForm>,
) -> AppResult<Json<Strike>> {
    auth.require_scope("write:accounts")?;
    let row = find(&state, id).await?;
    if row.target_account_id != Some(auth.account_id) {
        return Err(AppError::NotFound);
    }
    authorize(appeal::appeal_eligible(row.created_at))?;
    appeal::create(&state, id, form.text.as_deref()).await?;
    let role = role::acting(&state.db, auth.account_id).await?;
    Ok(Json(
        strike_entity(&state, id, auth.account_id, &role).await?,
    ))
}
