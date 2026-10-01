use super::accounts::{batch_account_roles, fetch_account_emojis};
use super::convert::account_from_db;
use super::extractors::QueryOrJson;
use super::types::Account as ApiAccount;
use crate::{
    db::models,
    error::{AppError, AppResult},
    middleware::{AuthenticatedUser, ResolvedInstance},
    state::AppState,
};
use axum::{
    extract::{Extension, Multipart, Path, Query},
    http::{HeaderMap, StatusCode, Uri},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

// ── Admin auth guard ──────────────────────────────────────────────────────

/// Mastodon UserRole permission flags (subset used for permission gating).
pub mod perm {
    pub const ADMINISTRATOR: i64 = 1 << 0;
    pub const MANAGE_REPORTS: i64 = 1 << 4;
    pub const MANAGE_FEDERATION: i64 = 1 << 5;
    pub const MANAGE_TAXONOMIES: i64 = 1 << 8;
    pub const MANAGE_USERS: i64 = 1 << 10;
    pub const MANAGE_INVITES: i64 = 1 << 11;
    pub const MANAGE_CUSTOM_EMOJIS: i64 = 1 << 14;
    pub const INVITE_USERS: i64 = 1 << 16;
    pub const MANAGE_ROLES: i64 = 1 << 17;
    pub const INVITE_BYPASS_APPROVAL: i64 = 1 << 21;

    /// Mastodon `UserRole::Flags::ALL`: every flag it defines, the last of
    /// which in 4.7.0 is `manage_email_subscriptions` (1 << 22).
    pub const ALL: i64 = (1 << 23) - 1;
}

/// Mastodon's `UserRole#computed_permissions`, as `(position, permissions)`.
///
/// A role grants the union of its own permissions with the everyone role's, and
/// the `administrator` flag grants everything. A user with no role of its own
/// has exactly the everyone role — `User#role` falls back to it rather than to
/// nothing — so the everyone row (`EVERYONE_ROLE_ID`, seeded by migration 009
/// with `invite_users`) is what an instance edits to decide what an ordinary
/// member may do. Upstream will only let it hold `Flags::SAFE`
/// (`invite_users | invite_bypass_approval`); eunha has no role editor, so that
/// row is edited by hand and nothing enforces it here.
pub(super) async fn computed_permissions(
    state: &AppState,
    account_id: i64,
) -> AppResult<(i32, i64)> {
    let row = sqlx::query!(
        r#"SELECT COALESCE(own.position, 0) AS "pos!: i32",
                  COALESCE(own.permissions, 0) AS "own!: i64",
                  COALESCE(everyone.permissions, 0) AS "everyone!: i64"
           FROM users u
           LEFT JOIN user_roles own ON own.id = u.role_id
           LEFT JOIN user_roles everyone ON everyone.id = $2
           WHERE u.account_id = $1"#,
        account_id,
        super::accounts::EVERYONE_ROLE_ID,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::Unauthorized)?;

    // The expansion is keyed on the user's *own* role, matching Mastodon: on
    // the everyone role `computed_permissions` returns early, so an everyone
    // row carrying the flag would grant that one bit rather than everything.
    let permissions = if row.own & perm::ADMINISTRATOR != 0 {
        perm::ALL
    } else {
        row.own | row.everyone
    };
    Ok((row.pos, permissions))
}

/// Authorize by Mastodon permission bit: the `administrator` flag grants
/// everything, otherwise the specific permission is required. eunha's existing
/// position≥100 admin gate is preserved as a superset so current admins keep
/// access — this only *adds* moderator support for roles carrying the flag.
pub(crate) async fn require_permission(
    state: &AppState,
    account_id: i64,
    flag: i64,
) -> AppResult<()> {
    let (pos, perms) = computed_permissions(state, account_id).await?;
    if perms & flag != 0 || pos >= 100 {
        return Ok(());
    }
    Err(AppError::Forbidden)
}

/// Full-admin gate (position≥100 or the administrator flag). Used for endpoints
/// Mastodon restricts to admin-level permissions eunha doesn't split out.
async fn require_admin(state: &AppState, account_id: i64) -> AppResult<()> {
    let (pos, perms) = computed_permissions(state, account_id).await?;
    if pos >= 100 || perms & perm::ADMINISTRATOR != 0 {
        return Ok(());
    }
    Err(AppError::Forbidden)
}

// ── Admin Account type ────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AdminAccount {
    pub id: String,
    pub username: String,
    pub domain: Option<String>,
    pub created_at: String,
    pub email: String,
    pub ip: Option<String>,
    pub ips: Vec<serde_json::Value>,
    pub role: AdminRole,
    pub confirmed: bool,
    pub suspended: bool,
    pub silenced: bool,
    pub sensitized: bool,
    pub disabled: bool,
    pub approved: bool,
    pub locale: Option<String>,
    pub invite_request: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_application_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invited_by_account_id: Option<String>,
    pub account: ApiAccount,
}

#[derive(Debug, Serialize)]
pub struct AdminRole {
    pub id: String,
    pub name: String,
    pub color: String,
    pub position: i32,
    pub permissions: i64,
    pub highlighted: bool,
    pub created_at: String,
    pub updated_at: String,
}

fn role_for(role_str: &str) -> AdminRole {
    match role_str {
        "admin" => AdminRole {
            id: "1".into(),
            name: "Admin".into(),
            color: "#6364ff".into(),
            position: 100,
            permissions: 2031612,
            highlighted: true,
            created_at: "2022-09-08T22:48:07.983Z".into(),
            updated_at: "2022-09-08T22:48:07.983Z".into(),
        },
        "moderator" => AdminRole {
            id: "2".into(),
            name: "Moderator".into(),
            color: "#6364ff".into(),
            position: 10,
            permissions: 1049884,
            highlighted: true,
            created_at: "2022-09-08T22:48:07.983Z".into(),
            updated_at: "2022-09-08T22:48:07.983Z".into(),
        },
        _ => AdminRole {
            id: "0".into(),
            name: "".into(),
            color: "".into(),
            position: 0,
            permissions: 0,
            highlighted: false,
            created_at: "2022-09-08T22:48:07.983Z".into(),
            updated_at: "2022-09-08T22:48:07.983Z".into(),
        },
    }
}

async fn build_admin_account(
    state: &AppState,
    account: &models::Account,
) -> AppResult<AdminAccount> {
    let user = sqlx::query!(
        r#"SELECT u.id, u.email, u.confirmed_at, u.approved,
                  COALESCE(ur.position, 0) AS "role_position!: i32",
                  inv_u.account_id AS "invited_by_account_id?: i64"
           FROM users u
           LEFT JOIN user_roles ur ON ur.id = u.role_id
           LEFT JOIN invites i ON i.id = u.invite_id
           LEFT JOIN users inv_u ON inv_u.id = i.user_id
           WHERE u.account_id = $1"#,
        account.id,
    )
    .fetch_optional(&state.db)
    .await?;

    let (user_id, email, confirmed, approved, reason, role_str, invited_by_account_id) = match user
    {
        Some(u) => {
            let role = if u.role_position >= 1000 {
                "admin"
            } else if u.role_position >= 100 {
                "moderator"
            } else {
                "user"
            };
            (
                Some(u.id),
                u.email,
                u.confirmed_at.is_some(),
                u.approved,
                None::<String>,
                role.to_string(),
                u.invited_by_account_id.map(|id| id.to_string()),
            )
        }
        None => (
            None,
            String::new(),
            true,
            true,
            None,
            "user".to_string(),
            None,
        ),
    };

    // Fetch IP addresses from user_ips view for local accounts
    let ips: Vec<serde_json::Value> = if let Some(uid) = user_id {
        sqlx::query!(
            "SELECT ip::text AS ip, used_at FROM user_ips WHERE user_id = $1 ORDER BY used_at DESC LIMIT 20",
            uid,
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| serde_json::json!({
            "ip": r.ip.unwrap_or_default(),
            "used_at": r.used_at.map(super::convert::mastodon_date),
        }))
        .collect()
    } else {
        vec![]
    };
    let first_ip = ips
        .first()
        .and_then(|v| v["ip"].as_str().map(str::to_string));

    Ok(AdminAccount {
        id: account.id.to_string(),
        username: account.username.clone(),
        domain: account.domain.clone(),
        created_at: super::convert::mastodon_date(account.created_at),
        email,
        ip: first_ip,
        ips,
        role: role_for(&role_str),
        confirmed,
        suspended: account.suspended_at.is_some(),
        silenced: account.silenced_at.is_some(),
        sensitized: account.sensitized_at.is_some(),
        disabled: false,
        approved,
        locale: None,
        invite_request: reason,
        created_by_application_id: None,
        invited_by_account_id,
        account: {
            let mut api = account_from_db(&state.urls, account);
            api.emojis = fetch_account_emojis(state, account).await;
            api.roles = {
                let m = batch_account_roles(state, std::slice::from_ref(account)).await;
                m.get(&account.id).cloned().unwrap_or_default()
            };
            super::accounts::apply_account_stats(state, &mut api, account.id).await;
            api
        },
    })
}

// ── GET /api/v1/admin/accounts ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AdminAccountsParams {
    pub status: Option<String>,
    pub username: Option<String>,
    pub by_domain: Option<String>,
    pub role_ids: Option<Vec<String>>,
    pub limit: Option<i64>,
    pub max_id: Option<String>,
    pub min_id: Option<String>,
    pub since_id: Option<String>,
}

pub async fn list_admin_accounts(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(params): Query<AdminAccountsParams>,
    uri: Uri,
    req_headers: HeaderMap,
) -> AppResult<impl IntoResponse> {
    require_permission(&state, auth.account_id, perm::MANAGE_USERS).await?;

    let limit = params.limit.unwrap_or(40).clamp(1, 80);
    let max_id = params.max_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let min_id = params.min_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let since_id = params
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());

    let accounts = sqlx::query_as!(
        models::Account,
        r#"SELECT a.* FROM accounts a
           WHERE a.domain IS NULL
             AND ($2::text IS NULL OR a.username = $2)
             AND ($3::text IS NULL OR
                  ($3 = 'suspended' AND a.suspended_at IS NOT NULL) OR
                  ($3 = 'silenced' AND a.silenced_at IS NOT NULL) OR
                  ($3 = 'sensitized' AND a.sensitized_at IS NOT NULL) OR
                  ($3 = 'disabled' AND a.suspended_at IS NOT NULL) OR
                  ($3 = 'pending' AND EXISTS (
                      SELECT 1 FROM users u WHERE u.account_id = a.id AND NOT u.approved
                  )) OR
                  ($3 = 'active' AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.silenced_at IS NULL
                      AND NOT EXISTS (SELECT 1 FROM users u WHERE u.account_id = a.id AND NOT u.approved))
             )
             AND ($4::bigint IS NULL OR a.id < $4)
             AND ($5::bigint IS NULL OR a.id > $5)
             AND ($6::bigint IS NULL OR a.id > $6)
           ORDER BY a.id DESC
           LIMIT $1"#,
        limit, params.username.as_deref(), params.status.as_deref(),
        max_id, since_id, min_id,
    )
    .fetch_all(&state.db)
    .await?;

    let mut result = Vec::with_capacity(accounts.len());
    for a in &accounts {
        result.push(build_admin_account(&state, a).await?);
    }

    let bounds = result
        .first()
        .zip(result.last())
        .map(|(n, o)| (n.id.as_str(), o.id.as_str()));
    let resp_headers = super::link_headers(&req_headers, &uri, bounds);
    Ok((resp_headers, Json(result)))
}

// ── GET /api/v2/admin/accounts ────────────────────────────────────────────
// Adds origin (local/remote) and display_name filters on top of v1.

#[derive(Debug, Deserialize)]
pub struct AdminAccountsV2Params {
    pub origin: Option<String>, // "local" | "remote"
    pub status: Option<String>,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub by_domain: Option<String>,
    pub email: Option<String>,
    pub role_ids: Option<Vec<String>>,
    pub limit: Option<i64>,
    pub max_id: Option<String>,
    pub min_id: Option<String>,
    pub since_id: Option<String>,
}

pub async fn list_admin_accounts_v2(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(params): Query<AdminAccountsV2Params>,
    uri: Uri,
    req_headers: HeaderMap,
) -> AppResult<impl IntoResponse> {
    require_permission(&state, auth.account_id, perm::MANAGE_USERS).await?;

    let limit = params.limit.unwrap_or(40).clamp(1, 80);
    let max_id = params.max_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let min_id = params.min_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let since_id = params
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());

    // origin filter: "local" = domain IS NULL, "remote" = domain IS NOT NULL
    let local_only = params.origin.as_deref() == Some("local");
    let remote_only = params.origin.as_deref() == Some("remote");

    let display_name_pattern = params.display_name.as_ref().map(|s| format!("%{}%", s));
    let accounts = sqlx::query_as!(
        models::Account,
        r#"SELECT a.* FROM accounts a
           LEFT JOIN users u ON u.account_id = a.id
           WHERE ($2::boolean IS NOT TRUE OR a.domain IS NULL)
             AND ($3::boolean IS NOT TRUE OR a.domain IS NOT NULL)
             AND ($4::text IS NULL OR a.username ILIKE $4)
             AND ($5::text IS NULL OR a.display_name ILIKE $5)
             AND ($6::text IS NULL OR
                  ($6 = 'suspended' AND a.suspended_at IS NOT NULL) OR
                  ($6 = 'silenced' AND a.silenced_at IS NOT NULL) OR
                  ($6 = 'sensitized' AND a.sensitized_at IS NOT NULL) OR
                  ($6 = 'disabled' AND a.suspended_at IS NOT NULL) OR
                  ($6 = 'pending' AND NOT u.approved AND u.id IS NOT NULL) OR
                  ($6 = 'active' AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.silenced_at IS NULL
                      AND (u.id IS NULL OR u.approved))
             )
             AND ($7::text IS NULL OR (lower(u.email) = lower($7) AND a.domain IS NULL))
             AND ($8::bigint IS NULL OR a.id < $8)
             AND ($9::bigint IS NULL OR a.id > $9)
             AND ($10::bigint IS NULL OR a.id > $10)
           ORDER BY a.id DESC
           LIMIT $1"#,
        limit,
        local_only,
        remote_only,
        params.username.as_deref(),
        display_name_pattern.as_deref(),
        params.status.as_deref(),
        params.email.as_deref(),
        max_id,
        since_id,
        min_id,
    )
    .fetch_all(&state.db)
    .await?;

    let mut result = Vec::with_capacity(accounts.len());
    for a in &accounts {
        result.push(build_admin_account(&state, a).await?);
    }

    let bounds = result
        .first()
        .zip(result.last())
        .map(|(n, o)| (n.id.as_str(), o.id.as_str()));
    let resp_headers = super::link_headers(&req_headers, &uri, bounds);
    Ok((resp_headers, Json(result)))
}

// ── GET /api/v1/admin/accounts/:id ───────────────────────────────────────

pub async fn get_admin_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_permission(&state, auth.account_id, perm::MANAGE_USERS).await?;
    let account = sqlx::query_as!(models::Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(build_admin_account(&state, &account).await?))
}

// ── POST /api/v1/admin/accounts/:id/approve ──────────────────────────────

pub async fn approve_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_permission(&state, auth.account_id, perm::MANAGE_USERS).await?;
    sqlx::query!("UPDATE users SET approved = true WHERE account_id = $1", id,)
        .execute(&state.db)
        .await?;
    let account = sqlx::query_as!(models::Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(build_admin_account(&state, &account).await?))
}

// ── POST /api/v1/admin/accounts/:id/reject ───────────────────────────────

pub async fn reject_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_permission(&state, auth.account_id, perm::MANAGE_USERS).await?;
    // `UserPolicy#reject?` — only a signup still awaiting approval can be
    // rejected; anything else would be a full delete of a live account.
    let pending = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM users u JOIN accounts a ON a.id = u.account_id
             WHERE u.account_id = $1 AND NOT u.approved AND a.domain IS NULL
           ) AS "exists!""#,
        id,
    )
    .fetch_one(&state.db)
    .await?;
    if !pending {
        return Err(AppError::Forbidden);
    }
    // Mastodon: `DeleteAccountService.new.call(@account, reserve_email: false,
    // reserve_username: false)` — a rejected signup leaves nothing behind.
    crate::delete_account::call(&state, id, crate::delete_account::Options::purge()).await?;
    Ok(StatusCode::OK)
}

// ── POST /api/v1/admin/accounts/:id/enable ───────────────────────────────

pub async fn enable_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_permission(&state, auth.account_id, perm::MANAGE_USERS).await?;
    sqlx::query!("UPDATE accounts SET suspended_at = NULL WHERE id = $1", id,)
        .execute(&state.db)
        .await?;
    let account = sqlx::query_as!(models::Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(build_admin_account(&state, &account).await?))
}

// ── POST /api/v1/admin/accounts/:id/silence ──────────────────────────────

pub async fn silence_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_permission(&state, auth.account_id, perm::MANAGE_USERS).await?;
    sqlx::query!(
        "UPDATE accounts SET silenced_at = now() WHERE id = $1 AND silenced_at IS NULL",
        id,
    )
    .execute(&state.db)
    .await?;
    let account = sqlx::query_as!(models::Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(build_admin_account(&state, &account).await?))
}

// ── POST /api/v1/admin/accounts/:id/unsilence ────────────────────────────

pub async fn unsilence_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_permission(&state, auth.account_id, perm::MANAGE_USERS).await?;
    sqlx::query!("UPDATE accounts SET silenced_at = NULL WHERE id = $1", id,)
        .execute(&state.db)
        .await?;
    let account = sqlx::query_as!(models::Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(build_admin_account(&state, &account).await?))
}

// ── POST /api/v1/admin/accounts/:id/suspend ──────────────────────────────

pub async fn suspend_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_permission(&state, auth.account_id, perm::MANAGE_USERS).await?;
    let account = sqlx::query_as!(models::Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if account.suspended_at.is_none() {
        // `Account#suspend!`: records the deletion request that keeps the
        // suspension reversible for 30 days and blocks the email from reuse.
        let origin = if account.is_local() {
            crate::delete_account::suspension_origin::LOCAL
        } else {
            crate::delete_account::suspension_origin::REMOTE
        };
        crate::delete_account::suspend(&state, id, origin, true).await?;
    }
    // Suspension hides the account's content rather than deleting it, so that
    // unsuspending brings everything back (Mastodon `SuspendAccountService`).
    // Data is only removed once the 30-day deletion request comes due.
    let account = sqlx::query_as!(models::Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(build_admin_account(&state, &account).await?))
}

// ── POST /api/v1/admin/accounts/:id/unsuspend ────────────────────────────

pub async fn unsuspend_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_permission(&state, auth.account_id, perm::MANAGE_USERS).await?;
    // `Account#unsuspend!`: drops the deletion request and the email block too.
    crate::delete_account::unsuspend(&state, id).await?;
    let account = sqlx::query_as!(models::Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(build_admin_account(&state, &account).await?))
}

// ── Admin Report type ─────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AdminReport {
    pub id: String,
    pub action_taken: bool,
    pub action_taken_at: Option<String>,
    pub category: String,
    pub comment: String,
    pub forwarded: bool,
    pub created_at: String,
    pub updated_at: String,
    pub account: ApiAccount,
    pub target_account: ApiAccount,
    pub assigned_account: Option<ApiAccount>,
    pub action_taken_by_account: Option<ApiAccount>,
    pub statuses: Vec<serde_json::Value>,
    pub rules: Vec<serde_json::Value>,
}

async fn build_admin_report(state: &AppState, report: &AdminReportRow) -> AppResult<AdminReport> {
    let account = sqlx::query_as!(
        models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        report.account_id,
    )
    .fetch_one(&state.db)
    .await?;
    let target = sqlx::query_as!(
        models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        report.target_account_id,
    )
    .fetch_one(&state.db)
    .await?;

    let mut account_api = account_from_db(&state.urls, &account);
    account_api.emojis = fetch_account_emojis(state, &account).await;
    account_api.roles = {
        let m = batch_account_roles(state, std::slice::from_ref(&account)).await;
        m.get(&account.id).cloned().unwrap_or_default()
    };
    super::accounts::apply_account_stats(state, &mut account_api, account.id).await;
    let mut target_api = account_from_db(&state.urls, &target);
    target_api.emojis = fetch_account_emojis(state, &target).await;
    target_api.roles = {
        let m = batch_account_roles(state, std::slice::from_ref(&target)).await;
        m.get(&target.id).cloned().unwrap_or_default()
    };
    super::accounts::apply_account_stats(state, &mut target_api, target.id).await;
    Ok(AdminReport {
        id: report.id.to_string(),
        action_taken: report.action_taken_at.is_some(),
        action_taken_at: report.action_taken_at.map(super::convert::mastodon_date),
        category: report.category.clone(),
        comment: report.comment.clone(),
        forwarded: report.forwarded.unwrap_or(false),
        created_at: super::convert::mastodon_date(report.created_at),
        updated_at: super::convert::mastodon_date(report.updated_at),
        account: account_api,
        target_account: target_api,
        assigned_account: None,
        action_taken_by_account: None,
        statuses: vec![],
        rules: vec![],
    })
}

struct AdminReportRow {
    id: i64,
    account_id: i64,
    target_account_id: i64,
    comment: String,
    forwarded: Option<bool>,
    category: String,
    action_taken_at: Option<chrono::NaiveDateTime>,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

// ── GET /api/v1/admin/reports ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AdminReportsParams {
    pub resolved: Option<bool>,
    pub limit: Option<i64>,
    pub max_id: Option<String>,
    pub min_id: Option<String>,
    pub since_id: Option<String>,
}

pub async fn list_admin_reports(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(params): Query<AdminReportsParams>,
    uri: Uri,
    req_headers: HeaderMap,
) -> AppResult<impl IntoResponse> {
    require_permission(&state, auth.account_id, perm::MANAGE_REPORTS).await?;

    let limit = params.limit.unwrap_or(20).clamp(1, 40);
    let resolved = params.resolved.unwrap_or(false);
    let max_id = params.max_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let min_id = params.min_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let since_id = params
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());

    let rows = sqlx::query!(
        r#"SELECT r.id, r.account_id, r.target_account_id,
                  r.comment, r.forwarded, r.action_taken_at,
                  r.created_at, r.updated_at,
                  CASE r.category WHEN 1000 THEN 'spam' WHEN 1500 THEN 'legal' WHEN 2000 THEN 'violation' ELSE 'other' END AS "category!"
           FROM reports r
           WHERE ($1 = (r.action_taken_at IS NOT NULL))
             AND ($3::bigint IS NULL OR r.id < $3)
             AND ($4::bigint IS NULL OR r.id > $4)
             AND ($5::bigint IS NULL OR r.id > $5)
           ORDER BY r.id DESC
           LIMIT $2"#,
        resolved, limit, max_id, since_id, min_id,
    )
    .fetch_all(&state.db)
    .await?;

    let mut result = Vec::with_capacity(rows.len());
    for r in &rows {
        let row = AdminReportRow {
            id: r.id,
            account_id: r.account_id,
            target_account_id: r.target_account_id,
            comment: r.comment.clone(),
            forwarded: r.forwarded,
            category: r.category.clone(),
            action_taken_at: r.action_taken_at,
            created_at: r.created_at,
            updated_at: r.updated_at,
        };
        result.push(build_admin_report(&state, &row).await?);
    }

    let bounds = result
        .first()
        .zip(result.last())
        .map(|(n, o)| (n.id.as_str(), o.id.as_str()));
    let resp_headers = super::link_headers(&req_headers, &uri, bounds);
    Ok((resp_headers, Json(result)))
}

// ── GET /api/v1/admin/reports/:id ────────────────────────────────────────

pub async fn get_admin_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminReport>> {
    require_permission(&state, auth.account_id, perm::MANAGE_REPORTS).await?;
    let r = sqlx::query!(
        r#"SELECT r.id, r.account_id, r.target_account_id,
                  r.comment, r.forwarded, r.action_taken_at,
                  r.created_at, r.updated_at,
                  CASE r.category WHEN 1000 THEN 'spam' WHEN 1500 THEN 'legal' WHEN 2000 THEN 'violation' ELSE 'other' END AS "category!"
           FROM reports r
           WHERE r.id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let row = AdminReportRow {
        id: r.id,
        account_id: r.account_id,
        target_account_id: r.target_account_id,
        comment: r.comment.clone(),
        forwarded: r.forwarded,
        category: r.category.clone(),
        action_taken_at: r.action_taken_at,
        created_at: r.created_at,
        updated_at: r.updated_at,
    };
    Ok(Json(build_admin_report(&state, &row).await?))
}

// ── POST /api/v1/admin/reports/:id/resolve ───────────────────────────────

pub async fn resolve_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminReport>> {
    require_permission(&state, auth.account_id, perm::MANAGE_REPORTS).await?;
    sqlx::query!(
        "UPDATE reports SET action_taken_at = now(), action_taken_by_account_id = $1 WHERE id = $2",
        auth.account_id,
        id,
    )
    .execute(&state.db)
    .await?;
    let r = sqlx::query!(
        r#"SELECT r.id, r.account_id, r.target_account_id,
                  r.comment, r.forwarded, r.action_taken_at,
                  r.created_at, r.updated_at,
                  CASE r.category WHEN 1000 THEN 'spam' WHEN 1500 THEN 'legal' WHEN 2000 THEN 'violation' ELSE 'other' END AS "category!"
           FROM reports r
           WHERE r.id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let row = AdminReportRow {
        id: r.id,
        account_id: r.account_id,
        target_account_id: r.target_account_id,
        comment: r.comment.clone(),
        forwarded: r.forwarded,
        category: r.category.clone(),
        action_taken_at: r.action_taken_at,
        created_at: r.created_at,
        updated_at: r.updated_at,
    };
    Ok(Json(build_admin_report(&state, &row).await?))
}

// ── POST /api/v1/admin/reports/:id/reopen ────────────────────────────────

pub async fn reopen_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminReport>> {
    require_permission(&state, auth.account_id, perm::MANAGE_REPORTS).await?;
    sqlx::query!(
        "UPDATE reports SET action_taken_at = NULL, action_taken_by_account_id = NULL WHERE id = $1",
        id,
    )
    .execute(&state.db)
    .await?;
    get_admin_report(state, Extension(auth), Path(id)).await
}

// ── GET /api/v1/admin/roles ───────────────────────────────────────────────

pub async fn list_admin_roles(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminRole>>> {
    require_admin(&state, auth.account_id).await?;
    Ok(Json(vec![
        role_for("admin"),
        role_for("moderator"),
        role_for("user"),
    ]))
}

pub async fn get_admin_role(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<String>,
) -> AppResult<Json<AdminRole>> {
    require_admin(&state, auth.account_id).await?;
    let role = match id.as_str() {
        "1" => role_for("admin"),
        "2" => role_for("moderator"),
        _ => role_for("user"),
    };
    Ok(Json(role))
}

// ── POST /api/v1/admin/measures ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MeasuresRequest {
    pub keys: Vec<String>,
    pub start_at: Option<String>,
    pub end_at: Option<String>,
}

pub async fn get_measures(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    QueryOrJson(body): QueryOrJson<MeasuresRequest>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
    require_admin(&state, auth.account_id).await?;

    let start: chrono::NaiveDateTime = body
        .start_at
        .as_deref()
        .and_then(parse_admin_date)
        .unwrap_or_else(|| chrono::Utc::now().naive_utc() - chrono::Duration::days(7));
    let end: chrono::NaiveDateTime = body
        .end_at
        .as_deref()
        .and_then(parse_admin_date)
        .unwrap_or_else(|| chrono::Utc::now().naive_utc());
    let prev_start = start - (end - start);

    let mut result = Vec::new();

    for key in &body.keys {
        let measure = match key.as_str() {
            "new_users" => {
                let total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM users WHERE created_at BETWEEN $1 AND $2",
                    start,
                    end,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let previous_total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM users WHERE created_at BETWEEN $1 AND $2",
                    prev_start,
                    start,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let data = sqlx::query!(
                    r#"SELECT axis.day::timestamp,
                              (SELECT COUNT(*) FROM users
                               WHERE date_trunc('day', created_at)::date = axis.day) AS n
                       FROM (SELECT generate_series($1::timestamp, $2::timestamp, '1 day')::date AS day) AS axis
                       ORDER BY axis.day"#,
                    start, end,
                ).fetch_all(&state.db).await?;
                serde_json::json!({
                    "key": key, "unit": null,
                    "total": total.to_string(),
                    "human_value": total.to_string(),
                    "previous_total": previous_total.to_string(),
                    "data": data.iter().map(|r| serde_json::json!({
                        "date": r.day.map(super::convert::mastodon_date).unwrap_or_default(),
                        "value": r.n.unwrap_or(0).to_string(),
                    })).collect::<Vec<_>>(),
                })
            }
            "active_users" => {
                // Matches Mastodon: counts users by current_sign_in_at (updated on every login).
                let total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM users WHERE current_sign_in_at BETWEEN $1 AND $2",
                    start,
                    end,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let previous_total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM users WHERE current_sign_in_at BETWEEN $1 AND $2",
                    prev_start,
                    start,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let data = sqlx::query!(
                    r#"SELECT axis.day::timestamp,
                              (SELECT COUNT(*) FROM users
                               WHERE date_trunc('day', current_sign_in_at)::date = axis.day) AS n
                       FROM (SELECT generate_series($1::timestamp, $2::timestamp, '1 day')::date AS day) AS axis
                       ORDER BY axis.day"#,
                    start, end,
                ).fetch_all(&state.db).await?;
                serde_json::json!({
                    "key": key, "unit": null,
                    "total": total.to_string(),
                    "human_value": total.to_string(),
                    "previous_total": previous_total.to_string(),
                    "data": data.iter().map(|r| serde_json::json!({
                        "date": r.day.map(super::convert::mastodon_date).unwrap_or_default(),
                        "value": r.n.unwrap_or(0).to_string(),
                    })).collect::<Vec<_>>(),
                })
            }
            "new_statuses" => {
                let total = sqlx::query_scalar!(
                    r#"SELECT COUNT(*) FROM statuses s JOIN accounts a ON a.id = s.account_id
                       WHERE a.domain IS NULL AND s.created_at BETWEEN $1 AND $2 AND s.deleted_at IS NULL"#,
                    start, end,
                ).fetch_one(&state.db).await?.unwrap_or(0);
                let previous_total = sqlx::query_scalar!(
                    r#"SELECT COUNT(*) FROM statuses s JOIN accounts a ON a.id = s.account_id
                       WHERE a.domain IS NULL AND s.created_at BETWEEN $1 AND $2 AND s.deleted_at IS NULL"#,
                    prev_start, start,
                ).fetch_one(&state.db).await?.unwrap_or(0);
                let data = sqlx::query!(
                    r#"SELECT axis.day::timestamp,
                              (SELECT COUNT(*) FROM statuses s JOIN accounts a ON a.id = s.account_id
                               WHERE a.domain IS NULL AND s.deleted_at IS NULL
                                 AND date_trunc('day', s.created_at)::date = axis.day) AS n
                       FROM (SELECT generate_series($1::timestamp, $2::timestamp, '1 day')::date AS day) AS axis
                       ORDER BY axis.day"#,
                    start, end,
                ).fetch_all(&state.db).await?;
                serde_json::json!({
                    "key": key, "unit": null,
                    "total": total.to_string(),
                    "human_value": total.to_string(),
                    "previous_total": previous_total.to_string(),
                    "data": data.iter().map(|r| serde_json::json!({
                        "date": r.day.map(super::convert::mastodon_date).unwrap_or_default(),
                        "value": r.n.unwrap_or(0).to_string(),
                    })).collect::<Vec<_>>(),
                })
            }
            "opened_reports" => {
                let total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM reports WHERE created_at BETWEEN $1 AND $2",
                    start,
                    end,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let previous_total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM reports WHERE created_at BETWEEN $1 AND $2",
                    prev_start,
                    start,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let data = sqlx::query!(
                    r#"SELECT axis.day::timestamp,
                              (SELECT COUNT(*) FROM reports
                               WHERE date_trunc('day', created_at)::date = axis.day) AS n
                       FROM (SELECT generate_series($1::timestamp, $2::timestamp, '1 day')::date AS day) AS axis
                       ORDER BY axis.day"#,
                    start, end,
                ).fetch_all(&state.db).await?;
                serde_json::json!({
                    "key": key, "unit": null,
                    "total": total.to_string(), "human_value": total.to_string(),
                    "previous_total": previous_total.to_string(),
                    "data": data.iter().map(|r| serde_json::json!({
                        "date": r.day.map(super::convert::mastodon_date).unwrap_or_default(),
                        "value": r.n.unwrap_or(0).to_string(),
                    })).collect::<Vec<_>>(),
                })
            }
            "resolved_reports" => {
                let total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM reports WHERE action_taken_at BETWEEN $1 AND $2",
                    start,
                    end,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let previous_total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM reports WHERE action_taken_at BETWEEN $1 AND $2",
                    prev_start,
                    start,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let data = sqlx::query!(
                    r#"SELECT axis.day::timestamp,
                              (SELECT COUNT(*) FROM reports
                               WHERE date_trunc('day', action_taken_at)::date = axis.day) AS n
                       FROM (SELECT generate_series($1::timestamp, $2::timestamp, '1 day')::date AS day) AS axis
                       ORDER BY axis.day"#,
                    start, end,
                ).fetch_all(&state.db).await?;
                serde_json::json!({
                    "key": key, "unit": null,
                    "total": total.to_string(), "human_value": total.to_string(),
                    "previous_total": previous_total.to_string(),
                    "data": data.iter().map(|r| serde_json::json!({
                        "date": r.day.map(super::convert::mastodon_date).unwrap_or_default(),
                        "value": r.n.unwrap_or(0).to_string(),
                    })).collect::<Vec<_>>(),
                })
            }
            "interactions" => {
                // Approximates Mastodon's Redis-backed interactions counter:
                // statuses posted + favourites + follows by local users.
                macro_rules! count_interactions {
                    ($s:expr, $e:expr) => {
                        sqlx::query_scalar!(
                            r#"SELECT
                               (SELECT COUNT(*) FROM statuses s JOIN accounts a ON a.id = s.account_id
                                WHERE a.domain IS NULL AND s.deleted_at IS NULL AND s.created_at BETWEEN $1 AND $2)
                               +
                               (SELECT COUNT(*) FROM favourites f JOIN accounts a ON a.id = f.account_id
                                WHERE a.domain IS NULL AND f.created_at BETWEEN $1 AND $2)
                               +
                               (SELECT COUNT(*) FROM follows f JOIN accounts a ON a.id = f.account_id
                                WHERE a.domain IS NULL AND f.created_at BETWEEN $1 AND $2)"#,
                            $s, $e,
                        ).fetch_one(&state.db).await?.unwrap_or(0)
                    };
                }
                let total = count_interactions!(start, end);
                let previous_total = count_interactions!(prev_start, start);
                let data = sqlx::query!(
                    r#"SELECT axis.day::timestamp,
                              (SELECT COUNT(*) FROM statuses s JOIN accounts a ON a.id = s.account_id
                               WHERE a.domain IS NULL AND s.deleted_at IS NULL
                                 AND date_trunc('day', s.created_at)::date = axis.day)
                              +
                              (SELECT COUNT(*) FROM favourites f JOIN accounts a ON a.id = f.account_id
                               WHERE a.domain IS NULL
                                 AND date_trunc('day', f.created_at)::date = axis.day)
                              +
                              (SELECT COUNT(*) FROM follows f2 JOIN accounts a ON a.id = f2.account_id
                               WHERE a.domain IS NULL
                                 AND date_trunc('day', f2.created_at)::date = axis.day)
                              AS n
                       FROM (SELECT generate_series($1::timestamp, $2::timestamp, '1 day')::date AS day) AS axis
                       ORDER BY axis.day"#,
                    start, end,
                ).fetch_all(&state.db).await?;
                serde_json::json!({
                    "key": key, "unit": null,
                    "total": total.to_string(),
                    "human_value": total.to_string(),
                    "previous_total": previous_total.to_string(),
                    "data": data.iter().map(|r| serde_json::json!({
                        "date": r.day.map(super::convert::mastodon_date).unwrap_or_default(),
                        "value": r.n.unwrap_or(0).to_string(),
                    })).collect::<Vec<_>>(),
                })
            }
            _ => serde_json::json!({
                "key": key, "unit": null, "total": "0",
                "human_value": "0", "previous_total": "0", "data": [],
            }),
        };
        result.push(measure);
    }

    Ok(Json(result))
}

fn locale_name(code: &str) -> &'static str {
    match code {
        "ko" => "Korean",
        "en" => "English",
        "ja" => "Japanese",
        "zh" => "Chinese",
        "fr" => "French",
        "de" => "German",
        "es" => "Spanish",
        "pt" => "Portuguese",
        "ru" => "Russian",
        "ar" => "Arabic",
        _ => "Unknown",
    }
}

fn parse_admin_date(s: &str) -> Option<chrono::NaiveDateTime> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&chrono::Utc).naive_utc());
    }
    // Mastodon sends date-only strings like "2026-04-27"
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return date.and_hms_opt(0, 0, 0);
    }
    None
}

fn human_size(bytes: i64) -> String {
    const KB: i64 = 1024;
    const MB: i64 = 1024 * KB;
    const GB: i64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn parse_redis_info_field(info: &str, field: &str) -> Option<String> {
    info.lines()
        .find(|l| l.starts_with(field))
        .and_then(|l| l.split_once(':').map(|x| x.1))
        .map(|v| v.trim().to_string())
}

// ── POST /api/v1/admin/dimensions ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DimensionsRequest {
    pub keys: Vec<String>,
    pub start_at: Option<String>,
    pub end_at: Option<String>,
    pub limit: Option<i64>,
}

pub async fn get_dimensions(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    QueryOrJson(body): QueryOrJson<DimensionsRequest>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
    require_admin(&state, auth.account_id).await?;

    let start: chrono::NaiveDateTime = body
        .start_at
        .as_deref()
        .and_then(parse_admin_date)
        .unwrap_or_else(|| chrono::Utc::now().naive_utc() - chrono::Duration::days(7));
    let end: chrono::NaiveDateTime = body
        .end_at
        .as_deref()
        .and_then(parse_admin_date)
        .unwrap_or_else(|| chrono::Utc::now().naive_utc());
    let limit = body.limit.unwrap_or(10).clamp(1, 50);

    let mut result = Vec::new();

    for key in &body.keys {
        let dimension = match key.as_str() {
            "servers" => {
                let rows = sqlx::query!(
                    r#"SELECT COALESCE(a.domain, 'local') AS server, COUNT(*) AS n
                       FROM statuses s JOIN accounts a ON a.id = s.account_id
                       WHERE s.created_at BETWEEN $1 AND $2 AND s.deleted_at IS NULL
                       GROUP BY COALESCE(a.domain, 'local') ORDER BY n DESC LIMIT $3"#,
                    start,
                    end,
                    limit,
                )
                .fetch_all(&state.db)
                .await?;
                serde_json::json!({
                    "key": key,
                    "data": rows.iter().map(|r| {
                        let v = r.n.unwrap_or(0).to_string();
                        serde_json::json!({
                            "key": r.server,
                            "human_key": r.server,
                            "value": v,
                            "unit": null,
                            "human_value": v,
                        })
                    }).collect::<Vec<_>>(),
                })
            }
            "sources" => {
                let rows = sqlx::query!(
                    r#"SELECT COALESCE(a.name, 'web') AS name, COUNT(*) AS n
                       FROM users u
                       LEFT JOIN oauth_applications a ON a.id = u.created_by_application_id
                       WHERE u.created_at BETWEEN $1 AND $2
                       GROUP BY a.name ORDER BY n DESC LIMIT $3"#,
                    start,
                    end,
                    limit,
                )
                .fetch_all(&state.db)
                .await?;
                serde_json::json!({
                    "key": key,
                    "data": rows.iter().map(|r| {
                        let v = r.n.unwrap_or(0).to_string();
                        serde_json::json!({
                            "key": r.name, "human_key": r.name,
                            "value": v, "unit": null, "human_value": v,
                        })
                    }).collect::<Vec<_>>(),
                })
            }
            "languages" => {
                let rows = sqlx::query!(
                    r#"SELECT COALESCE(u.locale, 'und') AS locale, COUNT(*) AS n
                       FROM users u
                       WHERE u.current_sign_in_at BETWEEN $1 AND $2
                         AND u.locale IS NOT NULL
                       GROUP BY u.locale ORDER BY n DESC LIMIT $3"#,
                    start,
                    end,
                    limit,
                )
                .fetch_all(&state.db)
                .await?;
                serde_json::json!({
                    "key": key,
                    "data": rows.iter().map(|r| {
                        let v = r.n.unwrap_or(0).to_string();
                        let human = locale_name(r.locale.as_deref().unwrap_or("und"));
                        serde_json::json!({
                            "key": r.locale, "human_key": human,
                            "value": v, "unit": null, "human_value": v,
                        })
                    }).collect::<Vec<_>>(),
                })
            }
            "software_versions" => {
                let pg_version_raw: String = sqlx::query_scalar!("SELECT version()")
                    .fetch_one(&state.db)
                    .await?
                    .unwrap_or_default();
                let pg_version = pg_version_raw
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("unknown")
                    .to_string();

                let mut redis = state.redis.clone();
                let redis_info: String = redis::cmd("INFO")
                    .arg("server")
                    .query_async(&mut redis)
                    .await
                    .unwrap_or_default();
                let redis_version = parse_redis_info_field(&redis_info, "redis_version")
                    .unwrap_or_else(|| "unknown".to_string());

                let eunha_version = crate::version::EUNHA_FULL.to_string();

                serde_json::json!({
                    "key": key,
                    "data": [
                        {"key": "mastodon", "human_key": "Eunha", "value": eunha_version.clone(), "human_value": eunha_version},
                        {"key": "postgresql", "human_key": "PostgreSQL", "value": pg_version.clone(), "human_value": pg_version},
                        {"key": "redis", "human_key": "Redis", "value": redis_version.clone(), "human_value": redis_version},
                    ],
                })
            }
            "space_usage" => {
                let pg_size: i64 =
                    sqlx::query_scalar!("SELECT pg_database_size(current_database())")
                        .fetch_one(&state.db)
                        .await?
                        .unwrap_or(0);

                let redis_size = if state.config.redis_process_metrics
                    && !state.redis_keys.is_shared()
                    && state.config.redis_coordination_url.is_none()
                {
                    let mut redis = state.redis.clone();
                    let redis_mem_info: String = redis::cmd("INFO")
                        .arg("memory")
                        .query_async(&mut redis)
                        .await
                        .unwrap_or_default();
                    parse_redis_info_field(&redis_mem_info, "used_memory")
                        .and_then(|v| v.parse::<i64>().ok())
                } else {
                    None
                };

                let media_size: i64 = sqlx::query_scalar!(
                    r#"SELECT
                       COALESCE((SELECT SUM(COALESCE(file_file_size,0) + COALESCE(thumbnail_file_size,0)) FROM media_attachments), 0)
                       + COALESCE((SELECT SUM(COALESCE(image_file_size,0)) FROM custom_emojis), 0)
                       + COALESCE((SELECT SUM(COALESCE(image_file_size,0)) FROM preview_cards), 0)
                       + COALESCE((SELECT SUM(COALESCE(avatar_file_size,0) + COALESCE(header_file_size,0)) FROM accounts), 0)"#
                ).fetch_one(&state.db).await?.unwrap_or(0);

                serde_json::json!({
                    "key": key,
                    "data": [
                        {
                            "key": "postgresql", "human_key": "PostgreSQL",
                            "value": pg_size.to_string(), "unit": "bytes",
                            "human_value": human_size(pg_size),
                        },
                        redis_size.map(|bytes| serde_json::json!({
                            "key": "redis", "human_key": "Redis",
                            "value": bytes.to_string(), "unit": "bytes",
                            "human_value": human_size(bytes),
                        })).unwrap_or_else(|| serde_json::json!({
                            "key": "redis", "human_key": "Redis",
                            "value": null, "unit": "bytes",
                            "human_value": "Unavailable for shared Redis",
                        })),
                        {
                            "key": "media", "human_key": "Media storage",
                            "value": media_size.to_string(), "unit": "bytes",
                            "human_value": human_size(media_size),
                        },
                    ],
                })
            }
            _ => serde_json::json!({"key": key, "data": []}),
        };
        result.push(dimension);
    }

    Ok(Json(result))
}

// ── POST /api/v1/admin/retention ─────────────────────────────────────────
//
// Matches Mastodon: cohorts are groups of users who signed up in the same
// period; a user is "retained" in a later period if current_sign_in_at
// falls within that period (i.e. they logged in again).

#[derive(Debug, Deserialize)]
pub struct RetentionRequest {
    pub start_at: Option<String>,
    pub end_at: Option<String>,
    pub frequency: Option<String>, // "day" or "month"
}

pub async fn get_retention(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    QueryOrJson(body): QueryOrJson<RetentionRequest>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
    require_admin(&state, auth.account_id).await?;

    let start: chrono::NaiveDateTime = body
        .start_at
        .as_deref()
        .and_then(parse_admin_date)
        .unwrap_or_else(|| chrono::Utc::now().naive_utc() - chrono::Duration::days(30));
    let end: chrono::NaiveDateTime = body
        .end_at
        .as_deref()
        .and_then(parse_admin_date)
        .unwrap_or_else(|| chrono::Utc::now().naive_utc());
    let frequency = match body.frequency.as_deref().unwrap_or("day") {
        "month" => "month",
        _ => "day",
    };

    // Mirrors Mastodon's retention SQL exactly: for every (cohort_period,
    // retention_period) pair where retention_period >= cohort_period, count
    // users whose signup was in cohort_period and whose current_sign_in_at
    // is >= retention_period.
    let rows = sqlx::query!(
        r#"SELECT
               axis.cohort_period::timestamp,
               axis.retention_period::timestamp,
               (
                 WITH new_users AS (
                   SELECT users.id FROM users
                   WHERE date_trunc($3, users.created_at)::date = axis.cohort_period
                 ),
                 retained_users AS (
                   SELECT users.id FROM users
                   INNER JOIN new_users ON new_users.id = users.id
                   WHERE date_trunc($3, users.current_sign_in_at) >= axis.retention_period
                 )
                 SELECT ARRAY[
                   count(*)::bigint,
                   (count(*)::float /
                    GREATEST((SELECT count(*) FROM new_users), 1) * 1000000)::bigint
                 ]
                 FROM retained_users
               ) AS retention_value_and_rate
           FROM (
             WITH cohort_periods AS (
               SELECT generate_series(
                 date_trunc($3, $1::timestamp)::date,
                 date_trunc($3, $2::timestamp)::date,
                 ('1 ' || $3)::interval
               ) AS cohort_period
             ),
             retention_periods AS (
               SELECT cohort_period AS retention_period FROM cohort_periods
             )
             SELECT * FROM cohort_periods, retention_periods
             WHERE retention_period >= cohort_period
           ) AS axis
           ORDER BY axis.cohort_period, axis.retention_period"#,
        start,
        end,
        frequency,
    )
    .fetch_all(&state.db)
    .await?;

    let mut cohorts: indexmap::IndexMap<chrono::NaiveDateTime, Vec<serde_json::Value>> =
        indexmap::IndexMap::new();

    for row in &rows {
        let cohort_period = match row.cohort_period {
            Some(p) => p,
            None => continue,
        };
        let retention_period = match row.retention_period {
            Some(p) => p,
            None => continue,
        };
        let (value, rate_millionths) = match row.retention_value_and_rate.as_deref() {
            Some([v, r]) => (*v, *r),
            _ => (0, 0),
        };
        let rate = rate_millionths as f64 / 1_000_000.0;

        cohorts
            .entry(cohort_period)
            .or_default()
            .push(serde_json::json!({
                "date": super::convert::mastodon_date(retention_period),
                "rate": rate,
                "value": value.to_string(),
            }));
    }

    let data: Vec<serde_json::Value> = cohorts
        .into_iter()
        .map(|(period, entries)| {
            let cohort_size = entries
                .first()
                .and_then(|e| e["value"].as_str())
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            serde_json::json!({
                "period": super::convert::mastodon_date(period),
                "frequency": frequency,
                "cohort_size": cohort_size,
                "data": entries,
            })
        })
        .collect();

    Ok(Json(data))
}

// ── Admin CustomEmoji type ────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AdminCustomEmoji {
    pub id: String,
    pub shortcode: String,
    pub url: String,
    pub static_url: String,
    pub visible_in_picker: bool,
    pub disabled: bool,
    pub category: Option<String>,
}

// ── GET /api/v1/admin/custom_emojis ──────────────────────────────────────

pub async fn list_admin_custom_emojis(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminCustomEmoji>>> {
    require_admin(&state, auth.account_id).await?;

    let rows = sqlx::query!(
        "SELECT id, shortcode, image_remote_url, visible_in_picker, disabled
         FROM custom_emojis WHERE domain IS NULL ORDER BY shortcode",
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let url = r.image_remote_url.unwrap_or_default();
                AdminCustomEmoji {
                    id: r.id.to_string(),
                    shortcode: r.shortcode,
                    url: url.clone(),
                    static_url: url,
                    visible_in_picker: r.visible_in_picker,
                    disabled: r.disabled,
                    category: None,
                }
            })
            .collect(),
    ))
}

// ── POST /api/v1/admin/custom_emojis ─────────────────────────────────────

pub async fn create_admin_custom_emoji(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    mut multipart: Multipart,
) -> AppResult<Json<AdminCustomEmoji>> {
    require_admin(&state, auth.account_id).await?;

    let mut shortcode = String::new();
    let mut image_bytes: Option<Vec<u8>> = None;
    let mut content_type = "image/png".to_string();

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "shortcode" => {
                shortcode = field.text().await.unwrap_or_default();
            }
            "image" => {
                content_type = field.content_type().unwrap_or("image/png").to_string();
                image_bytes = field.bytes().await.ok().map(|b| b.to_vec());
            }
            _ => {}
        }
    }

    if shortcode.is_empty() {
        return Err(AppError::Unprocessable("shortcode is required".into()));
    }
    let image_data =
        image_bytes.ok_or_else(|| AppError::Unprocessable("image is required".into()))?;

    // Upload to storage
    let ext = match content_type.as_str() {
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    };
    let key = format!("emoji/{}.{}", shortcode, ext);
    state
        .storage
        .store(&image_data, &key, &content_type)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("storage: {e}")))?;
    let url = state.storage.public_url(&key);

    let row = if let Some(row) = sqlx::query!(
        r#"UPDATE custom_emojis
           SET image_remote_url = $2, disabled = false, visible_in_picker = true, updated_at = now()
           WHERE shortcode = $1 AND domain IS NULL
           RETURNING id, shortcode, image_remote_url, visible_in_picker, disabled"#,
        shortcode,
        url,
    )
    .fetch_optional(&state.db)
    .await?
    {
        (
            row.id,
            row.shortcode,
            row.image_remote_url,
            row.visible_in_picker,
            row.disabled,
        )
    } else {
        let row = sqlx::query!(
            r#"INSERT INTO custom_emojis (shortcode, image_remote_url, visible_in_picker, created_at, updated_at)
               VALUES ($1, $2, true, now(), now())
               RETURNING id, shortcode, image_remote_url, visible_in_picker, disabled"#,
            shortcode, url,
        )
        .fetch_one(&state.db)
        .await?;
        (
            row.id,
            row.shortcode,
            row.image_remote_url,
            row.visible_in_picker,
            row.disabled,
        )
    };

    let url = row.2.unwrap_or_default();
    Ok(Json(AdminCustomEmoji {
        id: row.0.to_string(),
        shortcode: row.1,
        url: url.clone(),
        static_url: url,
        visible_in_picker: row.3,
        disabled: row.4,
        category: None,
    }))
}

// ── DELETE /api/v1/admin/custom_emojis/:id ───────────────────────────────

pub async fn delete_admin_custom_emoji(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_admin(&state, auth.account_id).await?;
    sqlx::query!("DELETE FROM custom_emojis WHERE id = $1", id,)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::OK)
}

// ── PATCH /api/v1/admin/custom_emojis/:id ────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PatchEmojiForm {
    pub shortcode: Option<String>,
    pub visible_in_picker: Option<bool>,
    pub disabled: Option<bool>,
}

pub async fn update_admin_custom_emoji(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Json(form): Json<PatchEmojiForm>,
) -> AppResult<Json<AdminCustomEmoji>> {
    require_admin(&state, auth.account_id).await?;
    if let Some(sc) = &form.shortcode {
        sqlx::query!(
            "UPDATE custom_emojis SET shortcode = $1 WHERE id = $2",
            sc,
            id
        )
        .execute(&state.db)
        .await?;
    }
    if let Some(v) = form.visible_in_picker {
        sqlx::query!(
            "UPDATE custom_emojis SET visible_in_picker = $1 WHERE id = $2",
            v,
            id
        )
        .execute(&state.db)
        .await?;
    }
    if let Some(d) = form.disabled {
        sqlx::query!(
            "UPDATE custom_emojis SET disabled = $1 WHERE id = $2",
            d,
            id
        )
        .execute(&state.db)
        .await?;
    }
    let row = sqlx::query!(
        "SELECT id, shortcode, image_remote_url, visible_in_picker, disabled FROM custom_emojis WHERE id = $1",
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let url = row.image_remote_url.unwrap_or_default();
    Ok(Json(AdminCustomEmoji {
        id: row.id.to_string(),
        shortcode: row.shortcode,
        url: url.clone(),
        static_url: url,
        visible_in_picker: row.visible_in_picker,
        disabled: row.disabled,
        category: None,
    }))
}

// ── Admin DomainBlock / DomainAllow types ─────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AdminDomainBlock {
    pub id: String,
    pub domain: String,
    pub digest: String,
    pub created_at: String,
    pub severity: String,
    pub reject_media: bool,
    pub reject_reports: bool,
    pub private_comment: Option<String>,
    pub public_comment: Option<String>,
    pub obfuscate: bool,
}

#[derive(Debug, Serialize)]
pub struct AdminDomainAllow {
    pub id: String,
    pub domain: String,
    pub created_at: String,
}

// ── GET /api/v1/admin/domain_blocks ──────────────────────────────────────

pub async fn list_domain_blocks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminDomainBlock>>> {
    require_admin(&state, auth.account_id).await?;
    let rows = sqlx::query!(
        r#"SELECT id, domain, reject_media, reject_reports, private_comment, public_comment, obfuscate, created_at,
                  CASE severity WHEN 1 THEN 'suspend' WHEN 2 THEN 'noop' ELSE 'silence' END AS "severity!"
           FROM domain_blocks ORDER BY domain"#,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| AdminDomainBlock {
                id: r.id.to_string(),
                digest: sha256_hex(&r.domain),
                domain: r.domain,
                created_at: super::convert::mastodon_date(r.created_at),
                severity: r.severity,
                reject_media: r.reject_media,
                reject_reports: r.reject_reports,
                private_comment: r.private_comment,
                public_comment: r.public_comment,
                obfuscate: r.obfuscate,
            })
            .collect(),
    ))
}

// ── POST /api/v1/admin/domain_blocks ─────────────────────────────────────

/// `enum :severity, ..., validate: true`: an unknown value fails validation.
fn parse_domain_severity(s: &str) -> AppResult<i32> {
    crate::db::models::domain_severity::parse(s).ok_or_else(|| {
        AppError::Unprocessable("Validation failed: Severity is not included in the list".into())
    })
}

/// `validates :severity, presence: true` plus the validated enum.
fn parse_ip_severity(s: Option<&str>) -> AppResult<i32> {
    match s.filter(|s| !s.is_empty()) {
        None => Err(AppError::Unprocessable(
            "Validation failed: Severity can't be blank".into(),
        )),
        Some(s) => crate::db::models::ip_severity::parse(s).ok_or_else(|| {
            AppError::Unprocessable(
                "Validation failed: Severity is not included in the list".into(),
            )
        }),
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateDomainBlockForm {
    pub domain: String,
    pub severity: Option<String>,
    pub reject_media: Option<bool>,
    pub reject_reports: Option<bool>,
    pub private_comment: Option<String>,
    pub public_comment: Option<String>,
    pub obfuscate: Option<bool>,
}

pub async fn create_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<CreateDomainBlockForm>,
) -> AppResult<Json<AdminDomainBlock>> {
    require_admin(&state, auth.account_id).await?;
    let severity = parse_domain_severity(form.severity.as_deref().unwrap_or("silence"))?;
    let row = sqlx::query!(
        r#"INSERT INTO domain_blocks (domain, severity, reject_media, reject_reports, private_comment, public_comment, obfuscate, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, now(), now())
           ON CONFLICT (domain) DO UPDATE SET severity = $2, reject_media = $3, reject_reports = $4,
             private_comment = $5, public_comment = $6, obfuscate = $7, updated_at = now()
           RETURNING id, domain, reject_media, reject_reports, private_comment, public_comment, obfuscate, created_at,
                     CASE severity WHEN 1 THEN 'suspend' WHEN 2 THEN 'noop' ELSE 'silence' END AS "severity!""#,
        form.domain, severity,
        form.reject_media.unwrap_or(false),
        form.reject_reports.unwrap_or(false),
        form.private_comment,
        form.public_comment,
        form.obfuscate.unwrap_or(false),
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(AdminDomainBlock {
        id: row.id.to_string(),
        digest: sha256_hex(&row.domain),
        domain: row.domain,
        created_at: super::convert::mastodon_date(row.created_at),
        severity: row.severity,
        reject_media: row.reject_media,
        reject_reports: row.reject_reports,
        private_comment: row.private_comment,
        public_comment: row.public_comment,
        obfuscate: row.obfuscate,
    }))
}

// ── GET /api/v1/admin/domain_blocks/:id ──────────────────────────────────

pub async fn get_admin_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminDomainBlock>> {
    require_admin(&state, auth.account_id).await?;
    let r = sqlx::query!(
        r#"SELECT id, domain, reject_media, reject_reports, private_comment, public_comment, obfuscate, created_at,
                  CASE severity WHEN 1 THEN 'suspend' WHEN 2 THEN 'noop' ELSE 'silence' END AS "severity!"
           FROM domain_blocks WHERE id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(AdminDomainBlock {
        id: r.id.to_string(),
        digest: sha256_hex(&r.domain),
        domain: r.domain,
        created_at: super::convert::mastodon_date(r.created_at),
        severity: r.severity,
        reject_media: r.reject_media,
        reject_reports: r.reject_reports,
        private_comment: r.private_comment,
        public_comment: r.public_comment,
        obfuscate: r.obfuscate,
    }))
}

// ── PATCH /api/v1/admin/domain_blocks/:id ────────────────────────────────

pub async fn update_admin_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Json(form): Json<CreateDomainBlockForm>,
) -> AppResult<Json<AdminDomainBlock>> {
    require_admin(&state, auth.account_id).await?;
    let severity_int: Option<i32> = form
        .severity
        .as_deref()
        .map(parse_domain_severity)
        .transpose()?;
    let r = sqlx::query!(
        r#"UPDATE domain_blocks SET
               severity       = COALESCE($2, severity),
               reject_media   = COALESCE($3, reject_media),
               reject_reports = COALESCE($4, reject_reports),
               private_comment = $5,
               public_comment  = $6,
               obfuscate      = COALESCE($7, obfuscate),
               updated_at     = now()
           WHERE id = $1
           RETURNING id, domain, reject_media, reject_reports, private_comment, public_comment, obfuscate, created_at,
                     CASE severity WHEN 1 THEN 'suspend' WHEN 2 THEN 'noop' ELSE 'silence' END AS "severity!""#,
        id,
        severity_int,
        form.reject_media,
        form.reject_reports,
        form.private_comment,
        form.public_comment,
        form.obfuscate,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(AdminDomainBlock {
        id: r.id.to_string(),
        digest: sha256_hex(&r.domain),
        domain: r.domain,
        created_at: super::convert::mastodon_date(r.created_at),
        severity: r.severity,
        reject_media: r.reject_media,
        reject_reports: r.reject_reports,
        private_comment: r.private_comment,
        public_comment: r.public_comment,
        obfuscate: r.obfuscate,
    }))
}

// ── DELETE /api/v1/admin/domain_blocks/:id ───────────────────────────────

pub async fn delete_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_admin(&state, auth.account_id).await?;
    sqlx::query!("DELETE FROM domain_blocks WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::OK)
}

// ── GET /api/v1/admin/domain_allows ──────────────────────────────────────

pub async fn list_domain_allows(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminDomainAllow>>> {
    require_admin(&state, auth.account_id).await?;
    let rows = sqlx::query!("SELECT id, domain, created_at FROM domain_allows ORDER BY domain",)
        .fetch_all(&state.db)
        .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| AdminDomainAllow {
                id: r.id.to_string(),
                domain: r.domain,
                created_at: super::convert::mastodon_date(r.created_at),
            })
            .collect(),
    ))
}

// ── POST /api/v1/admin/domain_allows ─────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateDomainAllowForm {
    pub domain: String,
}

pub async fn create_domain_allow(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<CreateDomainAllowForm>,
) -> AppResult<Json<AdminDomainAllow>> {
    require_admin(&state, auth.account_id).await?;
    let row = sqlx::query!(
        r#"INSERT INTO domain_allows (domain, created_at, updated_at) VALUES ($1, now(), now())
           ON CONFLICT (domain) DO UPDATE SET updated_at = now()
           RETURNING id, domain, created_at"#,
        form.domain,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(AdminDomainAllow {
        id: row.id.to_string(),
        domain: row.domain,
        created_at: super::convert::mastodon_date(row.created_at),
    }))
}

// ── DELETE /api/v1/admin/domain_allows/:id ───────────────────────────────

pub async fn delete_domain_allow(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_admin(&state, auth.account_id).await?;
    sqlx::query!("DELETE FROM domain_allows WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::OK)
}

// ── Admin IP blocks ───────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AdminIpBlock {
    pub id: String,
    pub ip: String,
    pub severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateIpBlockForm {
    pub ip: String,
    pub severity: Option<String>,
    pub comment: Option<String>,
    pub expires_in: Option<i64>,
}

pub async fn list_ip_blocks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminIpBlock>>> {
    require_admin(&state, auth.account_id).await?;
    let rows = sqlx::query!(
        r#"SELECT id, host(ip) as "ip!", comment, expires_at, created_at,
                  CASE severity WHEN 5000 THEN 'sign_up_requires_approval' WHEN 5500 THEN 'sign_up_block' WHEN 9999 THEN 'no_access' ELSE '' END AS "severity!"
           FROM ip_blocks ORDER BY created_at DESC"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| AdminIpBlock {
                id: r.id.to_string(),
                ip: r.ip,
                severity: r.severity,
                comment: Some(r.comment),
                expires_at: r.expires_at.map(super::convert::mastodon_date),
                created_at: super::convert::mastodon_date(r.created_at),
            })
            .collect(),
    ))
}

pub async fn get_ip_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminIpBlock>> {
    require_admin(&state, auth.account_id).await?;
    let r = sqlx::query!(
        r#"SELECT id, host(ip) as "ip!", comment, expires_at, created_at,
                  CASE severity WHEN 5000 THEN 'sign_up_requires_approval' WHEN 5500 THEN 'sign_up_block' WHEN 9999 THEN 'no_access' ELSE '' END AS "severity!"
           FROM ip_blocks WHERE id = $1"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(AdminIpBlock {
        id: r.id.to_string(),
        ip: r.ip,
        severity: r.severity,
        comment: Some(r.comment),
        expires_at: r.expires_at.map(super::convert::mastodon_date),
        created_at: super::convert::mastodon_date(r.created_at),
    }))
}

pub async fn create_ip_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<CreateIpBlockForm>,
) -> AppResult<Json<AdminIpBlock>> {
    require_admin(&state, auth.account_id).await?;
    let severity = parse_ip_severity(form.severity.as_deref())?;
    let expires_at = form
        .expires_in
        .map(|secs| chrono::Utc::now().naive_utc() + chrono::Duration::seconds(secs));
    let r = sqlx::query!(
        r#"INSERT INTO ip_blocks (ip, severity, comment, expires_at, created_at, updated_at)
           VALUES ($1::text::inet, $2, $3, $4, now(), now())
           ON CONFLICT (ip) DO UPDATE SET severity = $2, comment = $3, expires_at = $4, updated_at = now()
           RETURNING id, host(ip) as "ip!", comment, expires_at, created_at,
                     CASE severity WHEN 5000 THEN 'sign_up_requires_approval' WHEN 5500 THEN 'sign_up_block' WHEN 9999 THEN 'no_access' ELSE '' END AS "severity!""#,
        form.ip, severity, form.comment.unwrap_or_default(), expires_at,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(AdminIpBlock {
        id: r.id.to_string(),
        ip: r.ip,
        severity: r.severity,
        comment: Some(r.comment),
        expires_at: r.expires_at.map(super::convert::mastodon_date),
        created_at: super::convert::mastodon_date(r.created_at),
    }))
}

pub async fn update_ip_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Json(form): Json<CreateIpBlockForm>,
) -> AppResult<Json<AdminIpBlock>> {
    require_admin(&state, auth.account_id).await?;
    let severity = parse_ip_severity(form.severity.as_deref())?;
    let expires_at = form
        .expires_in
        .map(|secs| chrono::Utc::now().naive_utc() + chrono::Duration::seconds(secs));
    let r = sqlx::query!(
        r#"UPDATE ip_blocks SET severity = $2, comment = $3, expires_at = $4, updated_at = now()
           WHERE id = $1
           RETURNING id, host(ip) as "ip!", comment, expires_at, created_at,
                     CASE severity WHEN 5000 THEN 'sign_up_requires_approval' WHEN 5500 THEN 'sign_up_block' WHEN 9999 THEN 'no_access' ELSE '' END AS "severity!""#,
        id, severity, form.comment.unwrap_or_default(), expires_at,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(AdminIpBlock {
        id: r.id.to_string(),
        ip: r.ip,
        severity: r.severity,
        comment: Some(r.comment),
        expires_at: r.expires_at.map(super::convert::mastodon_date),
        created_at: super::convert::mastodon_date(r.created_at),
    }))
}

pub async fn delete_ip_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_admin(&state, auth.account_id).await?;
    sqlx::query!("DELETE FROM ip_blocks WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::OK)
}

// ── Admin Email Domain blocks ─────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AdminEmailDomainBlock {
    pub id: String,
    pub domain: String,
    pub created_at: String,
    pub history: Vec<AdminEmailDomainBlockHistory>,
    pub allow_with_approval: bool,
}

#[derive(Debug, Serialize)]
pub struct AdminEmailDomainBlockHistory {
    pub day: String,
    pub accounts: String,
    pub uses: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateEmailDomainBlockForm {
    pub domain: String,
}

pub async fn list_email_domain_blocks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminEmailDomainBlock>>> {
    require_admin(&state, auth.account_id).await?;
    let rows = sqlx::query!(
        "SELECT id, domain, created_at, allow_with_approval FROM email_domain_blocks ORDER BY domain"
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| AdminEmailDomainBlock {
                id: r.id.to_string(),
                domain: r.domain,
                created_at: super::convert::mastodon_date(r.created_at),
                history: vec![],
                allow_with_approval: r.allow_with_approval,
            })
            .collect(),
    ))
}

pub async fn get_email_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminEmailDomainBlock>> {
    require_admin(&state, auth.account_id).await?;
    let r = sqlx::query!(
        "SELECT id, domain, created_at, allow_with_approval FROM email_domain_blocks WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(AdminEmailDomainBlock {
        id: r.id.to_string(),
        domain: r.domain,
        created_at: super::convert::mastodon_date(r.created_at),
        history: vec![],
        allow_with_approval: r.allow_with_approval,
    }))
}

pub async fn create_email_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<CreateEmailDomainBlockForm>,
) -> AppResult<Json<AdminEmailDomainBlock>> {
    require_admin(&state, auth.account_id).await?;
    let r = sqlx::query!(
        r#"INSERT INTO email_domain_blocks (domain, created_at, updated_at) VALUES ($1, now(), now())
           ON CONFLICT (domain) DO UPDATE SET updated_at = now()
           RETURNING id, domain, created_at, allow_with_approval"#,
        form.domain,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(AdminEmailDomainBlock {
        id: r.id.to_string(),
        domain: r.domain,
        created_at: super::convert::mastodon_date(r.created_at),
        history: vec![],
        allow_with_approval: r.allow_with_approval,
    }))
}

pub async fn delete_email_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_admin(&state, auth.account_id).await?;
    sqlx::query!("DELETE FROM email_domain_blocks WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::OK)
}

// ── POST /api/v1/admin/reports/:id/assign_to_self ────────────────────────

pub async fn assign_report_to_self(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminReport>> {
    require_permission(&state, auth.account_id, perm::MANAGE_REPORTS).await?;
    sqlx::query!(
        "UPDATE reports SET assigned_account_id = $1 WHERE id = $2",
        auth.account_id,
        id,
    )
    .execute(&state.db)
    .await?;
    get_admin_report(state, Extension(auth), Path(id)).await
}

// ── POST /api/v1/admin/reports/:id/unassign ──────────────────────────────

pub async fn unassign_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminReport>> {
    require_permission(&state, auth.account_id, perm::MANAGE_REPORTS).await?;
    sqlx::query!(
        "UPDATE reports SET assigned_account_id = NULL WHERE id = $1",
        id,
    )
    .execute(&state.db)
    .await?;
    get_admin_report(state, Extension(auth), Path(id)).await
}

// ── POST /api/v1/admin/accounts/:id/sensitive ────────────────────────────

pub async fn sensitive_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_permission(&state, auth.account_id, perm::MANAGE_USERS).await?;
    sqlx::query!(
        "UPDATE accounts SET sensitized_at = now() WHERE id = $1 AND sensitized_at IS NULL",
        id,
    )
    .execute(&state.db)
    .await?;
    let account = sqlx::query_as!(models::Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(build_admin_account(&state, &account).await?))
}

// ── POST /api/v1/admin/accounts/:id/unsensitive ──────────────────────────

pub async fn unsensitive_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_permission(&state, auth.account_id, perm::MANAGE_USERS).await?;
    sqlx::query!("UPDATE accounts SET sensitized_at = NULL WHERE id = $1", id,)
        .execute(&state.db)
        .await?;
    let account = sqlx::query_as!(models::Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(build_admin_account(&state, &account).await?))
}

// ── POST /api/v1/admin/accounts/:id/action ───────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AccountActionForm {
    #[serde(rename = "type")]
    pub action_type: Option<String>,
    pub text: Option<String>,
    pub report_id: Option<String>,
    pub warning_preset_id: Option<String>,
    pub send_email_notification: Option<bool>,
}

pub async fn account_action(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Json(form): Json<AccountActionForm>,
) -> AppResult<StatusCode> {
    require_permission(&state, auth.account_id, perm::MANAGE_USERS).await?;

    match form.action_type.as_deref().unwrap_or("none") {
        "disable" => {
            sqlx::query!(
                "UPDATE accounts SET suspended_at = now() WHERE id = $1 AND suspended_at IS NULL",
                id,
            )
            .execute(&state.db)
            .await?;
        }
        "sensitive" => {
            sqlx::query!(
                "UPDATE accounts SET sensitized_at = now() WHERE id = $1 AND sensitized_at IS NULL",
                id,
            )
            .execute(&state.db)
            .await?;
        }
        "silence" => {
            sqlx::query!(
                "UPDATE accounts SET silenced_at = now() WHERE id = $1 AND silenced_at IS NULL",
                id,
            )
            .execute(&state.db)
            .await?;
        }
        "suspend" => {
            let account =
                sqlx::query_as!(models::Account, "SELECT * FROM accounts WHERE id = $1", id)
                    .fetch_optional(&state.db)
                    .await?
                    .ok_or(AppError::NotFound)?;
            if account.suspended_at.is_none() {
                let origin = if account.is_local() {
                    crate::delete_account::suspension_origin::LOCAL
                } else {
                    crate::delete_account::suspension_origin::REMOTE
                };
                crate::delete_account::suspend(&state, id, origin, true).await?;
            }
        }
        _ => {}
    }

    if let Some(report_id_str) = &form.report_id {
        if let Ok(report_id) = report_id_str.parse::<i64>() {
            sqlx::query!(
                "UPDATE reports SET action_taken_at = now(), action_taken_by_account_id = $1 WHERE id = $2",
                auth.account_id, report_id,
            )
            .execute(&state.db)
            .await?;
        }
    }

    Ok(StatusCode::OK)
}

// ── DELETE /api/v1/admin/accounts/:id ────────────────────────────────────

/// `Admin::AccountDeletionWorker`: purge the data of an account a moderator has
/// already suspended, keeping both the account and user records. Mastodon's
/// `AccountPolicy#destroy?` requires the suspension to still be reversible
/// (i.e. to have a pending deletion request), so a live account is a 403.
pub async fn delete_admin_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_permission(&state, auth.account_id, perm::MANAGE_USERS).await?;

    let suspended_temporarily = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM accounts a
             JOIN account_deletion_requests r ON r.account_id = a.id
             WHERE a.id = $1 AND a.suspended_at IS NOT NULL
           ) AS "exists!""#,
        id,
    )
    .fetch_one(&state.db)
    .await?;
    if !suspended_temporarily {
        return Err(AppError::Forbidden);
    }

    crate::delete_account::call(
        &state,
        id,
        crate::delete_account::Options {
            reserve_username: true,
            reserve_email: true,
            ..Default::default()
        },
    )
    .await?;

    Ok(StatusCode::OK)
}

// ── GET /api/v1/admin/trends/* ────────────────────────────────────────────

pub async fn admin_trending_tags(
    state: AppState,
    instance: axum::extract::Extension<crate::middleware::ResolvedInstance>,
    query: axum::extract::Query<super::trends::TrendParams>,
    auth: axum::extract::Extension<AuthenticatedUser>,
) -> AppResult<axum::Json<Vec<super::types::Tag>>> {
    require_permission(&state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    // Mastodon's admin trends controllers do not paginate, so the Link header
    // the public endpoint builds is dropped rather than passed on.
    let (_headers, json) = super::trends::trending_tags(
        state,
        instance,
        query,
        None,
        axum::http::HeaderMap::new(),
        axum::http::Uri::default(),
    )
    .await?;
    Ok(json)
}

pub async fn admin_trending_statuses(
    state: AppState,
    query: axum::extract::Query<super::trends::TrendParams>,
    auth: axum::extract::Extension<AuthenticatedUser>,
) -> AppResult<axum::Json<Vec<super::types::Status>>> {
    require_permission(&state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let (_headers, json) = super::trends::trending_statuses(
        state,
        query,
        Some(axum::extract::Extension(
            crate::middleware::AuthenticatedUser {
                account_id: auth.account_id,
                user_id: auth.user_id,
                token_id: auth.token_id,
                scopes: auth.scopes.clone(),
                application_id: auth.application_id,
            },
        )),
        axum::http::HeaderMap::new(),
        axum::http::Uri::default(),
    )
    .await?;
    Ok(json)
}

pub async fn admin_trending_links(
    state: AppState,
    query: axum::extract::Query<super::trends::TrendParams>,
    auth: axum::extract::Extension<AuthenticatedUser>,
) -> AppResult<axum::Json<Vec<super::types::PreviewCard>>> {
    require_permission(&state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let (_headers, json) = super::trends::trending_links(
        state,
        query,
        axum::http::HeaderMap::new(),
        axum::http::Uri::default(),
    )
    .await?;
    Ok(json)
}

// ── Admin Trends Approve / Reject (stubs — eunha computes trends dynamically) ──

pub async fn admin_approve_trending_tag(
    Extension(auth): Extension<AuthenticatedUser>,
    state: AppState,
    Path(_id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    require_admin(&state, auth.account_id).await?;
    Ok(Json(serde_json::json!({})))
}

pub async fn admin_reject_trending_tag(
    Extension(auth): Extension<AuthenticatedUser>,
    state: AppState,
    Path(_id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    require_admin(&state, auth.account_id).await?;
    Ok(Json(serde_json::json!({})))
}

pub async fn admin_approve_trending_status(
    Extension(auth): Extension<AuthenticatedUser>,
    state: AppState,
    Path(_id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    require_admin(&state, auth.account_id).await?;
    Ok(Json(serde_json::json!({})))
}

pub async fn admin_reject_trending_status(
    Extension(auth): Extension<AuthenticatedUser>,
    state: AppState,
    Path(_id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    require_admin(&state, auth.account_id).await?;
    Ok(Json(serde_json::json!({})))
}

pub async fn admin_approve_trending_link(
    Extension(auth): Extension<AuthenticatedUser>,
    state: AppState,
    Path(_id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    require_admin(&state, auth.account_id).await?;
    Ok(Json(serde_json::json!({})))
}

pub async fn admin_reject_trending_link(
    Extension(auth): Extension<AuthenticatedUser>,
    state: AppState,
    Path(_id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    require_admin(&state, auth.account_id).await?;
    Ok(Json(serde_json::json!({})))
}

// ── Admin Canonical Email Blocks ──────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct CanonicalEmailBlock {
    pub id: String,
    pub canonical_email_hash: String,
    pub created_at: String,
}

pub async fn list_canonical_email_blocks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<CanonicalEmailBlock>>> {
    require_admin(&state, auth.account_id).await?;
    let rows = sqlx::query!(
        "SELECT id, canonical_email_hash, created_at FROM canonical_email_blocks ORDER BY id DESC LIMIT 100",
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| CanonicalEmailBlock {
                id: r.id.to_string(),
                canonical_email_hash: r.canonical_email_hash,
                created_at: super::convert::mastodon_date(r.created_at),
            })
            .collect(),
    ))
}

#[derive(Debug, Deserialize)]
pub struct CreateCanonicalEmailBlockForm {
    pub email: Option<String>,
    pub canonical_email_hash: Option<String>,
}

pub async fn create_canonical_email_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<CreateCanonicalEmailBlockForm>,
) -> AppResult<Json<CanonicalEmailBlock>> {
    require_admin(&state, auth.account_id).await?;
    let hash = if let Some(h) = form.canonical_email_hash {
        h
    } else if let Some(email) = form.email {
        let normalized = email.trim().to_lowercase();
        sha256_hex(&normalized)
    } else {
        return Err(AppError::Unprocessable(
            "email or canonical_email_hash required".into(),
        ));
    };
    let row = sqlx::query!(
        "INSERT INTO canonical_email_blocks (canonical_email_hash, created_at, updated_at) VALUES ($1, now(), now()) ON CONFLICT (canonical_email_hash) DO UPDATE SET canonical_email_hash = EXCLUDED.canonical_email_hash RETURNING id, canonical_email_hash, created_at",
        hash,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(CanonicalEmailBlock {
        id: row.id.to_string(),
        canonical_email_hash: row.canonical_email_hash,
        created_at: super::convert::mastodon_date(row.created_at),
    }))
}

pub async fn get_canonical_email_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<CanonicalEmailBlock>> {
    require_admin(&state, auth.account_id).await?;
    let row = sqlx::query!(
        "SELECT id, canonical_email_hash, created_at FROM canonical_email_blocks WHERE id = $1",
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(CanonicalEmailBlock {
        id: row.id.to_string(),
        canonical_email_hash: row.canonical_email_hash,
        created_at: super::convert::mastodon_date(row.created_at),
    }))
}

pub async fn delete_canonical_email_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    require_admin(&state, auth.account_id).await?;
    sqlx::query!("DELETE FROM canonical_email_blocks WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    Ok(Json(serde_json::json!({})))
}

pub async fn test_canonical_email_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<CreateCanonicalEmailBlockForm>,
) -> AppResult<Json<Vec<CanonicalEmailBlock>>> {
    require_admin(&state, auth.account_id).await?;
    let hash = if let Some(h) = form.canonical_email_hash {
        h
    } else if let Some(email) = form.email {
        let normalized = email.trim().to_lowercase();
        sha256_hex(&normalized)
    } else {
        return Err(AppError::Unprocessable(
            "email or canonical_email_hash required".into(),
        ));
    };
    let rows = sqlx::query!(
        "SELECT id, canonical_email_hash, created_at FROM canonical_email_blocks WHERE canonical_email_hash = $1",
        hash,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| CanonicalEmailBlock {
                id: r.id.to_string(),
                canonical_email_hash: r.canonical_email_hash,
                created_at: super::convert::mastodon_date(r.created_at),
            })
            .collect(),
    ))
}

// ── Admin Tags ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AdminTag {
    pub id: String,
    pub name: String,
    pub url: String,
    pub trendable: bool,
    pub usable: bool,
    pub requires_review: bool,
    pub listable: bool,
}

fn admin_tag_url(domain: &str, name: &str) -> String {
    format!("https://{domain}/tags/{name}")
}

#[derive(Debug, Deserialize)]
pub struct UpdateAdminTagForm {
    pub trendable: Option<bool>,
    pub usable: Option<bool>,
    pub listable: Option<bool>,
}

#[derive(serde::Deserialize)]
pub struct AdminTagsParams {
    #[serde(flatten)]
    pub pagination: super::types::PaginationParams,
    pub name: Option<String>,
}

pub async fn list_admin_tags(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Query(params): Query<AdminTagsParams>,
) -> AppResult<Json<Vec<AdminTag>>> {
    require_permission(&state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let domain = &instance.domain;
    let limit = params.pagination.limit_clamped(100, 100);
    let max_id = params
        .pagination
        .max_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let since_id = params
        .pagination
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let min_id = params
        .pagination
        .min_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let name_filter = params.name.as_deref().map(|s| s.to_lowercase());

    let rows = sqlx::query!(
        r#"SELECT id, name, trendable, usable, listable, reviewed_at
           FROM tags
           WHERE ($2::bigint IS NULL OR id < $2)
             AND ($3::bigint IS NULL OR id > $3)
             AND ($4::bigint IS NULL OR id > $4)
             AND ($5::text IS NULL OR name = $5)
           ORDER BY id DESC
           LIMIT $1"#,
        limit,
        max_id,
        since_id,
        min_id,
        name_filter,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| AdminTag {
                id: r.id.to_string(),
                name: r.name.clone(),
                url: admin_tag_url(domain, &r.name),
                trendable: r.trendable.unwrap_or(false),
                usable: r.usable.unwrap_or(true),
                listable: r.listable.unwrap_or(true),
                requires_review: r.reviewed_at.is_none(),
            })
            .collect(),
    ))
}

pub async fn get_admin_tag(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminTag>> {
    require_permission(&state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let domain = &instance.domain;
    let r = sqlx::query!(
        "SELECT id, name, trendable, usable, listable, reviewed_at FROM tags WHERE id = $1",
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(AdminTag {
        id: r.id.to_string(),
        name: r.name.clone(),
        url: admin_tag_url(domain, &r.name),
        trendable: r.trendable.unwrap_or(false),
        usable: r.usable.unwrap_or(true),
        listable: r.listable.unwrap_or(true),
        requires_review: r.reviewed_at.is_none(),
    }))
}

pub async fn update_admin_tag(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Path(id): Path<i64>,
    Json(form): Json<UpdateAdminTagForm>,
) -> AppResult<Json<AdminTag>> {
    require_permission(&state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let domain = &instance.domain;
    let r = sqlx::query!(
        r#"UPDATE tags SET
               trendable   = COALESCE($2, trendable),
               usable      = COALESCE($3, usable),
               listable    = COALESCE($4, listable),
               reviewed_at = now(),
               updated_at  = now()
           WHERE id = $1
           RETURNING id, name, trendable, usable, listable, reviewed_at"#,
        id,
        form.trendable,
        form.usable,
        form.listable,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(AdminTag {
        id: r.id.to_string(),
        name: r.name.clone(),
        url: admin_tag_url(domain, &r.name),
        trendable: r.trendable.unwrap_or(false),
        usable: r.usable.unwrap_or(true),
        listable: r.listable.unwrap_or(true),
        requires_review: r.reviewed_at.is_none(),
    }))
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}
