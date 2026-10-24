//! The health check a control plane polls.
//!
//! Mastodon: none

use axum::Json;
use serde::Serialize;

use crate::{error::AppResult, state::AppState};

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
}

/// GET /api/eunha/v1/health
///
/// eunha-specific endpoint (Mastodon has no health API). One trivial query, so
/// a 200 says both this process and the instance's database are answering.
///
/// A control plane running many instances checks each of them on a timer, and
/// `/api/v2/instance` is a poor thing to check: it counts distinct domains and
/// posting accounts, takes a connection per statistic, and repeating it faster
/// than `database_pool.idle_timeout_seconds` leaves an instance that serves
/// nobody holding its whole pool. This costs one connection, briefly.
pub async fn health(state: AppState) -> AppResult<Json<HealthResponse>> {
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.db)
        .await?;
    Ok(Json(HealthResponse { status: "ok" }))
}
