//! `Api::V1::Admin::IpBlocksController`, `EmailDomainBlocksController` and
//! `CanonicalEmailBlocksController`.

use axum::{
    extract::{Extension, Path},
    http::{HeaderMap, Uri},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::{FlexBool, FlexId, Params};
use super::{perm, require_permission, PageParams};
use crate::{
    db::models::ip_severity,
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::action_log::{self, Target},
    state::AppState,
};

/// `LIMIT` of all three controllers, at most twice it per page.
const LIMIT: i64 = 100;

fn page<T: Serialize>(
    req_headers: &HeaderMap,
    uri: &Uri,
    mut items: Vec<(i64, T)>,
    ascending: bool,
) -> Response {
    if ascending {
        items.reverse();
    }
    let first = items.first().map(|(id, _)| id.to_string());
    let last = items.last().map(|(id, _)| id.to_string());
    let headers =
        super::super::link_headers(req_headers, uri, first.as_deref().zip(last.as_deref()));
    let body: Vec<T> = items.into_iter().map(|(_, v)| v).collect();
    (headers, Json(body)).into_response()
}

fn taken(field: &str) -> AppError {
    AppError::Unprocessable(format!("Validation failed: {field} has already been taken"))
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some("23505"))
}

// ── IP blocks ─────────────────────────────────────────────────────────────

/// `REST::Admin::IpBlockSerializer`.
#[derive(Debug, Serialize)]
pub struct AdminIpBlock {
    pub id: String,
    pub ip: String,
    pub severity: &'static str,
    pub comment: String,
    pub created_at: String,
    pub expires_at: Option<String>,
}

struct IpRow {
    id: i64,
    cidr: String,
    severity: i32,
    comment: String,
    created_at: chrono::NaiveDateTime,
    expires_at: Option<chrono::NaiveDateTime>,
}

impl From<IpRow> for AdminIpBlock {
    fn from(r: IpRow) -> Self {
        Self {
            id: r.id.to_string(),
            ip: r.cidr,
            severity: ip_severity::to_str(r.severity),
            comment: r.comment,
            created_at: super::super::convert::mastodon_date(r.created_at),
            expires_at: r.expires_at.map(super::super::convert::mastodon_date),
        }
    }
}

/// The row, with `IpBlock#to_cidr` as `cidr`.
async fn find_ip(state: &AppState, id: i64) -> AppResult<IpRow> {
    sqlx::query_as!(
        IpRow,
        r#"SELECT id, host(ip) || '/' || masklen(ip) AS "cidr!", severity, comment, created_at, expires_at
           FROM ip_blocks WHERE id = $1"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

pub async fn list_ip_blocks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Params(p): Params<PageParams>,
) -> AppResult<Response> {
    auth.require_scope("admin:read:ip_blocks")?;
    require_permission(&state, auth.account_id, perm::MANAGE_BLOCKS).await?;
    let rows = sqlx::query_as!(
        IpRow,
        r#"SELECT id, host(ip) || '/' || masklen(ip) AS "cidr!", severity, comment, created_at, expires_at
           FROM ip_blocks
           WHERE ($1::bigint IS NULL OR id < $1) AND ($2::bigint IS NULL OR id > $2)
             AND ($3::bigint IS NULL OR id > $3)
           ORDER BY CASE WHEN $3::bigint IS NULL THEN -id ELSE id END LIMIT $4"#,
        p.max_id.map(|i| i.0),
        p.since_id.map(|i| i.0),
        p.min_id.map(|i| i.0),
        p.limit(LIMIT, LIMIT * 2),
    )
    .fetch_all(&state.db)
    .await?;
    Ok(page(
        &req_headers,
        &uri,
        rows.into_iter()
            .map(|r| (r.id, AdminIpBlock::from(r)))
            .collect(),
        p.min_id.is_some(),
    ))
}

pub async fn get_ip_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminIpBlock>> {
    auth.require_scope("admin:read:ip_blocks")?;
    let row = find_ip(&state, id).await?;
    require_permission(&state, auth.account_id, perm::MANAGE_BLOCKS).await?;
    Ok(Json(row.into()))
}

#[derive(Debug, Deserialize)]
pub struct IpBlockForm {
    pub ip: Option<String>,
    pub severity: Option<String>,
    pub comment: Option<String>,
    pub expires_in: Option<FlexId>,
}

/// `validates :severity, presence: true` and the validated enum.
fn parse_ip_severity(s: Option<&str>) -> AppResult<i32> {
    match s.filter(|s| !s.is_empty()) {
        None => Err(AppError::Unprocessable(
            "Validation failed: Severity can't be blank".into(),
        )),
        Some(s) => ip_severity::parse(s).ok_or_else(|| {
            AppError::Unprocessable(
                "Validation failed: Severity is not included in the list".into(),
            )
        }),
    }
}

fn parse_ip(s: Option<&str>) -> AppResult<String> {
    let s = s
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::Unprocessable("Validation failed: Ip can't be blank".into()))?;
    crate::remote_ip::Cidr::parse(s)
        .map(|c| format!("{}/{}", c.addr, c.prefix))
        .ok_or_else(|| AppError::Unprocessable("Validation failed: Ip is invalid".into()))
}

/// `Expireable#expires_in=`: seconds from now, none when not given.
fn expires_at(expires_in: Option<FlexId>) -> Option<chrono::NaiveDateTime> {
    expires_in.map(|s| chrono::Utc::now().naive_utc() + chrono::Duration::seconds(s.0))
}

pub async fn create_ip_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<IpBlockForm>,
) -> AppResult<Json<AdminIpBlock>> {
    auth.require_scope("admin:write:ip_blocks")?;
    require_permission(&state, auth.account_id, perm::MANAGE_BLOCKS).await?;
    let ip = parse_ip(form.ip.as_deref())?;
    let severity = parse_ip_severity(form.severity.as_deref())?;
    let id = sqlx::query_scalar!(
        r#"INSERT INTO ip_blocks (ip, severity, comment, expires_at, created_at, updated_at)
           VALUES ($1::text::inet, $2, $3, $4, now(), now()) RETURNING id"#,
        ip,
        severity,
        form.comment.unwrap_or_default(),
        expires_at(form.expires_in),
    )
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            taken("Ip")
        } else {
            e.into()
        }
    })?;
    let row = find_ip(&state, id).await?;
    action_log::log(
        &state.db,
        auth.account_id,
        "create",
        &Target::ip_block(id, &row.cidr),
    )
    .await?;
    Ok(Json(row.into()))
}

pub async fn update_ip_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<IpBlockForm>,
) -> AppResult<Json<AdminIpBlock>> {
    auth.require_scope("admin:write:ip_blocks")?;
    find_ip(&state, id).await?;
    require_permission(&state, auth.account_id, perm::MANAGE_BLOCKS).await?;
    // `update!(resource_params)`: what is given changes, and is validated.
    let ip = form
        .ip
        .as_deref()
        .map(|ip| parse_ip(Some(ip)))
        .transpose()?;
    let severity = form
        .severity
        .as_deref()
        .map(|s| parse_ip_severity(Some(s)))
        .transpose()?;
    sqlx::query!(
        r#"UPDATE ip_blocks SET
             ip = COALESCE($2::text::inet, ip),
             severity = COALESCE($3, severity),
             comment = COALESCE($4, comment),
             expires_at = CASE WHEN $6 THEN $5 ELSE expires_at END,
             updated_at = now()
           WHERE id = $1"#,
        id,
        ip,
        severity,
        form.comment,
        expires_at(form.expires_in),
        form.expires_in.is_some(),
    )
    .execute(&state.db)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            taken("Ip")
        } else {
            e.into()
        }
    })?;
    let row = find_ip(&state, id).await?;
    action_log::log(
        &state.db,
        auth.account_id,
        "update",
        &Target::ip_block(id, &row.cidr),
    )
    .await?;
    Ok(Json(row.into()))
}

pub async fn delete_ip_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("admin:write:ip_blocks")?;
    let row = find_ip(&state, id).await?;
    require_permission(&state, auth.account_id, perm::MANAGE_BLOCKS).await?;
    sqlx::query!("DELETE FROM ip_blocks WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    action_log::log(
        &state.db,
        auth.account_id,
        "destroy",
        &Target::ip_block(id, &row.cidr),
    )
    .await?;
    Ok(Json(serde_json::json!({})))
}

// ── Email domain blocks ───────────────────────────────────────────────────

/// `REST::Admin::EmailDomainBlockSerializer`.
#[derive(Debug, Serialize)]
pub struct AdminEmailDomainBlock {
    pub id: String,
    pub domain: String,
    pub created_at: String,
    pub history: serde_json::Value,
    pub allow_with_approval: bool,
}

struct EmailRow {
    id: i64,
    domain: String,
    allow_with_approval: bool,
    created_at: chrono::NaiveDateTime,
}

async fn email_entity(state: &AppState, r: EmailRow) -> AdminEmailDomainBlock {
    AdminEmailDomainBlock {
        id: r.id.to_string(),
        history: crate::moderation::history::as_json(state, "email_domain_blocks", r.id).await,
        domain: r.domain,
        created_at: super::super::convert::mastodon_date(r.created_at),
        allow_with_approval: r.allow_with_approval,
    }
}

async fn find_email(state: &AppState, id: i64) -> AppResult<EmailRow> {
    sqlx::query_as!(
        EmailRow,
        "SELECT id, domain, allow_with_approval, created_at FROM email_domain_blocks WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

pub async fn list_email_domain_blocks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Params(p): Params<PageParams>,
) -> AppResult<Response> {
    auth.require_scope("admin:read:email_domain_blocks")?;
    require_permission(&state, auth.account_id, perm::MANAGE_BLOCKS).await?;
    let rows = sqlx::query_as!(
        EmailRow,
        r#"SELECT id, domain, allow_with_approval, created_at FROM email_domain_blocks
           WHERE ($1::bigint IS NULL OR id < $1) AND ($2::bigint IS NULL OR id > $2)
             AND ($3::bigint IS NULL OR id > $3)
           ORDER BY CASE WHEN $3::bigint IS NULL THEN -id ELSE id END LIMIT $4"#,
        p.max_id.map(|i| i.0),
        p.since_id.map(|i| i.0),
        p.min_id.map(|i| i.0),
        p.limit(LIMIT, LIMIT * 2),
    )
    .fetch_all(&state.db)
    .await?;
    let mut items = vec![];
    for r in rows {
        items.push((r.id, email_entity(&state, r).await));
    }
    Ok(page(&req_headers, &uri, items, p.min_id.is_some()))
}

pub async fn get_email_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminEmailDomainBlock>> {
    auth.require_scope("admin:read:email_domain_blocks")?;
    let row = find_email(&state, id).await?;
    require_permission(&state, auth.account_id, perm::MANAGE_BLOCKS).await?;
    Ok(Json(email_entity(&state, row).await))
}

#[derive(Debug, Deserialize)]
pub struct EmailDomainBlockForm {
    pub domain: Option<String>,
    pub allow_with_approval: Option<FlexBool>,
}

pub async fn create_email_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<EmailDomainBlockForm>,
) -> AppResult<Json<AdminEmailDomainBlock>> {
    auth.require_scope("admin:write:email_domain_blocks")?;
    require_permission(&state, auth.account_id, perm::MANAGE_BLOCKS).await?;
    // `DomainNormalizable`, then presence and `domain: true`.
    let domain = form
        .domain
        .as_deref()
        .and_then(|d| crate::moderation::signup::email_domain(&format!("x@{d}")))
        .ok_or_else(|| {
            AppError::Unprocessable("Validation failed: Domain can't be blank".into())
        })?;
    let id = sqlx::query_scalar!(
        r#"INSERT INTO email_domain_blocks (domain, allow_with_approval, created_at, updated_at)
           VALUES ($1, $2, now(), now()) RETURNING id"#,
        domain,
        form.allow_with_approval.is_some_and(|b| b.0),
    )
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            taken("Domain")
        } else {
            e.into()
        }
    })?;
    action_log::log(
        &state.db,
        auth.account_id,
        "create",
        &Target::email_domain_block(id, &domain),
    )
    .await?;
    let row = find_email(&state, id).await?;
    Ok(Json(email_entity(&state, row).await))
}

pub async fn delete_email_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("admin:write:email_domain_blocks")?;
    let row = find_email(&state, id).await?;
    require_permission(&state, auth.account_id, perm::MANAGE_BLOCKS).await?;
    // `has_many :children, dependent: :destroy`.
    sqlx::query!(
        "DELETE FROM email_domain_blocks WHERE id = $1 OR parent_id = $1",
        id
    )
    .execute(&state.db)
    .await?;
    action_log::log(
        &state.db,
        auth.account_id,
        "destroy",
        &Target::email_domain_block(id, &row.domain),
    )
    .await?;
    Ok(Json(serde_json::json!({})))
}

// ── Canonical email blocks ────────────────────────────────────────────────

/// `REST::Admin::CanonicalEmailBlockSerializer`.
#[derive(Debug, Serialize)]
pub struct CanonicalEmailBlock {
    pub id: String,
    pub canonical_email_hash: String,
}

struct CanonicalRow {
    id: i64,
    canonical_email_hash: String,
}

impl From<CanonicalRow> for CanonicalEmailBlock {
    fn from(r: CanonicalRow) -> Self {
        Self {
            id: r.id.to_string(),
            canonical_email_hash: r.canonical_email_hash,
        }
    }
}

/// `CanonicalEmailBlock.digest(canonicalize_email(email))`.
fn canonical_hash(email: &str) -> String {
    super::sha256_hex(&crate::moderation::signup::canonicalize_email(email))
}

async fn find_canonical(state: &AppState, id: i64) -> AppResult<CanonicalRow> {
    sqlx::query_as!(
        CanonicalRow,
        "SELECT id, canonical_email_hash FROM canonical_email_blocks WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

pub async fn list_canonical_email_blocks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Params(p): Params<PageParams>,
) -> AppResult<Response> {
    auth.require_scope("admin:read:canonical_email_blocks")?;
    require_permission(&state, auth.account_id, perm::MANAGE_BLOCKS).await?;
    let rows = sqlx::query_as!(
        CanonicalRow,
        r#"SELECT id, canonical_email_hash FROM canonical_email_blocks
           WHERE ($1::bigint IS NULL OR id < $1) AND ($2::bigint IS NULL OR id > $2)
             AND ($3::bigint IS NULL OR id > $3)
           ORDER BY CASE WHEN $3::bigint IS NULL THEN -id ELSE id END LIMIT $4"#,
        p.max_id.map(|i| i.0),
        p.since_id.map(|i| i.0),
        p.min_id.map(|i| i.0),
        p.limit(LIMIT, LIMIT * 2),
    )
    .fetch_all(&state.db)
    .await?;
    Ok(page(
        &req_headers,
        &uri,
        rows.into_iter()
            .map(|r| (r.id, CanonicalEmailBlock::from(r)))
            .collect(),
        p.min_id.is_some(),
    ))
}

pub async fn get_canonical_email_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<CanonicalEmailBlock>> {
    auth.require_scope("admin:read:canonical_email_blocks")?;
    let row = find_canonical(&state, id).await?;
    require_permission(&state, auth.account_id, perm::MANAGE_BLOCKS).await?;
    Ok(Json(row.into()))
}

#[derive(Debug, Deserialize)]
pub struct CanonicalEmailBlockForm {
    pub email: Option<String>,
    pub canonical_email_hash: Option<String>,
}

pub async fn create_canonical_email_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<CanonicalEmailBlockForm>,
) -> AppResult<Json<CanonicalEmailBlock>> {
    auth.require_scope("admin:write:canonical_email_blocks")?;
    require_permission(&state, auth.account_id, perm::MANAGE_BLOCKS).await?;
    // `email=` sets the hash; otherwise the hash as given.
    let hash = match (form.email.as_deref(), form.canonical_email_hash.as_deref()) {
        (Some(email), _) if !email.is_empty() => canonical_hash(email),
        (_, Some(hash)) if !hash.is_empty() => hash.to_owned(),
        _ => {
            return Err(AppError::Unprocessable(
                "Validation failed: Canonical email hash can't be blank".into(),
            ))
        }
    };
    let id = sqlx::query_scalar!(
        r#"INSERT INTO canonical_email_blocks (canonical_email_hash, created_at, updated_at)
           VALUES ($1, now(), now()) RETURNING id"#,
        hash,
    )
    .fetch_one(&state.db)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            taken("Canonical email hash")
        } else {
            e.into()
        }
    })?;
    action_log::log(
        &state.db,
        auth.account_id,
        "create",
        &Target::canonical_email_block(id, &hash),
    )
    .await?;
    Ok(Json(find_canonical(&state, id).await?.into()))
}

pub async fn delete_canonical_email_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("admin:write:canonical_email_blocks")?;
    let row = find_canonical(&state, id).await?;
    require_permission(&state, auth.account_id, perm::MANAGE_BLOCKS).await?;
    sqlx::query!("DELETE FROM canonical_email_blocks WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    action_log::log(
        &state.db,
        auth.account_id,
        "destroy",
        &Target::canonical_email_block(id, &row.canonical_email_hash),
    )
    .await?;
    Ok(Json(serde_json::json!({})))
}

#[derive(Debug, Deserialize)]
pub struct CanonicalTestForm {
    pub email: Option<String>,
}

/// `test`: the blocks an email matches, `params.require(:email)`.
pub async fn test_canonical_email_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<CanonicalTestForm>,
) -> AppResult<Json<Vec<CanonicalEmailBlock>>> {
    auth.require_scope("admin:read:canonical_email_blocks")?;
    require_permission(&state, auth.account_id, perm::MANAGE_BLOCKS).await?;
    let email = form.email.filter(|e| !e.is_empty()).ok_or_else(|| {
        AppError::BadRequest("param is missing or the value is empty: email".into())
    })?;
    let rows = sqlx::query_as!(
        CanonicalRow,
        "SELECT id, canonical_email_hash FROM canonical_email_blocks WHERE canonical_email_hash = $1",
        canonical_hash(&email),
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}
