//! `Api::V1::Admin::ReportsController`.

use axum::{
    extract::{Extension, Path},
    http::{HeaderMap, Uri},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::{FlexBool, FlexId, FlexIds, Params};
use super::accounts::{build_admin_account, AdminAccount};
use crate::{
    db::models::{self, report_category},
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::{
        action_log::{self, Target},
        role::flag,
    },
    state::AppState,
};

/// `Api::V1::Admin::ReportsController::LIMIT`.
const LIMIT: i64 = 100;

struct ReportRow {
    id: i64,
    account_id: i64,
    target_account_id: i64,
    assigned_account_id: Option<i64>,
    action_taken_by_account_id: Option<i64>,
    action_taken_at: Option<chrono::NaiveDateTime>,
    category: i32,
    comment: String,
    forwarded: Option<bool>,
    status_ids: Vec<i64>,
    rule_ids: Option<Vec<i64>>,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

/// `REST::Admin::ReportSerializer`.
#[derive(Debug, Serialize)]
pub struct AdminReport {
    pub id: String,
    pub action_taken: bool,
    pub action_taken_at: Option<String>,
    pub category: &'static str,
    pub comment: String,
    pub forwarded: Option<bool>,
    pub created_at: String,
    pub updated_at: String,
    pub account: Option<AdminAccount>,
    pub target_account: Option<AdminAccount>,
    pub assigned_account: Option<AdminAccount>,
    pub action_taken_by_account: Option<AdminAccount>,
    pub statuses: Vec<super::super::types::Status>,
    pub rules: Vec<super::super::types::Rule>,
}

async fn admin_account(state: &AppState, id: Option<i64>) -> AppResult<Option<AdminAccount>> {
    let Some(id) = id else {
        return Ok(None);
    };
    match sqlx::query_as!(models::Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
    {
        Some(account) => Ok(Some(build_admin_account(state, &account).await?)),
        None => Ok(None),
    }
}

/// `Report#statuses`: `Status.with_discarded.where(id: status_ids)`, each
/// serialized for `viewer` — the moderator reading the report, whose
/// `favourited`, `bookmarked` and the rest `REST::StatusSerializer` fills in
/// from `current_user` — or for nobody where nobody is reading, as a webhook.
pub(crate) async fn report_statuses(
    state: &AppState,
    ids: &[i64],
    viewer: Option<i64>,
) -> AppResult<Vec<super::super::types::Status>> {
    use super::super::status_serialize::{build_status, fetch_reblog_data, fetch_status_media};
    let statuses = sqlx::query_as!(
        models::Status,
        "SELECT * FROM statuses WHERE id = ANY($1) ORDER BY id DESC",
        ids,
    )
    .fetch_all(&state.db)
    .await?;
    let mut out = Vec::with_capacity(statuses.len());
    for s in statuses {
        let Some(author) = sqlx::query_as!(
            models::Account,
            "SELECT * FROM accounts WHERE id = $1",
            s.account_id
        )
        .fetch_optional(&state.db)
        .await?
        else {
            continue;
        };
        let media = fetch_status_media(state, s.id).await?;
        let reblog = fetch_reblog_data(state, &s).await?;
        let viewer_ctx = match viewer {
            Some(v) => Some(super::super::statuses::build_viewer_context(state, v, s.id).await?),
            None => None,
        };
        out.push(build_status(state, &s, &author, media, reblog, viewer_ctx).await?);
    }
    Ok(out)
}

async fn build(state: &AppState, r: ReportRow, viewer: Option<i64>) -> AppResult<AdminReport> {
    let rules = match r.rule_ids.as_deref() {
        Some(ids) if !ids.is_empty() => {
            crate::moderation::rules::serialize(state, Some(ids)).await?
        }
        _ => vec![],
    };
    Ok(AdminReport {
        id: r.id.to_string(),
        action_taken: r.action_taken_at.is_some(),
        action_taken_at: r.action_taken_at.map(super::super::convert::mastodon_date),
        category: report_category::to_str(r.category),
        comment: r.comment,
        forwarded: r.forwarded,
        created_at: super::super::convert::mastodon_date(r.created_at),
        updated_at: super::super::convert::mastodon_date(r.updated_at),
        account: admin_account(state, Some(r.account_id)).await?,
        target_account: admin_account(state, Some(r.target_account_id)).await?,
        assigned_account: admin_account(state, r.assigned_account_id).await?,
        action_taken_by_account: admin_account(state, r.action_taken_by_account_id).await?,
        statuses: report_statuses(state, &r.status_ids, viewer).await?,
        rules,
    })
}

async fn find(state: &AppState, id: i64) -> AppResult<ReportRow> {
    sqlx::query_as!(
        ReportRow,
        r#"SELECT id, account_id, target_account_id, assigned_account_id,
                  action_taken_by_account_id, action_taken_at, category, comment, forwarded,
                  status_ids, rule_ids, created_at, updated_at
           FROM reports WHERE id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

async fn render(state: &AppState, id: i64, viewer: i64) -> AppResult<Json<AdminReport>> {
    let row = find(state, id).await?;
    Ok(Json(build(state, row, Some(viewer)).await?))
}

/// `REST::Admin::ReportSerializer` of one report, if it exists.
pub async fn admin_report_entity(state: &AppState, id: i64) -> AppResult<Option<AdminReport>> {
    match find(state, id).await {
        Ok(row) => Ok(Some(build(state, row, None).await?)),
        Err(AppError::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

fn require_scope(auth: &AuthenticatedUser, write: bool) -> AppResult<()> {
    auth.require_scope(if write {
        "admin:write:reports"
    } else {
        "admin:read:reports"
    })
}

// ── GET /api/v1/admin/reports ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ReportsParams {
    pub resolved: Option<FlexBool>,
    pub unresolved: Option<FlexBool>,
    pub account_id: Option<FlexId>,
    pub target_account_id: Option<FlexId>,
    pub limit: Option<FlexId>,
    pub max_id: Option<FlexId>,
    pub since_id: Option<FlexId>,
    pub min_id: Option<FlexId>,
}

pub async fn list_admin_reports(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Params(params): Params<ReportsParams>,
) -> AppResult<impl IntoResponse> {
    require_scope(&auth, false)?;
    super::require_permission(&state, auth.account_id, flag::MANAGE_REPORTS).await?;

    // `ReportFilter#status_scope`: both is everything, resolved is resolved,
    // and anything else is the open ones.
    let resolved = params.resolved.is_some_and(|b| b.0);
    let unresolved = params.unresolved.is_some_and(|b| b.0);
    let scope = match (resolved, unresolved) {
        (true, true) => "all",
        (true, false) => "resolved",
        _ => "unresolved",
    };
    let limit = params.limit.map_or(LIMIT, |l| l.0.abs().min(LIMIT * 2));
    let min_id = params.min_id.map(|i| i.0);
    let mut rows = sqlx::query_as!(
        ReportRow,
        r#"SELECT id, account_id, target_account_id, assigned_account_id,
                  action_taken_by_account_id, action_taken_at, category, comment, forwarded,
                  status_ids, rule_ids, created_at, updated_at
           FROM reports
           WHERE ($1 = 'all' OR ($1 = 'resolved') = (action_taken_at IS NOT NULL))
             AND ($2::bigint IS NULL OR account_id = $2)
             AND ($3::bigint IS NULL OR target_account_id = $3)
             AND ($4::bigint IS NULL OR id < $4)
             AND ($5::bigint IS NULL OR id > $5)
             AND ($6::bigint IS NULL OR id > $6)
           ORDER BY CASE WHEN $6::bigint IS NULL THEN -id ELSE id END
           LIMIT $7"#,
        scope,
        params.account_id.map(|i| i.0),
        params.target_account_id.map(|i| i.0),
        params.max_id.map(|i| i.0),
        params.since_id.map(|i| i.0),
        min_id,
        limit,
    )
    .fetch_all(&state.db)
    .await?;
    if min_id.is_some() {
        rows.reverse();
    }

    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        result.push(build(&state, row, Some(auth.account_id)).await?);
    }
    let bounds = result
        .first()
        .zip(result.last())
        .map(|(n, o)| (n.id.as_str(), o.id.as_str()));
    let headers = super::super::link_headers(&req_headers, &uri, bounds);
    Ok((headers, Json(result)))
}

// ── GET /api/v1/admin/reports/:id ────────────────────────────────────────

pub async fn get_admin_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminReport>> {
    require_scope(&auth, false)?;
    let row = find(&state, id).await?;
    super::require_permission(&state, auth.account_id, flag::MANAGE_REPORTS).await?;
    Ok(Json(build(&state, row, Some(auth.account_id)).await?))
}

// ── PATCH /api/v1/admin/reports/:id ──────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UpdateReportForm {
    pub category: Option<String>,
    pub rule_ids: Option<FlexIds>,
}

pub async fn update_admin_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<UpdateReportForm>,
) -> AppResult<Json<AdminReport>> {
    require_scope(&auth, true)?;
    let row = find(&state, id).await?;
    super::require_permission(&state, auth.account_id, flag::MANAGE_REPORTS).await?;

    let category = match form.category.as_deref() {
        Some(c) => report_category::parse(c)
            .ok_or_else(|| AppError::Unprocessable(format!("'{c}' is not a valid category")))?,
        None => row.category,
    };
    let rule_ids: Option<Vec<i64>> = match form.rule_ids {
        Some(ids) => (!ids.0.is_empty()).then_some(ids.0),
        None => row.rule_ids.clone().filter(|ids| !ids.is_empty()),
    };
    let changed =
        category != row.category || rule_ids != row.rule_ids.clone().filter(|i| !i.is_empty());
    if changed {
        // `validates :rule_ids, absence: true` unless a violation, and
        // `validate_rule_ids` when one.
        if category != report_category::VIOLATION && rule_ids.is_some() {
            return Err(AppError::Unprocessable(
                "Validation failed: Rule ids must be blank".into(),
            ));
        }
        if category == report_category::VIOLATION {
            if let Some(ids) = &rule_ids {
                if !crate::moderation::rules::all_exist(&state, ids).await? {
                    return Err(AppError::Unprocessable(
                        "Validation failed: Rule ids does not reference valid rules".into(),
                    ));
                }
            }
        }
    }
    sqlx::query!(
        "UPDATE reports SET category = $2, rule_ids = $3, updated_at = now() WHERE id = $1",
        id,
        category,
        rule_ids.as_deref(),
    )
    .execute(&state.db)
    .await?;
    action_log::log(&state.db, auth.account_id, "update", &Target::report(id)).await?;
    crate::moderation::webhooks::trigger(
        &state,
        "report.updated",
        crate::moderation::webhooks::Object::Report(id),
    )
    .await;
    render(&state, id, auth.account_id).await
}

async fn act(
    state: &AppState,
    auth: &AuthenticatedUser,
    id: i64,
    action: &str,
    sql: &str,
) -> AppResult<Json<AdminReport>> {
    require_scope(auth, true)?;
    find(state, id).await?;
    super::require_permission(state, auth.account_id, flag::MANAGE_REPORTS).await?;
    sqlx::query(sql)
        .bind(id)
        .bind(auth.account_id)
        .execute(&state.db)
        .await?;
    action_log::log(&state.db, auth.account_id, action, &Target::report(id)).await?;
    crate::moderation::webhooks::trigger(
        state,
        "report.updated",
        crate::moderation::webhooks::Object::Report(id),
    )
    .await;
    render(state, id, auth.account_id).await
}

// ── POST /api/v1/admin/reports/:id/assign_to_self ────────────────────────

pub async fn assign_report_to_self(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminReport>> {
    act(
        &state,
        &auth,
        id,
        "assigned_to_self",
        "UPDATE reports SET assigned_account_id = $2, updated_at = now() WHERE id = $1",
    )
    .await
}

// ── POST /api/v1/admin/reports/:id/unassign ──────────────────────────────

pub async fn unassign_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminReport>> {
    act(
        &state,
        &auth,
        id,
        "unassigned",
        "UPDATE reports SET assigned_account_id = NULL, updated_at = now() WHERE id = $1 AND $2::bigint IS NOT NULL",
    )
    .await
}

// ── POST /api/v1/admin/reports/:id/reopen ────────────────────────────────

pub async fn reopen_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminReport>> {
    act(
        &state,
        &auth,
        id,
        "reopen",
        "UPDATE reports SET action_taken_at = NULL, action_taken_by_account_id = NULL, updated_at = now() WHERE id = $1 AND $2::bigint IS NOT NULL",
    )
    .await
}

// ── POST /api/v1/admin/reports/:id/resolve ───────────────────────────────

pub async fn resolve_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminReport>> {
    act(
        &state,
        &auth,
        id,
        "resolve",
        "UPDATE reports SET action_taken_at = now(), action_taken_by_account_id = $2, updated_at = now() WHERE id = $1",
    )
    .await
}

// ── POST /api/v1/admin/reports/:id/actions ───────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ReportActionForm {
    /// `delete`, `mark_as_sensitive`, `silence` or `suspend`.
    pub moderation_action: Option<String>,
    pub text: Option<String>,
}

/// `Admin::Reports::ActionsController#create`: removing or marking sensitive
/// what the report cites (`Admin::ModerationAction`), or limiting or
/// suspending its account (`Admin::AccountAction`). A spam report's account
/// is not notified. Eunha's own endpoint; see `moderation-tools-rest-api`.
pub async fn report_moderation_action(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<ReportActionForm>,
) -> AppResult<Json<AdminReport>> {
    require_scope(&auth, true)?;
    let report = find(&state, id).await?;
    // `authorize @report, :show?`
    super::require_permission(&state, auth.account_id, flag::MANAGE_REPORTS).await?;
    let kind = form.moderation_action.unwrap_or_default();
    let send_email_notification = report.category != report_category::SPAM;
    match kind.as_str() {
        "delete" | "mark_as_sensitive" => {
            crate::moderation::moderation_action::save(
                &state,
                auth.account_id,
                id,
                &kind,
                form.text,
                send_email_notification,
            )
            .await?;
        }
        "silence" | "suspend" => {
            let target = sqlx::query_as!(
                models::Account,
                "SELECT * FROM accounts WHERE id = $1",
                report.target_account_id
            )
            .fetch_one(&state.db)
            .await?;
            crate::moderation::account_action::save(
                &state,
                auth.account_id,
                &target,
                crate::moderation::account_action::AccountAction {
                    kind: Some(kind),
                    report_id: Some(id),
                    text: form.text,
                    send_email_notification,
                    ..Default::default()
                },
            )
            .await?;
        }
        // `admin.reports.unknown_action_msg`.
        other => return Err(AppError::Unprocessable(format!("Unknown action: {other}"))),
    }
    render(&state, id, auth.account_id).await
}
