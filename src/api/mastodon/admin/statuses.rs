//! `Admin::StatusesController`, `Admin::StatusFilter` and
//! `Admin::StatusBatchAction`: an account's posts as a moderator sees them,
//! and gathering some of them into a report.

use axum::{
    extract::{Extension, Path},
    http::{HeaderMap, Uri},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::{FlexBool, FlexId, FlexIds, Params};
use super::super::types::{Status, StatusEdit};
use crate::{
    db::models::{self, vis},
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::role::{self, authorize, flag},
    state::AppState,
};

/// `Admin::StatusesController::PER_PAGE`.
const PER_PAGE: i64 = 20;

/// `Admin::StatusBatchAction::TYPES`.
pub const BATCH_TYPES: &[&str] = &["report", "remove_from_report"];

fn require_scope(auth: &AuthenticatedUser, write: bool) -> AppResult<()> {
    auth.require_scope(if write {
        "admin:write:accounts"
    } else {
        "admin:read:accounts"
    })
}

/// `Admin::StatusPolicy#index?`: `manage_reports` or `manage_users`.
async fn require_index(state: &AppState, auth: &AuthenticatedUser) -> AppResult<role::Role> {
    let acting = role::acting(&state.db, auth.account_id).await?;
    authorize(acting.can(&[flag::MANAGE_REPORTS, flag::MANAGE_USERS]))?;
    Ok(acting)
}

async fn find_account(state: &AppState, id: i64) -> AppResult<models::Account> {
    sqlx::query_as!(models::Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

// ── GET /api/v1/admin/accounts/:account_id/statuses ───────────────────────

#[derive(Debug, Deserialize)]
pub struct AdminStatusesParams {
    pub media: Option<FlexBool>,
    pub limit: Option<FlexId>,
    pub max_id: Option<FlexId>,
    pub since_id: Option<FlexId>,
    pub min_id: Option<FlexId>,
}

/// `Admin::StatusFilter#results`: the account's public and unlisted posts,
/// or with `media` only those carrying its media; newest first.
pub async fn list_admin_account_statuses(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(account_id): Path<i64>,
    uri: Uri,
    req_headers: HeaderMap,
    Params(params): Params<AdminStatusesParams>,
) -> AppResult<impl IntoResponse> {
    require_scope(&auth, false)?;
    find_account(&state, account_id).await?;
    require_index(&state, &auth).await?;
    let page = super::PageParams {
        limit: params.limit,
        max_id: params.max_id,
        since_id: params.since_id,
        min_id: params.min_id,
    };
    let min_id = page.min_id.map(|i| i.0);
    let mut ids = sqlx::query_scalar!(
        r#"SELECT s.id FROM statuses s
           WHERE s.account_id = $1 AND s.deleted_at IS NULL
             AND s.visibility IN ($2, $3)
             AND (NOT $4 OR EXISTS (
                   SELECT 1 FROM media_attachments m
                   WHERE m.status_id = s.id AND m.account_id = $1))
             AND ($5::bigint IS NULL OR s.id < $5)
             AND ($6::bigint IS NULL OR s.id > $6)
             AND ($7::bigint IS NULL OR s.id > $7)
           ORDER BY CASE WHEN $7::bigint IS NULL THEN -s.id ELSE s.id END
           LIMIT $8"#,
        account_id,
        vis::PUBLIC,
        vis::UNLISTED,
        params.media.is_some_and(|b| b.0),
        page.max_id.map(|i| i.0),
        page.since_id.map(|i| i.0),
        min_id,
        page.limit(PER_PAGE, PER_PAGE * 2),
    )
    .fetch_all(&state.db)
    .await?;
    if min_id.is_some() {
        ids.reverse();
    }
    let statuses = super::report_statuses(&state, &ids).await?;
    let bounds = statuses
        .first()
        .zip(statuses.last())
        .map(|(n, o)| (n.id.as_str(), o.id.as_str()));
    let headers = super::super::link_headers(&req_headers, &uri, bounds);
    Ok((headers, Json(statuses)))
}

// ── GET /api/v1/admin/accounts/:account_id/statuses/:id ───────────────────

/// One post with its edit history, as the admin status page shows it.
#[derive(Debug, Serialize)]
pub struct AdminStatusDetail {
    #[serde(flatten)]
    pub status: Status,
    /// `batched_ordered_status_edits`, the current version last.
    pub edits: Vec<StatusEdit>,
}

/// `Status#reported?`: an open report about its author cites it, or a strike
/// against its author does.
async fn reported(state: &AppState, status: &models::Status) -> AppResult<bool> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM reports
             WHERE target_account_id = $1 AND action_taken_at IS NULL AND $2 = ANY(status_ids)
           ) OR EXISTS (
             SELECT 1 FROM account_warnings
             WHERE target_account_id = $1 AND $2::bigint::text = ANY(status_ids)
           ) AS "e!""#,
        status.account_id,
        status.id,
    )
    .fetch_one(&state.db)
    .await?)
}

/// `Admin::StatusPolicy#show?`: a moderator sees a post that is public or
/// unlisted, reported, or one they could read anyway.
pub async fn get_admin_account_status(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path((account_id, id)): Path<(i64, i64)>,
) -> AppResult<Json<AdminStatusDetail>> {
    require_scope(&auth, false)?;
    find_account(&state, account_id).await?;
    // `@account.statuses.find(params[:id])`.
    let status = sqlx::query_as!(
        models::Status,
        "SELECT * FROM statuses WHERE id = $1 AND account_id = $2 AND deleted_at IS NULL",
        id,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let acting = role::acting(&state.db, auth.account_id).await?;
    let eligible = matches!(status.visibility, vis::PUBLIC | vis::UNLISTED)
        || reported(&state, &status).await?
        || super::super::statuses::check_status_visible(&state, &status, auth.account_id)
            .await
            .is_ok();
    authorize(acting.can(&[flag::MANAGE_REPORTS, flag::MANAGE_USERS]) && eligible)?;
    let edits = super::super::statuses::status_edits(&state, &status).await?;
    let status = super::report_statuses(&state, &[id])
        .await?
        .pop()
        .ok_or(AppError::NotFound)?;
    Ok(Json(AdminStatusDetail { status, edits }))
}

// ── POST /api/v1/admin/accounts/:account_id/statuses/batch ────────────────

#[derive(Debug, Deserialize)]
pub struct StatusBatchForm {
    #[serde(rename = "type")]
    pub action_type: Option<String>,
    #[serde(default)]
    pub status_ids: FlexIds,
    pub report_id: Option<FlexId>,
}

/// `Admin::AccountStatusesFilter#results` narrowed to `ids`: the posts of
/// `target` that `viewer` could read, discarded ones included, without the
/// block check (`blocked?` is false for moderators).
async fn allowed_status_ids(
    state: &AppState,
    target: i64,
    viewer: i64,
    ids: &[i64],
) -> AppResult<Vec<i64>> {
    Ok(sqlx::query_scalar!(
        r#"SELECT s.id FROM statuses s
           JOIN accounts a ON a.id = s.account_id
           LEFT JOIN statuses r ON r.id = s.reblog_of_id
           LEFT JOIN accounts ra ON ra.id = r.account_id
           WHERE s.account_id = $1 AND s.id = ANY($3)
             AND a.suspended_at IS NULL
             AND (
               $1 = $2
               OR (
                 (s.visibility IN (0, 1)
                  OR (s.visibility = 2 AND EXISTS (
                        SELECT 1 FROM follows f WHERE f.account_id = $2 AND f.target_account_id = $1))
                  OR EXISTS (
                        SELECT 1 FROM mentions m WHERE m.status_id = s.id AND m.account_id = $2))
                 AND (s.reblog_of_id IS NULL OR (
                   (ra.domain IS NULL OR ra.domain NOT IN (
                      SELECT domain FROM account_domain_blocks WHERE account_id = $2))
                   AND r.account_id NOT IN (
                      SELECT target_account_id FROM blocks WHERE account_id = $2
                      UNION SELECT account_id FROM blocks WHERE target_account_id = $2
                      UNION SELECT target_account_id FROM mutes WHERE account_id = $2)))
               )
             )
           ORDER BY s.id"#,
        target,
        viewer,
        ids,
    )
    .fetch_all(&state.db)
    .await?)
}

/// `Admin::StatusesController#batch`: `report` adds the posts to the given
/// report, or to a new one about their author; `remove_from_report` takes
/// them out of the given report. Answers with the report.
pub async fn batch_admin_account_statuses(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(account_id): Path<i64>,
    Params(form): Params<StatusBatchForm>,
) -> AppResult<Json<serde_json::Value>> {
    require_scope(&auth, true)?;
    find_account(&state, account_id).await?;
    // `authorize [:admin, :status], :index?`
    require_index(&state, &auth).await?;
    // `params.expect(admin_status_batch_action: [status_ids: []])`.
    if form.status_ids.0.is_empty() {
        return Err(AppError::BadRequest(
            "No posts were changed as none were selected".into(),
        ));
    }
    // `validates :type, presence: true, inclusion: { in: TYPES }`.
    let kind = match form.action_type.as_deref().filter(|t| !t.is_empty()) {
        None => {
            return Err(AppError::Unprocessable(
                "Validation failed: Type can't be blank".into(),
            ))
        }
        Some(t) if BATCH_TYPES.contains(&t) => t.to_owned(),
        Some(_) => {
            return Err(AppError::Unprocessable(
                "Validation failed: Type is not included in the list".into(),
            ))
        }
    };
    // `Report.find(report_id) if report_id.present?`.
    let report = match form.report_id {
        Some(FlexId(id)) => Some(
            sqlx::query!(
                "SELECT id, target_account_id, status_ids FROM reports WHERE id = $1",
                id
            )
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?,
        ),
        None => None,
    };
    let ids = form.status_ids.0;

    let report_id = match kind.as_str() {
        "report" => {
            let (report_id, target_id, existing) = match &report {
                Some(r) => (Some(r.id), r.target_account_id, r.status_ids.clone()),
                None => {
                    // `statuses.first.account`.
                    let author = sqlx::query_scalar!(
                        "SELECT account_id FROM statuses WHERE id = ANY($1) LIMIT 1",
                        &ids
                    )
                    .fetch_optional(&state.db)
                    .await?
                    .ok_or(AppError::NotFound)?;
                    (None, author, vec![])
                }
            };
            let allowed = allowed_status_ids(&state, target_id, auth.account_id, &ids).await?;
            let mut status_ids = existing;
            for id in allowed {
                if !status_ids.contains(&id) {
                    status_ids.push(id);
                }
            }
            match report_id {
                Some(id) => {
                    sqlx::query!(
                        "UPDATE reports SET status_ids = $2, updated_at = now() WHERE id = $1",
                        id,
                        &status_ids,
                    )
                    .execute(&state.db)
                    .await?;
                    crate::moderation::webhooks::trigger(
                        &state,
                        "report.updated",
                        crate::moderation::webhooks::Object::Report(id),
                    )
                    .await;
                    Some(id)
                }
                None => {
                    // `Report.new(account: current_account, target_account:)`,
                    // whose `set_uri` gives a local reporter's report an id.
                    let uri =
                        crate::federation::relationships::generate_uri(&state.instance.domain);
                    let id = sqlx::query_scalar!(
                        r#"INSERT INTO reports
                             (account_id, target_account_id, status_ids, comment, uri,
                              created_at, updated_at)
                           VALUES ($1, $2, $3, '', $4, now(), now())
                           RETURNING id"#,
                        auth.account_id,
                        target_id,
                        &status_ids,
                        uri,
                    )
                    .fetch_one(&state.db)
                    .await?;
                    crate::moderation::webhooks::trigger(
                        &state,
                        "report.created",
                        crate::moderation::webhooks::Object::Report(id),
                    )
                    .await;
                    Some(id)
                }
            }
        }
        // `remove_from_report`, which does nothing without a report.
        _ => match &report {
            Some(r) => {
                let status_ids: Vec<i64> = r
                    .status_ids
                    .iter()
                    .copied()
                    .filter(|id| !ids.contains(id))
                    .collect();
                sqlx::query!(
                    "UPDATE reports SET status_ids = $2, updated_at = now() WHERE id = $1",
                    r.id,
                    &status_ids,
                )
                .execute(&state.db)
                .await?;
                crate::moderation::webhooks::trigger(
                    &state,
                    "report.updated",
                    crate::moderation::webhooks::Object::Report(r.id),
                )
                .await;
                Some(r.id)
            }
            None => None,
        },
    };

    match report_id {
        Some(id) => match super::admin_report_entity(&state, id).await? {
            Some(report) => Ok(Json(
                serde_json::to_value(report).map_err(anyhow::Error::from)?,
            )),
            None => Ok(Json(serde_json::json!({}))),
        },
        None => Ok(Json(serde_json::json!({}))),
    }
}
