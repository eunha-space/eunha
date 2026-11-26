//! `Admin::DashboardController#index`: the counts of what waits for a
//! moderator, and of the server health checks that want attention, for
//! `view_dashboard`.
//!
//! Mastodon: app/controllers/admin/dashboard_controller.rb
//! Mastodon: app/models/admin/server_check.rb, app/policies/dashboard_policy.rb

use axum::{extract::Extension, Json};
use serde::Serialize;

use crate::{
    error::AppResult,
    middleware::AuthenticatedUser,
    moderation::role::{self, flag},
    state::AppState,
};

#[derive(Debug, Serialize)]
pub struct Dashboard {
    pub pending_appeals_count: i64,
    pub pending_reports_count: i64,
    pub pending_tags_count: i64,
    pub pending_users_count: i64,
    /// How many of the server health checks the role may see are pending and
    /// critical or a warning, which the dashboard sends to that page for.
    pub pending_server_checks: usize,
}

/// `GET /api/v1/admin/dashboard`.
pub async fn get_admin_dashboard(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Dashboard>> {
    auth.require_scope("admin:read")?;
    let acting = role::acting(&state.db, auth.account_id).await?;
    role::authorize(acting.can(&[flag::VIEW_DASHBOARD]))?;
    let counts = sqlx::query!(
        r#"SELECT
             (SELECT count(*) FROM appeals WHERE approved_at IS NULL AND rejected_at IS NULL)
               AS "appeals!",
             (SELECT count(*) FROM reports WHERE action_taken_at IS NULL) AS "reports!",
             (SELECT count(DISTINCT t.id) FROM tags t JOIN tag_trends tt ON tt.tag_id = t.id
              WHERE t.reviewed_at IS NULL AND t.requested_review_at IS NOT NULL) AS "tags!",
             (SELECT count(*) FROM users WHERE NOT approved) AS "users!""#
    )
    .fetch_one(&state.db)
    .await?;
    let pending_server_checks = crate::server_checks::important_count(&state, &acting).await?;
    Ok(Json(Dashboard {
        pending_appeals_count: counts.appeals,
        pending_reports_count: counts.reports,
        pending_tags_count: counts.tags,
        pending_users_count: counts.users,
        pending_server_checks,
    }))
}
