//! `Api::V1::ReportsController`.

use axum::{extract::Extension, Json};
use serde::Deserialize;

use super::extractors::{FlexBool, FlexId, FlexIds, Params};
use super::types::Report;
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::report_service,
    state::AppState,
};

// ── POST /api/v1/reports ──────────────────────────────────────────────────

/// `report_params`: `:account_id, :comment, :category, :forward,
/// forward_to_domains: [], status_ids: [], collection_ids: [], rule_ids: []`.
#[derive(Debug, Deserialize)]
pub struct ReportForm {
    pub account_id: Option<FlexId>,
    pub status_ids: Option<FlexIds>,
    pub collection_ids: Option<FlexIds>,
    pub comment: Option<String>,
    pub forward: Option<FlexBool>,
    pub forward_to_domains: Option<Vec<String>>,
    pub category: Option<String>,
    pub rule_ids: Option<FlexIds>,
}

pub async fn file_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<ReportForm>,
) -> AppResult<Json<Report>> {
    auth.require_scope("write:reports")?;
    let source = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?;
    // `Account.find(report_params[:account_id])`
    let target_id = form.account_id.ok_or(AppError::NotFound)?.0;
    let target = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        target_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let report_id = report_service::call(
        &state,
        &source,
        &target,
        report_service::Options {
            status_ids: form.status_ids.unwrap_or_default().0,
            collection_ids: form.collection_ids.unwrap_or_default().0,
            comment: form.comment.unwrap_or_default(),
            category: form.category,
            rule_ids: form.rule_ids.unwrap_or_default().0,
            forward: form.forward.is_some_and(|f| f.0),
            forward_to_domains: form.forward_to_domains,
            uri: None,
            application_id: auth.application_id,
        },
    )
    .await?;

    let report = super::notifications::report_entity(&state, report_id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(report))
}
