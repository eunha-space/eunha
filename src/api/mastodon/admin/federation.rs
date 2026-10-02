//! `Api::V1::Admin::DomainBlocksController` and `DomainAllowsController`.

use axum::{
    extract::{Extension, Path},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::{FlexBool, Params};
use super::{perm, require_permission, sha256_hex, PageParams};
use crate::{
    db::models::domain_severity,
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::action_log::{self, Target},
    state::AppState,
};

/// `LIMIT` and `MAX_LIMIT` of both controllers.
const LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;

/// `REST::Admin::DomainBlockSerializer`.
#[derive(Debug, Serialize)]
pub struct AdminDomainBlock {
    pub id: String,
    pub domain: String,
    pub digest: String,
    pub created_at: String,
    pub severity: &'static str,
    pub reject_media: bool,
    pub reject_reports: bool,
    pub private_comment: Option<String>,
    pub public_comment: Option<String>,
    pub obfuscate: bool,
}

/// `REST::Admin::DomainAllowSerializer`.
#[derive(Debug, Serialize)]
pub struct AdminDomainAllow {
    pub id: String,
    pub domain: String,
    pub created_at: String,
}

struct BlockRow {
    id: i64,
    domain: String,
    severity: Option<i32>,
    reject_media: bool,
    reject_reports: bool,
    private_comment: Option<String>,
    public_comment: Option<String>,
    obfuscate: bool,
    created_at: chrono::NaiveDateTime,
}

impl From<BlockRow> for AdminDomainBlock {
    fn from(r: BlockRow) -> Self {
        Self {
            id: r.id.to_string(),
            digest: sha256_hex(&r.domain),
            domain: r.domain,
            created_at: super::super::convert::mastodon_date(r.created_at),
            severity: domain_severity::to_str(r.severity),
            reject_media: r.reject_media,
            reject_reports: r.reject_reports,
            private_comment: r.private_comment,
            public_comment: r.public_comment,
            obfuscate: r.obfuscate,
        }
    }
}

async fn find_block(state: &AppState, id: i64) -> AppResult<BlockRow> {
    sqlx::query_as!(
        BlockRow,
        r#"SELECT id, domain, severity, reject_media, reject_reports, private_comment,
                  public_comment, obfuscate, created_at
           FROM domain_blocks WHERE id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

/// `TagManager#normalize_domain` (`DomainNormalizable`): stripped, without
/// trailing slashes, lowercased and in its ASCII form.
pub(super) fn normalize_domain(domain: &str) -> Option<String> {
    let domain = domain.trim().trim_end_matches('/').to_lowercase();
    if domain.is_empty() {
        return None;
    }
    url::Url::parse(&format!("https://{domain}/"))
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
}

fn with_links<'a, T: Serialize>(
    req_headers: &HeaderMap,
    uri: &Uri,
    ids: &'a [(i64, T)],
) -> (HeaderMap, Json<Vec<&'a T>>) {
    let first = ids.first().map(|(id, _)| id.to_string());
    let last = ids.last().map(|(id, _)| id.to_string());
    let bounds = first.as_deref().zip(last.as_deref());
    (
        super::super::link_headers(req_headers, uri, bounds),
        Json(ids.iter().map(|(_, v)| v).collect()),
    )
}

// ── GET /api/v1/admin/domain_blocks ──────────────────────────────────────

pub async fn list_domain_blocks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Params(page): Params<PageParams>,
) -> AppResult<Response> {
    auth.require_scope("admin:read:domain_blocks")?;
    require_permission(&state, auth.account_id, perm::MANAGE_FEDERATION).await?;
    let min_id = page.min_id.map(|i| i.0);
    let mut rows = sqlx::query_as!(
        BlockRow,
        r#"SELECT id, domain, severity, reject_media, reject_reports, private_comment,
                  public_comment, obfuscate, created_at
           FROM domain_blocks
           WHERE ($1::bigint IS NULL OR id < $1)
             AND ($2::bigint IS NULL OR id > $2)
             AND ($3::bigint IS NULL OR id > $3)
           ORDER BY CASE WHEN $3::bigint IS NULL THEN -id ELSE id END
           LIMIT $4"#,
        page.max_id.map(|i| i.0),
        page.since_id.map(|i| i.0),
        min_id,
        page.limit(LIMIT, MAX_LIMIT),
    )
    .fetch_all(&state.db)
    .await?;
    if min_id.is_some() {
        rows.reverse();
    }
    let items: Vec<(i64, AdminDomainBlock)> = rows.into_iter().map(|r| (r.id, r.into())).collect();
    Ok(with_links(&req_headers, &uri, &items).into_response())
}

// ── GET /api/v1/admin/domain_blocks/:id ──────────────────────────────────

pub async fn get_admin_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminDomainBlock>> {
    auth.require_scope("admin:read:domain_blocks")?;
    let row = find_block(&state, id).await?;
    require_permission(&state, auth.account_id, perm::MANAGE_FEDERATION).await?;
    Ok(Json(row.into()))
}

// ── POST /api/v1/admin/domain_blocks ─────────────────────────────────────

/// `enum :severity, ..., validate: true`: an unknown value fails validation.
fn parse_severity(s: &str) -> AppResult<i32> {
    domain_severity::parse(s).ok_or_else(|| {
        AppError::Unprocessable("Validation failed: Severity is not included in the list".into())
    })
}

#[derive(Debug, Deserialize)]
pub struct DomainBlockForm {
    pub domain: Option<String>,
    pub severity: Option<String>,
    pub reject_media: Option<FlexBool>,
    pub reject_reports: Option<FlexBool>,
    pub private_comment: Option<String>,
    pub public_comment: Option<String>,
    pub obfuscate: Option<FlexBool>,
}

/// `DomainBlock#stricter_than?`.
fn stricter_than(
    severity: i32,
    reject_media: bool,
    reject_reports: bool,
    other: &crate::federation::moderation::DomainBlock,
) -> bool {
    if severity == domain_severity::SUSPEND {
        return true;
    }
    if other.severity == domain_severity::SUSPEND
        && (severity == domain_severity::SILENCE || severity == domain_severity::NOOP)
    {
        return false;
    }
    if other.severity == domain_severity::SILENCE && severity == domain_severity::NOOP {
        return false;
    }
    (reject_media || !other.reject_media) && (reject_reports || !other.reject_reports)
}

pub async fn create_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<DomainBlockForm>,
) -> AppResult<Response> {
    auth.require_scope("admin:write:domain_blocks")?;
    require_permission(&state, auth.account_id, perm::MANAGE_FEDERATION).await?;

    let severity = parse_severity(form.severity.as_deref().unwrap_or("silence"))?;
    let reject_media = form.reject_media.is_some_and(|b| b.0);
    let reject_reports = form.reject_reports.is_some_and(|b| b.0);
    let domain = form
        .domain
        .as_deref()
        .and_then(normalize_domain)
        .ok_or_else(|| {
            AppError::Unprocessable("Validation failed: Domain can't be blank".into())
        })?;

    // `conflicts_with_existing_block?`: the same domain, or a parent block this
    // one would not be stricter than.
    if let Some(existing) = crate::federation::moderation::lookup(&state, &domain).await {
        let existing_row = sqlx::query_as!(
            BlockRow,
            r#"SELECT id, domain, severity, reject_media, reject_reports, private_comment,
                      public_comment, obfuscate, created_at
               FROM domain_blocks
               WHERE domain <> '' AND ($1 = domain OR $1 LIKE '%.' || domain)
               ORDER BY char_length(domain) DESC LIMIT 1"#,
            domain,
        )
        .fetch_one(&state.db)
        .await?;
        if existing_row.domain == domain
            || !stricter_than(severity, reject_media, reject_reports, &existing)
        {
            // `REST::Admin::ExistingDomainBlockErrorSerializer`.
            let error = format!(
                "You have already imposed stricter limits on {}.",
                existing_row.domain
            );
            let body = serde_json::json!({
                "error": error,
                "existing_domain_block": AdminDomainBlock::from(existing_row),
            });
            return Ok((StatusCode::UNPROCESSABLE_ENTITY, Json(body)).into_response());
        }
    }

    let id = sqlx::query_scalar!(
        r#"INSERT INTO domain_blocks
             (domain, severity, reject_media, reject_reports, private_comment, public_comment,
              obfuscate, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, now(), now())
           RETURNING id"#,
        domain,
        severity,
        reject_media,
        reject_reports,
        form.private_comment,
        form.public_comment,
        form.obfuscate.is_some_and(|b| b.0),
    )
    .fetch_one(&state.db)
    .await?;
    spawn_block(&state, id, false).await;
    action_log::log(
        &state.db,
        auth.account_id,
        "create",
        &Target::domain_block(id, &domain),
    )
    .await?;
    Ok(Json(AdminDomainBlock::from(find_block(&state, id).await?)).into_response())
}

/// `DomainBlockWorker.perform_async`; run in place under the tests'
/// synchronous switch, so they see the block take effect.
async fn spawn_block(state: &AppState, id: i64, update: bool) {
    if crate::feed::sync_fanout() {
        if let Err(error) = crate::moderation::domain_block::block(state, id, update).await {
            tracing::warn!(domain_block = id, %error, "BlockDomainService failed");
        }
    } else {
        crate::jobs::push(
            state,
            crate::moderation::domain_block::DomainBlockWorker {
                domain_block_id: id,
                update,
            },
        )
        .await;
    }
}

/// `AfterUnallowDomainWorker.perform_async`, run in place under the tests'
/// synchronous switch as [`spawn_block`] is.
async fn spawn_after_unallow(state: &AppState, domain: String) {
    if crate::feed::sync_fanout() {
        if let Err(error) = crate::moderation::domain_block::after_unallow(state, &domain).await {
            tracing::warn!(domain, %error, "AfterUnallowDomainService failed");
        }
    } else {
        crate::jobs::push(
            state,
            crate::moderation::domain_block::AfterUnallowDomainWorker { domain },
        )
        .await;
    }
}

// ── PATCH /api/v1/admin/domain_blocks/:id ────────────────────────────────

pub async fn update_admin_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<DomainBlockForm>,
) -> AppResult<Json<AdminDomainBlock>> {
    auth.require_scope("admin:write:domain_blocks")?;
    let before = find_block(&state, id).await?;
    require_permission(&state, auth.account_id, perm::MANAGE_FEDERATION).await?;
    let severity = form.severity.as_deref().map(parse_severity).transpose()?;
    sqlx::query!(
        r#"UPDATE domain_blocks SET
               severity        = COALESCE($2, severity),
               reject_media    = COALESCE($3, reject_media),
               reject_reports  = COALESCE($4, reject_reports),
               private_comment = CASE WHEN $8 THEN $5 ELSE private_comment END,
               public_comment  = CASE WHEN $9 THEN $6 ELSE public_comment END,
               obfuscate       = COALESCE($7, obfuscate),
               updated_at      = now()
           WHERE id = $1"#,
        id,
        severity,
        form.reject_media.map(|b| b.0),
        form.reject_reports.map(|b| b.0),
        form.private_comment,
        form.public_comment,
        form.obfuscate.map(|b| b.0),
        form.private_comment.is_some(),
        form.public_comment.is_some(),
    )
    .execute(&state.db)
    .await?;
    // `severity_previously_changed?`
    let changed = severity.is_some_and(|s| Some(s) != before.severity);
    spawn_block(&state, id, changed).await;
    action_log::log(
        &state.db,
        auth.account_id,
        "update",
        &Target::domain_block(id, &before.domain),
    )
    .await?;
    Ok(Json(find_block(&state, id).await?.into()))
}

// ── DELETE /api/v1/admin/domain_blocks/:id ───────────────────────────────

pub async fn delete_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("admin:write:domain_blocks")?;
    let block = find_block(&state, id).await?;
    require_permission(&state, auth.account_id, perm::MANAGE_FEDERATION).await?;
    crate::moderation::domain_block::unblock(&state, id)
        .await
        .map_err(AppError::Internal)?;
    action_log::log(
        &state.db,
        auth.account_id,
        "destroy",
        &Target::domain_block(id, &block.domain),
    )
    .await?;
    Ok(Json(serde_json::json!({})))
}

// ── /api/v1/admin/domain_allows ──────────────────────────────────────────

struct AllowRow {
    id: i64,
    domain: String,
    created_at: chrono::NaiveDateTime,
}

impl From<AllowRow> for AdminDomainAllow {
    fn from(r: AllowRow) -> Self {
        Self {
            id: r.id.to_string(),
            domain: r.domain,
            created_at: super::super::convert::mastodon_date(r.created_at),
        }
    }
}

async fn find_allow(state: &AppState, id: i64) -> AppResult<AllowRow> {
    sqlx::query_as!(
        AllowRow,
        "SELECT id, domain, created_at FROM domain_allows WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

pub async fn list_domain_allows(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Params(page): Params<PageParams>,
) -> AppResult<Response> {
    auth.require_scope("admin:read:domain_allows")?;
    require_permission(&state, auth.account_id, perm::MANAGE_FEDERATION).await?;
    let min_id = page.min_id.map(|i| i.0);
    let mut rows = sqlx::query_as!(
        AllowRow,
        r#"SELECT id, domain, created_at FROM domain_allows
           WHERE ($1::bigint IS NULL OR id < $1)
             AND ($2::bigint IS NULL OR id > $2)
             AND ($3::bigint IS NULL OR id > $3)
           ORDER BY CASE WHEN $3::bigint IS NULL THEN -id ELSE id END
           LIMIT $4"#,
        page.max_id.map(|i| i.0),
        page.since_id.map(|i| i.0),
        min_id,
        page.limit(LIMIT, MAX_LIMIT),
    )
    .fetch_all(&state.db)
    .await?;
    if min_id.is_some() {
        rows.reverse();
    }
    let items: Vec<(i64, AdminDomainAllow)> = rows.into_iter().map(|r| (r.id, r.into())).collect();
    Ok(with_links(&req_headers, &uri, &items).into_response())
}

pub async fn get_domain_allow(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminDomainAllow>> {
    auth.require_scope("admin:read:domain_allows")?;
    let row = find_allow(&state, id).await?;
    require_permission(&state, auth.account_id, perm::MANAGE_FEDERATION).await?;
    Ok(Json(row.into()))
}

#[derive(Debug, Deserialize)]
pub struct DomainAllowForm {
    pub domain: Option<String>,
}

pub async fn create_domain_allow(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<DomainAllowForm>,
) -> AppResult<Json<AdminDomainAllow>> {
    auth.require_scope("admin:write:domain_allows")?;
    require_permission(&state, auth.account_id, perm::MANAGE_FEDERATION).await?;
    let domain = form
        .domain
        .as_deref()
        .and_then(normalize_domain)
        .ok_or_else(|| {
            AppError::Unprocessable("Validation failed: Domain can't be blank".into())
        })?;
    // An existing allow is returned as it is, and not logged again.
    if let Some(existing) = sqlx::query_as!(
        AllowRow,
        "SELECT id, domain, created_at FROM domain_allows WHERE domain = $1",
        domain
    )
    .fetch_optional(&state.db)
    .await?
    {
        return Ok(Json(existing.into()));
    }
    let row = sqlx::query_as!(
        AllowRow,
        r#"INSERT INTO domain_allows (domain, created_at, updated_at) VALUES ($1, now(), now())
           RETURNING id, domain, created_at"#,
        domain,
    )
    .fetch_one(&state.db)
    .await?;
    action_log::log(
        &state.db,
        auth.account_id,
        "create",
        &Target::domain_allow(row.id, &row.domain),
    )
    .await?;
    Ok(Json(row.into()))
}

/// `UnallowDomainService`: in limited federation mode the domain's accounts
/// are suspended, and deleted afterwards (`AfterUnallowDomainWorker`);
/// otherwise the allow just goes.
pub async fn delete_domain_allow(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("admin:write:domain_allows")?;
    let row = find_allow(&state, id).await?;
    require_permission(&state, auth.account_id, perm::MANAGE_FEDERATION).await?;
    let limited = state.instance.limited_federation_mode;
    if limited {
        crate::moderation::domain_block::suspend_unallowed(&state, &row.domain)
            .await
            .map_err(AppError::Internal)?;
    }
    sqlx::query!("DELETE FROM domain_allows WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    if limited {
        spawn_after_unallow(&state, row.domain.clone()).await;
    }
    action_log::log(
        &state.db,
        auth.account_id,
        "destroy",
        &Target::domain_allow(id, &row.domain),
    )
    .await?;
    Ok(Json(serde_json::json!({})))
}
