//! `Api::V1::Admin::AccountsController`, `Api::V2::Admin::AccountsController`
//! and `Api::V1::Admin::AccountActionsController`.

use axum::{
    extract::{Extension, Path},
    http::{HeaderMap, Uri},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::{FlexBool, FlexId, Params};
use super::super::types::Account as ApiAccount;
use crate::{
    db::models,
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::{
        account_action::{self, AccountAction},
        action_log::{self, Target},
        role::{self, authorize, flag, Role},
    },
    state::AppState,
};

/// `Api::V1::Admin::AccountsController::LIMIT`.
const LIMIT: i64 = 100;

// ── REST::Admin::AccountSerializer ────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AdminAccount {
    pub id: String,
    pub username: String,
    pub domain: Option<String>,
    pub created_at: String,
    pub email: Option<String>,
    pub ip: Option<String>,
    pub confirmed: Option<bool>,
    pub suspended: bool,
    pub silenced: bool,
    pub sensitized: bool,
    pub disabled: Option<bool>,
    pub approved: Option<bool>,
    pub locale: Option<String>,
    pub invite_request: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by_application_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invited_by_account_id: Option<String>,
    pub ips: Option<Vec<AdminIp>>,
    pub account: ApiAccount,
    pub role: Option<RoleEntity>,
}

/// `REST::Admin::IpSerializer`.
#[derive(Debug, Serialize)]
pub struct AdminIp {
    pub ip: String,
    pub used_at: Option<String>,
}

/// `REST::RoleSerializer`.
#[derive(Debug, Serialize)]
pub struct RoleEntity {
    pub id: String,
    pub name: String,
    pub permissions: String,
    pub color: String,
    pub highlighted: bool,
    pub collection_limit: i32,
}

impl RoleEntity {
    pub fn of(role: &Role) -> Self {
        Self {
            id: role.id.unwrap_or_default().to_string(),
            name: role.name.clone(),
            permissions: role.computed.to_string(),
            color: role.color.clone(),
            highlighted: role.highlighted,
            collection_limit: role.collection_limit,
        }
    }
}

struct UserFacts {
    id: i64,
    email: String,
    confirmed: bool,
    approved: bool,
    disabled: bool,
    locale: Option<String>,
    created_by_application_id: Option<i64>,
    invite_request: Option<String>,
    invited_by_account_id: Option<i64>,
}

async fn user_facts(state: &AppState, account_id: i64) -> AppResult<Option<UserFacts>> {
    Ok(sqlx::query_as!(
        UserFacts,
        r#"SELECT u.id, u.email, (u.confirmed_at IS NOT NULL) AS "confirmed!", u.approved,
                  u.disabled, u.locale, u.created_by_application_id,
                  (SELECT r.text FROM user_invite_requests r WHERE r.user_id = u.id
                   ORDER BY r.id LIMIT 1) AS "invite_request?",
                  inviter.account_id AS "invited_by_account_id?"
           FROM users u
           LEFT JOIN invites i ON i.id = u.invite_id
           LEFT JOIN users inviter ON inviter.id = i.user_id
           WHERE u.account_id = $1"#,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?)
}

/// `REST::Admin::AccountSerializer`.
pub async fn build_admin_account(
    state: &AppState,
    account: &models::Account,
) -> AppResult<AdminAccount> {
    let user = user_facts(state, account.id).await?;
    let role = role::of_account(&state.db, account.id).await?;
    let ips = match &user {
        Some(u) => Some(
            sqlx::query!(
                r#"SELECT host(ip) AS "ip!", used_at FROM user_ips WHERE user_id = $1
                   ORDER BY used_at DESC"#,
                u.id,
            )
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|r| AdminIp {
                ip: r.ip,
                used_at: r.used_at.map(super::super::convert::mastodon_date),
            })
            .collect::<Vec<_>>(),
        ),
        None => None,
    };
    let ip = ips
        .as_ref()
        .and_then(|ips| ips.first())
        .map(|i| i.ip.clone());
    Ok(AdminAccount {
        id: account.id.to_string(),
        username: account.username.clone(),
        domain: account.domain.clone(),
        created_at: super::super::convert::mastodon_date(account.created_at),
        email: user.as_ref().map(|u| u.email.clone()),
        ip,
        confirmed: user.as_ref().map(|u| u.confirmed),
        suspended: account.suspended_at.is_some(),
        silenced: account.silenced_at.is_some(),
        sensitized: account.sensitized_at.is_some(),
        disabled: user.as_ref().map(|u| u.disabled),
        approved: user.as_ref().map(|u| u.approved),
        locale: user.as_ref().and_then(|u| u.locale.clone()),
        invite_request: user.as_ref().and_then(|u| u.invite_request.clone()),
        created_by_application_id: user
            .as_ref()
            .and_then(|u| u.created_by_application_id)
            .map(|id| id.to_string()),
        invited_by_account_id: user
            .as_ref()
            .and_then(|u| u.invited_by_account_id)
            .map(|id| id.to_string()),
        ips,
        account: super::super::accounts::account_to_api(state, account).await,
        role: role.as_ref().map(RoleEntity::of),
    })
}

async fn find_account(state: &AppState, id: i64) -> AppResult<models::Account> {
    sqlx::query_as!(models::Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

async fn render(state: &AppState, id: i64) -> AppResult<Json<AdminAccount>> {
    let account = find_account(state, id).await?;
    Ok(Json(build_admin_account(state, &account).await?))
}

fn require_scope(auth: &AuthenticatedUser, write: bool) -> AppResult<()> {
    // `authorize_if_got_token! :'admin:read', :'admin:read:accounts'`.
    let scope = if write {
        "admin:write:accounts"
    } else {
        "admin:read:accounts"
    };
    auth.require_scope(scope)
}

// ── AccountFilter ─────────────────────────────────────────────────────────

/// The `AccountFilter` keys, translated from either API version's params.
#[derive(Debug, Default)]
struct Filter {
    origin: Option<String>,
    status: Option<String>,
    role_ids: Option<Vec<i64>>,
    username: Option<String>,
    by_domain: Option<String>,
    display_name: Option<String>,
    email: Option<String>,
    ip: Option<String>,
    invited_by: Option<String>,
}

fn present(value: &Option<String>) -> Option<String> {
    value.as_ref().filter(|v| !v.trim().is_empty()).cloned()
}

/// `like` patterns are prefixes; escape what the value says literally.
fn like_prefix(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("{escaped}%")
}

/// `AccountFilter#results` with Mastodon's `to_a_paginated_by_id`.
async fn filtered_accounts(
    state: &AppState,
    filter: Filter,
    page: &Page,
) -> AppResult<Vec<models::Account>> {
    let mut q = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        "SELECT a.* FROM accounts a LEFT JOIN users u ON u.account_id = a.id WHERE a.id <> ",
    );
    q.push_bind(crate::federation::instance_actor::INSTANCE_ACTOR_ID);

    // `relevant_params`: a remote origin is dropped when a domain is given.
    let origin = if filter.origin.as_deref() == Some("remote") && filter.by_domain.is_some() {
        None
    } else {
        filter.origin
    };
    match origin.as_deref() {
        None => {}
        Some("local") => {
            q.push(" AND a.domain IS NULL");
        }
        Some("remote") => {
            q.push(" AND a.domain IS NOT NULL");
        }
        Some(other) => return Err(AppError::BadRequest(format!("Unknown origin: {other}"))),
    }
    match filter.status.as_deref() {
        None => {}
        Some("active") => {
            q.push(" AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL");
        }
        Some("pending") => {
            q.push(" AND u.approved = false");
        }
        Some("suspended") => {
            q.push(" AND a.suspended_at IS NOT NULL");
        }
        Some("disabled") => {
            q.push(
                " AND u.disabled = true AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL",
            );
        }
        Some("silenced") => {
            q.push(" AND a.silenced_at IS NOT NULL");
        }
        Some("sensitized") => {
            q.push(" AND a.sensitized_at IS NOT NULL");
        }
        Some(other) => return Err(AppError::BadRequest(format!("Unknown status: {other}"))),
    }
    if let Some(role_ids) = filter.role_ids {
        q.push(" AND u.role_id = ANY(")
            .push_bind(role_ids)
            .push(")");
    }
    if let Some(domain) = filter.by_domain {
        q.push(" AND a.domain = ")
            .push_bind(domain.trim().to_owned());
    }
    if let Some(username) = filter.username {
        let username = username.trim().trim_start_matches('@').to_owned();
        q.push(" AND lower(a.username) LIKE lower(")
            .push_bind(like_prefix(&username))
            .push(")");
    }
    if let Some(display_name) = filter.display_name {
        q.push(" AND a.display_name ILIKE ")
            .push_bind(like_prefix(display_name.trim()));
    }
    if let Some(email) = filter.email {
        q.push(" AND u.email ILIKE ")
            .push_bind(like_prefix(email.trim()));
    }
    if let Some(ip) = filter.ip {
        // `valid_ip?`: `IPAddr.new`, which takes an address or a CIDR range.
        let ip_trim = ip.trim();
        let addr = ip_trim.split_once('/').map_or(ip_trim, |(a, _)| a);
        if addr.parse::<std::net::IpAddr>().is_err() {
            return Ok(vec![]);
        }
        q.push(" AND EXISTS (SELECT 1 FROM user_ips ui WHERE ui.user_id = u.id AND ui.ip <<= ")
            .push_bind(ip.trim().to_owned())
            .push("::inet)");
    }
    if let Some(invited_by) = filter.invited_by {
        q.push(
            " AND EXISTS (SELECT 1 FROM invites i WHERE i.id = u.invite_id AND i.user_id::text = ",
        )
        .push_bind(invited_by)
        .push(")");
    }
    page.push(&mut q);
    let mut accounts: Vec<models::Account> = q.build_query_as().fetch_all(&state.db).await?;
    if page.min_id.is_some() {
        accounts.reverse();
    }
    Ok(accounts)
}

/// `to_a_paginated_by_id(limit, max_id:, since_id:, min_id:)`.
struct Page {
    limit: i64,
    max_id: Option<i64>,
    since_id: Option<i64>,
    min_id: Option<i64>,
}

impl Page {
    fn new(
        limit: Option<i64>,
        max_id: &Option<String>,
        since_id: &Option<String>,
        min_id: &Option<String>,
    ) -> Self {
        let parse = |v: &Option<String>| v.as_deref().and_then(|s| s.parse::<i64>().ok());
        Self {
            // `limit_param(LIMIT)`: the default, or at most twice it.
            limit: limit.map_or(LIMIT, |l| l.abs().min(LIMIT * 2)),
            max_id: parse(max_id),
            since_id: parse(since_id),
            min_id: parse(min_id),
        }
    }

    fn push(&self, q: &mut sqlx::QueryBuilder<'_, sqlx::Postgres>) {
        if let Some(max_id) = self.max_id {
            q.push(" AND a.id < ").push_bind(max_id);
        }
        if let Some(since_id) = self.since_id {
            q.push(" AND a.id > ").push_bind(since_id);
        }
        if let Some(min_id) = self.min_id {
            q.push(" AND a.id > ").push_bind(min_id);
            q.push(" ORDER BY a.id ASC");
        } else {
            q.push(" ORDER BY a.id DESC");
        }
        q.push(" LIMIT ").push_bind(self.limit);
    }
}

/// `role_ids` for `staff`: `UserRole.that_can(:manage_reports)`.
async fn staff_role_ids(state: &AppState) -> AppResult<Vec<i64>> {
    let everyone: i64 = sqlx::query_scalar!(
        "SELECT permissions FROM user_roles WHERE id = $1",
        role::EVERYONE_ROLE_ID
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(flag::DEFAULT);
    // `computed_permissions` of each role, then `can?(:manage_reports)`.
    Ok(sqlx::query_scalar!(
        r#"SELECT id FROM user_roles
           WHERE (CASE WHEN id = $1 THEN permissions
                       WHEN permissions & 1 = 1 THEN $3
                       ELSE permissions | $2 END) & $4 <> 0
           ORDER BY id"#,
        role::EVERYONE_ROLE_ID,
        everyone,
        flag::ALL,
        flag::MANAGE_REPORTS,
    )
    .fetch_all(&state.db)
    .await?)
}

async fn respond(
    state: &AppState,
    accounts: Vec<models::Account>,
    uri: &Uri,
    req_headers: &HeaderMap,
) -> AppResult<impl IntoResponse> {
    let mut result = Vec::with_capacity(accounts.len());
    for a in &accounts {
        result.push(build_admin_account(state, a).await?);
    }
    let bounds = result
        .first()
        .zip(result.last())
        .map(|(n, o)| (n.id.as_str(), o.id.as_str()));
    let headers = super::super::link_headers(req_headers, uri, bounds);
    Ok((headers, Json(result)))
}

// ── GET /api/v1/admin/accounts ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AccountsV1Params {
    pub remote: Option<String>,
    pub by_domain: Option<String>,
    pub active: Option<String>,
    pub pending: Option<String>,
    pub disabled: Option<String>,
    pub silenced: Option<String>,
    pub suspended: Option<String>,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub ip: Option<String>,
    pub staff: Option<String>,
    pub limit: Option<FlexId>,
    pub max_id: Option<String>,
    pub since_id: Option<String>,
    pub min_id: Option<String>,
}

pub async fn list_admin_accounts(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Params(params): Params<AccountsV1Params>,
) -> AppResult<impl IntoResponse> {
    require_scope(&auth, false)?;
    super::require_permission(&state, auth.account_id, flag::MANAGE_USERS).await?;

    // `translated_filter_params`: local and active unless said otherwise.
    let mut filter = Filter {
        origin: Some(
            if present(&params.remote).is_some() {
                "remote"
            } else {
                "local"
            }
            .into(),
        ),
        status: Some("active".into()),
        by_domain: present(&params.by_domain),
        username: present(&params.username),
        display_name: present(&params.display_name),
        email: present(&params.email),
        ip: present(&params.ip),
        ..Filter::default()
    };
    for (status, given) in [
        ("active", &params.active),
        ("pending", &params.pending),
        ("disabled", &params.disabled),
        ("silenced", &params.silenced),
        ("suspended", &params.suspended),
    ] {
        if present(given).is_some() {
            filter.status = Some(status.into());
        }
    }
    if present(&params.staff).is_some() {
        filter.role_ids = Some(staff_role_ids(&state).await?);
    }
    let page = Page::new(
        params.limit.map(|l| l.0),
        &params.max_id,
        &params.since_id,
        &params.min_id,
    );
    let accounts = filtered_accounts(&state, filter, &page).await?;
    respond(&state, accounts, &uri, &req_headers).await
}

// ── GET /api/v2/admin/accounts ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AccountsV2Params {
    pub origin: Option<String>,
    pub status: Option<String>,
    pub permissions: Option<String>,
    pub username: Option<String>,
    pub by_domain: Option<String>,
    pub display_name: Option<String>,
    pub email: Option<String>,
    pub ip: Option<String>,
    pub invited_by: Option<String>,
    #[serde(default)]
    pub role_ids: super::super::extractors::FlexIds,
    pub limit: Option<FlexId>,
    pub max_id: Option<String>,
    pub since_id: Option<String>,
    pub min_id: Option<String>,
}

pub async fn list_admin_accounts_v2(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Params(params): Params<AccountsV2Params>,
) -> AppResult<impl IntoResponse> {
    require_scope(&auth, false)?;
    super::require_permission(&state, auth.account_id, flag::MANAGE_USERS).await?;

    let mut filter = Filter {
        origin: present(&params.origin),
        status: present(&params.status),
        username: present(&params.username),
        by_domain: present(&params.by_domain),
        display_name: present(&params.display_name),
        email: present(&params.email),
        ip: present(&params.ip),
        invited_by: present(&params.invited_by),
        ..Filter::default()
    };
    if !params.role_ids.0.is_empty() {
        filter.role_ids = Some(params.role_ids.0.clone());
    }
    if params.permissions.as_deref() == Some("staff") {
        filter.role_ids = Some(staff_role_ids(&state).await?);
    }
    let page = Page::new(
        params.limit.map(|l| l.0),
        &params.max_id,
        &params.since_id,
        &params.min_id,
    );
    let accounts = filtered_accounts(&state, filter, &page).await?;
    respond(&state, accounts, &uri, &req_headers).await
}

// ── GET /api/v1/admin/accounts/:id ───────────────────────────────────────

pub async fn get_admin_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_scope(&auth, false)?;
    let account = find_account(&state, id).await?;
    super::require_permission(&state, auth.account_id, flag::MANAGE_USERS).await?;
    Ok(Json(build_admin_account(&state, &account).await?))
}

/// What `UserPolicy` and `AccountPolicy` look at here: the acting role and,
/// for a local account, its user.
struct Subject {
    account: models::Account,
    acting: Role,
    user: Option<UserFacts>,
}

async fn subject(state: &AppState, auth: &AuthenticatedUser, id: i64) -> AppResult<Subject> {
    let account = find_account(state, id).await?;
    Ok(Subject {
        acting: role::acting(&state.db, auth.account_id).await?,
        user: user_facts(state, account.id).await?,
        account,
    })
}

impl Subject {
    /// `require_local_account!`.
    fn local_user(&self) -> AppResult<&UserFacts> {
        match &self.user {
            Some(user) if self.account.is_local() => Ok(user),
            _ => Err(AppError::Forbidden),
        }
    }

    fn user_target(&self, user: &UserFacts) -> Target {
        Target::user(user.id, self.account.id, self.account.acct())
    }

    fn account_target(&self) -> Target {
        Target::account(self.account.id, self.account.acct())
    }
}

// ── POST /api/v1/admin/accounts/:id/enable ───────────────────────────────

pub async fn enable_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_scope(&auth, true)?;
    let s = subject(&state, &auth, id).await?;
    let user = s.local_user()?;
    // `UserPolicy#enable?`
    authorize(s.acting.can(&[flag::MANAGE_USERS]))?;
    // `User#enable!`
    sqlx::query!(
        "UPDATE users SET disabled = false, updated_at = now() WHERE id = $1",
        user.id
    )
    .execute(&state.db)
    .await?;
    action_log::log(&state.db, auth.account_id, "enable", &s.user_target(user)).await?;
    render(&state, id).await
}

// ── POST /api/v1/admin/accounts/:id/approve ──────────────────────────────

pub async fn approve_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_scope(&auth, true)?;
    let s = subject(&state, &auth, id).await?;
    let user = s.local_user()?;
    // `UserPolicy#approve?`
    authorize(s.acting.can(&[flag::MANAGE_USERS]) && !user.approved)?;
    crate::accounts::approve(&state, s.account.id).await?;
    action_log::log(&state.db, auth.account_id, "approve", &s.user_target(user)).await?;
    render(&state, id).await
}

// ── POST /api/v1/admin/accounts/:id/reject ───────────────────────────────

pub async fn reject_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    require_scope(&auth, true)?;
    let s = subject(&state, &auth, id).await?;
    let user = s.local_user()?;
    // `UserPolicy#reject?`
    authorize(s.acting.can(&[flag::MANAGE_USERS]) && !user.approved)?;
    let target = s.user_target(user);
    // `DeleteAccountService.new.call(@account, reserve_email: false,
    // reserve_username: false)` — a rejected signup leaves nothing behind.
    crate::delete_account::call(&state, id, crate::delete_account::Options::purge()).await?;
    action_log::log(&state.db, auth.account_id, "reject", &target).await?;
    Ok(Json(serde_json::json!({})))
}

// ── POST /api/v1/admin/accounts/:id/unsensitive ──────────────────────────

pub async fn unsensitive_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_scope(&auth, true)?;
    let s = subject(&state, &auth, id).await?;
    authorize(s.acting.can(&[flag::MANAGE_USERS]))?;
    sqlx::query!(
        "UPDATE accounts SET sensitized_at = NULL, updated_at = now() WHERE id = $1",
        id
    )
    .execute(&state.db)
    .await?;
    action_log::log(
        &state.db,
        auth.account_id,
        "unsensitive",
        &s.account_target(),
    )
    .await?;
    if s.account.is_local() {
        crate::moderation::webhooks::trigger(
            &state,
            "account.updated",
            crate::moderation::webhooks::Object::Account(id),
        )
        .await;
    }
    render(&state, id).await
}

// ── POST /api/v1/admin/accounts/:id/unsilence ────────────────────────────

pub async fn unsilence_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_scope(&auth, true)?;
    let s = subject(&state, &auth, id).await?;
    authorize(s.acting.can(&[flag::MANAGE_USERS]))?;
    sqlx::query!(
        "UPDATE accounts SET silenced_at = NULL, updated_at = now() WHERE id = $1",
        id
    )
    .execute(&state.db)
    .await?;
    crate::search::elasticsearch::indexing::account(&state, id).await;
    action_log::log(&state.db, auth.account_id, "unsilence", &s.account_target()).await?;
    if s.account.is_local() {
        crate::moderation::webhooks::trigger(
            &state,
            "account.updated",
            crate::moderation::webhooks::Object::Account(id),
        )
        .await;
    }
    render(&state, id).await
}

// ── POST /api/v1/admin/accounts/:id/unsuspend ────────────────────────────

pub async fn unsuspend_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_scope(&auth, true)?;
    let s = subject(&state, &auth, id).await?;
    // `AccountPolicy#unsuspend?`: only a suspension made here is undone here.
    authorize(
        s.acting.can(&[flag::MANAGE_USERS])
            && s.account.suspension_origin == Some(crate::delete_account::suspension_origin::LOCAL),
    )?;
    crate::delete_account::unsuspend(&state, id).await?;
    // `Admin::UnsuspensionWorker`.
    crate::moderation::suspension::unsuspend_later(&state, id).await;
    action_log::log(&state.db, auth.account_id, "unsuspend", &s.account_target()).await?;
    if s.account.is_local() {
        crate::moderation::webhooks::trigger(
            &state,
            "account.updated",
            crate::moderation::webhooks::Object::Account(id),
        )
        .await;
    }
    render(&state, id).await
}

// ── DELETE /api/v1/admin/accounts/:id ────────────────────────────────────

/// `Admin::AccountDeletionWorker`: purge the data of an account a moderator has
/// already suspended, keeping both the account and user records.
pub async fn delete_admin_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    require_scope(&auth, true)?;
    let s = subject(&state, &auth, id).await?;
    let has_request = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM account_deletion_requests WHERE account_id = $1) AS "e!""#,
        id,
    )
    .fetch_one(&state.db)
    .await?;
    // `AccountPolicy#destroy?`: `suspended_temporarily?` and `delete_user_data`.
    authorize(
        s.account.suspended_at.is_some() && has_request && s.acting.can(&[flag::DELETE_USER_DATA]),
    )?;
    // `Admin::AccountDeletionWorker`.
    if crate::feed::sync_fanout() {
        crate::delete_account::call(&state, id, crate::delete_account::Options::default()).await?;
    } else {
        crate::jobs::push(
            &state,
            crate::delete_account::AdminAccountDeletionWorker { account_id: id },
        )
        .await;
    }
    Ok(Json(serde_json::json!({})))
}

// ── POST /api/v1/admin/accounts/:id/action ───────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AccountActionForm {
    #[serde(rename = "type")]
    pub action_type: Option<String>,
    pub report_id: Option<FlexId>,
    pub warning_preset_id: Option<FlexId>,
    pub text: Option<String>,
    pub send_email_notification: Option<FlexBool>,
}

pub async fn account_action(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<AccountActionForm>,
) -> AppResult<Json<serde_json::Value>> {
    require_scope(&auth, true)?;
    let account = find_account(&state, id).await?;
    // `authorize @account, :show?`
    super::require_permission(&state, auth.account_id, flag::MANAGE_USERS).await?;
    account_action::save(
        &state,
        auth.account_id,
        &account,
        AccountAction {
            kind: form.action_type,
            report_id: form.report_id.map(|id| id.0),
            warning_preset_id: form.warning_preset_id.map(|id| id.0),
            text: form.text,
            send_email_notification: form.send_email_notification.is_none_or(|b| b.0),
            include_statuses: true,
        },
    )
    .await?;
    Ok(Json(serde_json::json!({})))
}
