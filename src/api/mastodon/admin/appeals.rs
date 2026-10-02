//! `Admin::Disputes::AppealsController` and `Admin::AppealFilter`.

use axum::{
    extract::{Extension, Path},
    http::{HeaderMap, Uri},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::disputes::{strike_entity, AppealEntity, Strike};
use super::super::extractors::{FlexId, Params};
use super::super::types::Account as ApiAccount;
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::{
        appeal,
        role::{self, authorize, flag},
    },
    state::AppState,
};

/// An appeal as the appeals page lists it: what was said, by whom, against
/// which strike.
#[derive(Debug, Serialize)]
pub struct AdminAppeal {
    #[serde(flatten)]
    pub appeal: AppealEntity,
    pub account: Option<ApiAccount>,
    pub strike: Strike,
}

fn require_scope(auth: &AuthenticatedUser, write: bool) -> AppResult<()> {
    auth.require_scope(if write {
        "admin:write:accounts"
    } else {
        "admin:read:accounts"
    })
}

#[derive(Debug, Deserialize)]
pub struct AppealsParams {
    pub status: Option<String>,
    pub limit: Option<FlexId>,
    pub max_id: Option<FlexId>,
    pub since_id: Option<FlexId>,
    pub min_id: Option<FlexId>,
}

async fn render(state: &AppState, auth: &AuthenticatedUser, id: i64) -> AppResult<AdminAppeal> {
    let row = sqlx::query!(
        "SELECT account_id, account_warning_id FROM appeals WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let role = role::acting(&state.db, auth.account_id).await?;
    let strike = strike_entity(state, row.account_warning_id, auth.account_id, &role).await?;
    let appeal = sqlx::query!(
        "SELECT text, approved_at, rejected_at, created_at FROM appeals WHERE id = $1",
        id
    )
    .fetch_one(&state.db)
    .await?;
    use super::super::convert::mastodon_date;
    Ok(AdminAppeal {
        appeal: AppealEntity {
            id: id.to_string(),
            text: appeal.text,
            state: if appeal.approved_at.is_some() {
                "approved"
            } else if appeal.rejected_at.is_some() {
                "rejected"
            } else {
                "pending"
            },
            created_at: mastodon_date(appeal.created_at),
            approved_at: appeal.approved_at.map(mastodon_date),
            rejected_at: appeal.rejected_at.map(mastodon_date),
        },
        account: super::api_account(state, row.account_id).await?,
        strike,
    })
}

// ── GET /api/v1/admin/disputes/appeals ────────────────────────────────────

/// `Admin::AppealFilter` with `status: 'pending'` by default, newest first.
pub async fn list_appeals(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Params(params): Params<AppealsParams>,
) -> AppResult<impl IntoResponse> {
    require_scope(&auth, false)?;
    // `authorize :appeal, :index?`
    super::require_permission(&state, auth.account_id, flag::MANAGE_APPEALS).await?;
    let status = params
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("pending");
    if !matches!(status, "pending" | "approved" | "rejected") {
        return Err(AppError::BadRequest(format!("Unknown status: {status}")));
    }
    let page = super::PageParams {
        limit: params.limit,
        max_id: params.max_id,
        since_id: params.since_id,
        min_id: params.min_id,
    };
    let min_id = page.min_id.map(|i| i.0);
    let mut ids = sqlx::query_scalar!(
        r#"SELECT id FROM appeals
           WHERE CASE $1
                   WHEN 'approved' THEN approved_at IS NOT NULL
                   WHEN 'rejected' THEN rejected_at IS NOT NULL
                   ELSE approved_at IS NULL AND rejected_at IS NULL
                 END
             AND ($2::bigint IS NULL OR id < $2)
             AND ($3::bigint IS NULL OR id > $3)
             AND ($4::bigint IS NULL OR id > $4)
           ORDER BY CASE WHEN $4::bigint IS NULL THEN -id ELSE id END
           LIMIT $5"#,
        status,
        page.max_id.map(|i| i.0),
        page.since_id.map(|i| i.0),
        min_id,
        page.limit(40, 80),
    )
    .fetch_all(&state.db)
    .await?;
    if min_id.is_some() {
        ids.reverse();
    }
    let mut result = Vec::with_capacity(ids.len());
    for id in ids {
        result.push(render(&state, &auth, id).await?);
    }
    let bounds = result
        .first()
        .zip(result.last())
        .map(|(n, o)| (n.appeal.id.as_str(), o.appeal.id.as_str()));
    let headers = super::super::link_headers(&req_headers, &uri, bounds);
    Ok((headers, Json(result)))
}

async fn decide(
    state: &AppState,
    auth: &AuthenticatedUser,
    id: i64,
    approve: bool,
) -> AppResult<Json<AdminAppeal>> {
    require_scope(auth, true)?;
    let pending = appeal::is_pending(state, id).await?;
    // `AppealPolicy#approve?` and `#reject?`.
    let acting = role::acting(&state.db, auth.account_id).await?;
    authorize(pending && acting.can(&[flag::MANAGE_APPEALS]))?;
    if approve {
        appeal::approve(state, id, auth.account_id).await?;
    } else {
        appeal::reject(state, id, auth.account_id).await?;
    }
    Ok(Json(render(state, auth, id).await?))
}

/// `POST /api/v1/admin/disputes/appeals/:id/approve`.
pub async fn approve_appeal(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAppeal>> {
    decide(&state, &auth, id, true).await
}

/// `POST /api/v1/admin/disputes/appeals/:id/reject`.
pub async fn reject_appeal(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAppeal>> {
    decide(&state, &auth, id, false).await
}
