//! `Admin::UsernameBlocksController`: the usernames sign-ups may not take, or
//! may take only with approval.

use axum::{
    extract::{Extension, Path},
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::{FlexBool, Params};
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::{
        action_log::{self, Target},
        role::flag,
        signup::normalize_username,
    },
    state::AppState,
};

/// A `username_blocks` row.
#[derive(Debug, Serialize)]
pub struct UsernameBlock {
    pub id: String,
    pub username: String,
    /// `UsernameBlock#comparison`: `equals` or `contains`.
    pub comparison: &'static str,
    pub allow_with_approval: bool,
    pub created_at: String,
    pub updated_at: String,
}

struct Row {
    id: i64,
    username: String,
    exact: bool,
    allow_with_approval: bool,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

impl From<Row> for UsernameBlock {
    fn from(r: Row) -> Self {
        Self {
            id: r.id.to_string(),
            username: r.username,
            comparison: if r.exact { "equals" } else { "contains" },
            allow_with_approval: r.allow_with_approval,
            created_at: super::super::convert::mastodon_date(r.created_at),
            updated_at: super::super::convert::mastodon_date(r.updated_at),
        }
    }
}

/// `UsernameBlockPolicy`: every action is `manage_blocks`.
async fn require_manage_blocks(state: &AppState, auth: &AuthenticatedUser) -> AppResult<()> {
    super::require_permission(state, auth.account_id, flag::MANAGE_BLOCKS).await
}

fn require_scope(auth: &AuthenticatedUser, write: bool) -> AppResult<()> {
    auth.require_scope(if write { "admin:write" } else { "admin:read" })
}

async fn find(state: &AppState, id: i64) -> AppResult<Row> {
    sqlx::query_as!(
        Row,
        r#"SELECT id, username, exact, allow_with_approval, created_at, updated_at
           FROM username_blocks WHERE id = $1"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

/// `validates :username, presence: true, uniqueness: true`. The table's
/// unique index is on `lower(username)`, so a name that differs only in case
/// is refused here as taken rather than by the index.
async fn validate(state: &AppState, username: &str, id: Option<i64>) -> AppResult<()> {
    if username.trim().is_empty() {
        return Err(AppError::Unprocessable(
            "Validation failed: Username can't be blank".into(),
        ));
    }
    let taken = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM username_blocks
             WHERE lower(username) = lower($1) AND ($2::bigint IS NULL OR id <> $2)
           ) AS "e!""#,
        username,
        id,
    )
    .fetch_one(&state.db)
    .await?;
    if taken {
        return Err(AppError::Unprocessable(
            "Validation failed: Username has already been taken".into(),
        ));
    }
    Ok(())
}

/// `GET /api/v1/admin/username_blocks`: `UsernameBlock.order(username: :asc)`.
pub async fn list_username_blocks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<UsernameBlock>>> {
    require_scope(&auth, false)?;
    require_manage_blocks(&state, &auth).await?;
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT id, username, exact, allow_with_approval, created_at, updated_at
           FROM username_blocks ORDER BY username ASC"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

/// `GET /api/v1/admin/username_blocks/:id`.
pub async fn get_username_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<UsernameBlock>> {
    require_scope(&auth, false)?;
    let row = find(&state, id).await?;
    require_manage_blocks(&state, &auth).await?;
    Ok(Json(row.into()))
}

#[derive(Debug, Deserialize)]
pub struct UsernameBlockForm {
    pub username: Option<String>,
    /// `comparison=`: `equals` makes the block exact, anything else partial.
    pub comparison: Option<String>,
    pub allow_with_approval: Option<FlexBool>,
}

/// `POST /api/v1/admin/username_blocks`.
pub async fn create_username_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<UsernameBlockForm>,
) -> AppResult<Json<UsernameBlock>> {
    require_scope(&auth, true)?;
    require_manage_blocks(&state, &auth).await?;
    let username = form.username.unwrap_or_default();
    validate(&state, &username, None).await?;
    let exact = form.comparison.as_deref() == Some("equals");
    let mut tx = state.db.begin().await?;
    let row = sqlx::query_as!(
        Row,
        r#"INSERT INTO username_blocks
             (username, normalized_username, exact, allow_with_approval, created_at, updated_at)
           VALUES ($1, $2, $3, $4, now(), now())
           RETURNING id, username, exact, allow_with_approval, created_at, updated_at"#,
        username,
        normalize_username(&username),
        exact,
        form.allow_with_approval.is_some_and(|b| b.0),
    )
    .fetch_one(&mut *tx)
    .await?;
    action_log::log(
        &mut *tx,
        auth.account_id,
        "create",
        &Target::username_block(row.id, &row.username),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(row.into()))
}

/// `PATCH /api/v1/admin/username_blocks/:id`.
pub async fn update_username_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<UsernameBlockForm>,
) -> AppResult<Json<UsernameBlock>> {
    require_scope(&auth, true)?;
    let current = find(&state, id).await?;
    require_manage_blocks(&state, &auth).await?;
    let username = form.username.unwrap_or(current.username);
    validate(&state, &username, Some(id)).await?;
    let exact = match form.comparison.as_deref() {
        Some(comparison) => comparison == "equals",
        None => current.exact,
    };
    let allow_with_approval = form
        .allow_with_approval
        .map_or(current.allow_with_approval, |b| b.0);
    let mut tx = state.db.begin().await?;
    let row = sqlx::query_as!(
        Row,
        r#"UPDATE username_blocks
           SET username = $2, normalized_username = $3, exact = $4, allow_with_approval = $5,
               updated_at = now()
           WHERE id = $1
           RETURNING id, username, exact, allow_with_approval, created_at, updated_at"#,
        id,
        username,
        normalize_username(&username),
        exact,
        allow_with_approval,
    )
    .fetch_one(&mut *tx)
    .await?;
    action_log::log(
        &mut *tx,
        auth.account_id,
        "update",
        &Target::username_block(row.id, &row.username),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(row.into()))
}

/// `DELETE /api/v1/admin/username_blocks/:id`: one block of
/// `Form::UsernameBlockBatch`'s `delete`.
pub async fn delete_username_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    require_scope(&auth, true)?;
    let row = find(&state, id).await?;
    require_manage_blocks(&state, &auth).await?;
    let mut tx = state.db.begin().await?;
    sqlx::query!("DELETE FROM username_blocks WHERE id = $1", id)
        .execute(&mut *tx)
        .await?;
    action_log::log(
        &mut *tx,
        auth.account_id,
        "destroy",
        &Target::username_block(row.id, &row.username),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(serde_json::json!({})))
}
