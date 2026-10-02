//! `GET /api/v1_alpha/async_refreshes/:id`
//! (`Api::V1Alpha::AsyncRefreshesController`): how background work a request
//! started is getting on. See [`crate::async_refresh`].

use axum::{
    extract::{Extension, Path},
    Json,
};

use crate::{
    async_refresh::AsyncRefresh,
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

pub async fn show_async_refresh(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("read")?;
    let refresh = AsyncRefresh::find(&state, &id)
        .await
        .ok_or(AppError::NotFound)?;
    Ok(Json(refresh.to_json(&state)))
}
