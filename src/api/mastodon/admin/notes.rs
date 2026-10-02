//! `Admin::ReportNotesController` and `Admin::AccountModerationNotesController`:
//! the notes moderators leave each other on a report and on an account.

use axum::{
    extract::{Extension, Path},
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::{FlexBool, FlexId, Params};
use super::super::types::Account as ApiAccount;
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::{
        action_log::{self, Target},
        role::{self, authorize, flag},
    },
    state::AppState,
};

/// `ReportNote::CONTENT_SIZE_LIMIT` and `AccountModerationNote::CONTENT_SIZE_LIMIT`.
pub const CONTENT_SIZE_LIMIT: usize = 2_000;

/// A note as eunha serves it: who wrote it, when, and what it says.
#[derive(Debug, Serialize)]
pub struct Note {
    pub id: String,
    pub content: String,
    pub created_at: String,
    pub account: Option<ApiAccount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_account_id: Option<String>,
}

/// `validates :content, presence: true, length: { maximum: CONTENT_SIZE_LIMIT }`.
pub(super) fn validate_text(
    attribute: &str,
    value: Option<&str>,
    limit: usize,
) -> AppResult<String> {
    let value = value.unwrap_or_default();
    if value.trim().is_empty() {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: {attribute} can't be blank"
        )));
    }
    if value.chars().count() > limit {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: {attribute} is too long (maximum is {limit} characters)"
        )));
    }
    Ok(value.to_owned())
}

struct NoteRow {
    id: i64,
    account_id: i64,
    content: String,
    created_at: chrono::NaiveDateTime,
}

async fn build(
    state: &AppState,
    row: NoteRow,
    report_id: Option<i64>,
    target_account_id: Option<i64>,
) -> AppResult<Note> {
    Ok(Note {
        id: row.id.to_string(),
        content: row.content,
        created_at: super::super::convert::mastodon_date(row.created_at),
        account: super::api_account(state, row.account_id).await?,
        report_id: report_id.map(|id| id.to_string()),
        target_account_id: target_account_id.map(|id| id.to_string()),
    })
}

/// `ReportNotePolicy#destroy?` and `AccountModerationNotePolicy#destroy?`: the
/// author, or a role that may handle reports and outranks the author's.
async fn may_destroy(state: &AppState, actor_id: i64, author_id: i64) -> AppResult<bool> {
    if author_id == actor_id {
        return Ok(true);
    }
    let acting = role::acting(&state.db, actor_id).await?;
    let author_role = role::of_account(&state.db, author_id).await?;
    Ok(acting.can(&[flag::MANAGE_REPORTS]) && acting.overrides(author_role.as_ref()))
}

// ── Report notes ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ReportNotesParams {
    pub report_id: Option<FlexId>,
}

/// `GET /api/v1/admin/report_notes?report_id=`: `@report.notes.chronological`,
/// as the report page lists them.
pub async fn list_report_notes(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(params): Params<ReportNotesParams>,
) -> AppResult<Json<Vec<Note>>> {
    auth.require_scope("admin:read:reports")?;
    let report_id = params
        .report_id
        .ok_or_else(|| {
            AppError::BadRequest("param is missing or the value is empty: report_id".into())
        })?
        .0;
    super::require_permission(&state, auth.account_id, flag::MANAGE_REPORTS).await?;
    sqlx::query_scalar!("SELECT id FROM reports WHERE id = $1", report_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let rows = sqlx::query_as!(
        NoteRow,
        "SELECT id, account_id, content, created_at FROM report_notes WHERE report_id = $1 ORDER BY id",
        report_id,
    )
    .fetch_all(&state.db)
    .await?;
    let mut notes = Vec::with_capacity(rows.len());
    for row in rows {
        notes.push(build(&state, row, Some(report_id), None).await?);
    }
    Ok(Json(notes))
}

#[derive(Debug, Deserialize)]
pub struct CreateReportNoteForm {
    pub report_id: Option<FlexId>,
    pub content: Option<String>,
    pub create_and_resolve: Option<FlexBool>,
    pub create_and_unresolve: Option<FlexBool>,
}

/// `POST /api/v1/admin/report_notes`: `Admin::ReportNotesController#create`,
/// with its `create_and_resolve` and `create_and_unresolve` buttons.
pub async fn create_report_note(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<CreateReportNoteForm>,
) -> AppResult<Json<Note>> {
    auth.require_scope("admin:write:reports")?;
    // `authorize :report_note, :create?`
    super::require_permission(&state, auth.account_id, flag::MANAGE_REPORTS).await?;
    let content = validate_text("Content", form.content.as_deref(), CONTENT_SIZE_LIMIT)?;
    // `belongs_to :report`.
    let report_id = match form.report_id {
        Some(FlexId(id)) => {
            sqlx::query_scalar!("SELECT id FROM reports WHERE id = $1", id)
                .fetch_optional(&state.db)
                .await?
        }
        None => None,
    }
    .ok_or_else(|| AppError::Unprocessable("Validation failed: Report must exist".into()))?;

    let mut tx = state.db.begin().await?;
    let row = sqlx::query_as!(
        NoteRow,
        r#"INSERT INTO report_notes (account_id, report_id, content, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())
           RETURNING id, account_id, content, created_at"#,
        auth.account_id,
        report_id,
        content,
    )
    .fetch_one(&mut *tx)
    .await?;
    // `belongs_to :report, touch: true`.
    sqlx::query!(
        "UPDATE reports SET updated_at = now() WHERE id = $1",
        report_id
    )
    .execute(&mut *tx)
    .await?;
    if form.create_and_resolve.is_some_and(|b| b.0) {
        // `@report.resolve!(current_account)` and `log_action :resolve`.
        sqlx::query!(
            r#"UPDATE reports SET action_taken_at = now(), action_taken_by_account_id = $2,
                      updated_at = now()
               WHERE id = $1"#,
            report_id,
            auth.account_id,
        )
        .execute(&mut *tx)
        .await?;
        action_log::log(
            &mut *tx,
            auth.account_id,
            "resolve",
            &Target::report(report_id),
        )
        .await?;
    } else if form.create_and_unresolve.is_some_and(|b| b.0) {
        // `@report.unresolve!` and `log_action :reopen`.
        sqlx::query!(
            r#"UPDATE reports SET action_taken_at = NULL, action_taken_by_account_id = NULL,
                      updated_at = now()
               WHERE id = $1"#,
            report_id,
        )
        .execute(&mut *tx)
        .await?;
        action_log::log(
            &mut *tx,
            auth.account_id,
            "reopen",
            &Target::report(report_id),
        )
        .await?;
    }
    tx.commit().await?;
    crate::moderation::webhooks::trigger(
        &state,
        "report.updated",
        crate::moderation::webhooks::Object::Report(report_id),
    );
    Ok(Json(build(&state, row, Some(report_id), None).await?))
}

/// `DELETE /api/v1/admin/report_notes/:id`.
pub async fn delete_report_note(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("admin:write:reports")?;
    let author_id = sqlx::query_scalar!("SELECT account_id FROM report_notes WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    authorize(may_destroy(&state, auth.account_id, author_id).await?)?;
    sqlx::query!("DELETE FROM report_notes WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    Ok(Json(serde_json::json!({})))
}

// ── Account moderation notes ──────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AccountNotesParams {
    pub target_account_id: Option<FlexId>,
}

/// `GET /api/v1/admin/account_moderation_notes?target_account_id=`:
/// `@account.targeted_moderation_notes.chronological`, as the account page
/// lists them.
pub async fn list_account_moderation_notes(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(params): Params<AccountNotesParams>,
) -> AppResult<Json<Vec<Note>>> {
    auth.require_scope("admin:read:accounts")?;
    let target_id = params
        .target_account_id
        .ok_or_else(|| {
            AppError::BadRequest("param is missing or the value is empty: target_account_id".into())
        })?
        .0;
    // `authorize @account, :show?`
    super::require_permission(&state, auth.account_id, flag::MANAGE_USERS).await?;
    sqlx::query_scalar!("SELECT id FROM accounts WHERE id = $1", target_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let rows = sqlx::query_as!(
        NoteRow,
        r#"SELECT id, account_id, content, created_at FROM account_moderation_notes
           WHERE target_account_id = $1 ORDER BY id"#,
        target_id,
    )
    .fetch_all(&state.db)
    .await?;
    let mut notes = Vec::with_capacity(rows.len());
    for row in rows {
        notes.push(build(&state, row, None, Some(target_id)).await?);
    }
    Ok(Json(notes))
}

#[derive(Debug, Deserialize)]
pub struct CreateAccountNoteForm {
    pub target_account_id: Option<FlexId>,
    pub content: Option<String>,
}

/// `POST /api/v1/admin/account_moderation_notes`.
pub async fn create_account_moderation_note(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<CreateAccountNoteForm>,
) -> AppResult<Json<Note>> {
    auth.require_scope("admin:write:accounts")?;
    // `authorize AccountModerationNote, :create?`
    super::require_permission(&state, auth.account_id, flag::MANAGE_REPORTS).await?;
    let content = validate_text("Content", form.content.as_deref(), CONTENT_SIZE_LIMIT)?;
    let target_id = match form.target_account_id {
        Some(FlexId(id)) => {
            sqlx::query_scalar!("SELECT id FROM accounts WHERE id = $1", id)
                .fetch_optional(&state.db)
                .await?
        }
        None => None,
    }
    .ok_or_else(|| {
        AppError::Unprocessable("Validation failed: Target account must exist".into())
    })?;
    let row = sqlx::query_as!(
        NoteRow,
        r#"INSERT INTO account_moderation_notes
             (account_id, target_account_id, content, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())
           RETURNING id, account_id, content, created_at"#,
        auth.account_id,
        target_id,
        content,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(build(&state, row, None, Some(target_id)).await?))
}

/// `DELETE /api/v1/admin/account_moderation_notes/:id`.
pub async fn delete_account_moderation_note(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("admin:write:accounts")?;
    let author_id = sqlx::query_scalar!(
        "SELECT account_id FROM account_moderation_notes WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    authorize(may_destroy(&state, auth.account_id, author_id).await?)?;
    sqlx::query!("DELETE FROM account_moderation_notes WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    Ok(Json(serde_json::json!({})))
}
