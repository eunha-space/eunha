//! `Admin::WarningPresetsController`: canned warning texts the account action
//! form offers.

use axum::{
    extract::{Extension, Path},
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::Params;
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::role::{self, authorize, flag},
    state::AppState,
};

/// An `account_warning_presets` row.
#[derive(Debug, Serialize)]
pub struct WarningPreset {
    pub id: String,
    pub title: String,
    pub text: String,
    pub created_at: String,
    pub updated_at: String,
}

struct Row {
    id: i64,
    title: String,
    text: String,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

impl From<Row> for WarningPreset {
    fn from(r: Row) -> Self {
        Self {
            id: r.id.to_string(),
            title: r.title,
            text: r.text,
            created_at: super::super::convert::mastodon_date(r.created_at),
            updated_at: super::super::convert::mastodon_date(r.updated_at),
        }
    }
}

/// `AccountWarningPresetPolicy`: every action is `manage_settings`.
async fn require_manage_settings(state: &AppState, auth: &AuthenticatedUser) -> AppResult<()> {
    super::require_permission(state, auth.account_id, flag::MANAGE_SETTINGS).await
}

async fn find(state: &AppState, id: i64) -> AppResult<Row> {
    sqlx::query_as!(
        Row,
        "SELECT id, title, text, created_at, updated_at FROM account_warning_presets WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

/// `validates :text, presence: true`.
fn validate(text: &str) -> AppResult<()> {
    if text.trim().is_empty() {
        return Err(AppError::Unprocessable(
            "Validation failed: Text can't be blank".into(),
        ));
    }
    Ok(())
}

/// `GET /api/v1/admin/warning_presets`: `AccountWarningPreset.alphabetic`.
///
/// Mastodon's preset page asks for `manage_settings`, but its account action
/// form lists the presets to anyone who may open it, which is `manage_users`
/// (`AccountPolicy#show?`) — or `manage_reports`, for the form a report
/// leads to. Listing them is open to all three, so the form here can offer
/// them too.
pub async fn list_warning_presets(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<WarningPreset>>> {
    auth.require_scope("admin:read")?;
    let acting = role::acting(&state.db, auth.account_id).await?;
    authorize(acting.can(&[
        flag::MANAGE_SETTINGS,
        flag::MANAGE_USERS,
        flag::MANAGE_REPORTS,
    ]))?;
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT id, title, text, created_at, updated_at FROM account_warning_presets
           ORDER BY title ASC, text ASC"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

/// `GET /api/v1/admin/warning_presets/:id`, what the edit page shows.
pub async fn get_warning_preset(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<WarningPreset>> {
    auth.require_scope("admin:read")?;
    let row = find(&state, id).await?;
    require_manage_settings(&state, &auth).await?;
    Ok(Json(row.into()))
}

#[derive(Debug, Deserialize)]
pub struct WarningPresetForm {
    pub title: Option<String>,
    pub text: Option<String>,
}

/// `POST /api/v1/admin/warning_presets`.
pub async fn create_warning_preset(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<WarningPresetForm>,
) -> AppResult<Json<WarningPreset>> {
    auth.require_scope("admin:write")?;
    require_manage_settings(&state, &auth).await?;
    let text = form.text.unwrap_or_default();
    validate(&text)?;
    let row = sqlx::query_as!(
        Row,
        r#"INSERT INTO account_warning_presets (title, text, created_at, updated_at)
           VALUES ($1, $2, now(), now())
           RETURNING id, title, text, created_at, updated_at"#,
        form.title.unwrap_or_default(),
        text,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row.into()))
}

/// `PATCH /api/v1/admin/warning_presets/:id`.
pub async fn update_warning_preset(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<WarningPresetForm>,
) -> AppResult<Json<WarningPreset>> {
    auth.require_scope("admin:write")?;
    let current = find(&state, id).await?;
    require_manage_settings(&state, &auth).await?;
    let text = form.text.unwrap_or(current.text);
    validate(&text)?;
    let row = sqlx::query_as!(
        Row,
        r#"UPDATE account_warning_presets SET title = $2, text = $3, updated_at = now()
           WHERE id = $1
           RETURNING id, title, text, created_at, updated_at"#,
        id,
        form.title.unwrap_or(current.title),
        text,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row.into()))
}

/// `DELETE /api/v1/admin/warning_presets/:id`.
pub async fn delete_warning_preset(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("admin:write")?;
    find(&state, id).await?;
    require_manage_settings(&state, &auth).await?;
    sqlx::query!("DELETE FROM account_warning_presets WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    Ok(Json(serde_json::json!({})))
}
