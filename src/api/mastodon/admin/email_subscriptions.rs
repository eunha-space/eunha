//! What Mastodon's admin pages for email subscriptions do, as REST:
//! `Admin::EmailSubscriptionsController`, `Admin::EmailSubscriptions::SetupsController`,
//! `AdditionalFooterTextsController` and `AccountsController`. Every one of
//! them authorizes with `EmailSubscriptionPolicy`, which is `manage_settings`.
//!
//! Mastodon serves these as web forms only; eunha's admin is a single-page
//! app, so it needs an API (the `email-subscriptions-rest-api` divergence).

use axum::{
    extract::{Extension, Path},
    http::{HeaderMap, Uri},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::{FlexBool, Params};
use super::{perm, require_permission, PageParams};
use crate::{
    db::models::Account,
    email_subscriptions as subs,
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

async fn authorize(state: &AppState, auth: &AuthenticatedUser, write: bool) -> AppResult<()> {
    auth.require_scope(if write { "admin:write" } else { "admin:read" })?;
    require_permission(state, auth.account_id, perm::MANAGE_SETTINGS).await
}

/// `require_enabled!`, which raises a routing error: a 404.
fn require_available(state: &AppState) -> AppResult<()> {
    if subs::available(state) {
        Ok(())
    } else {
        Err(AppError::NotFound)
    }
}

/// A role that may offer subscriptions, as the index lists it.
#[derive(Debug, Serialize)]
pub struct RoleEntry {
    pub id: String,
    pub name: String,
    pub color: String,
    /// `role.users.count`.
    pub accounts: i64,
}

/// An account with subscribers, as the index lists it.
#[derive(Debug, Serialize)]
pub struct AccountEntry {
    pub account: crate::api::mastodon::types::Account,
    /// See [`subs::admin_status`].
    pub status: &'static str,
    /// Every subscription, confirmed or not, as Mastodon counts them.
    pub subscribers: i64,
    pub last_status_at: Option<String>,
}

/// `Admin::EmailSubscriptionsController#index`'s page.
#[derive(Debug, Serialize)]
pub struct Overview {
    /// `config.x.email_subscriptions`: whether the feature can be enabled.
    pub available: bool,
    /// `Setting.email_subscriptions`.
    pub enabled: bool,
    /// `Setting.email_footer_text`.
    pub email_footer_text: String,
    pub roles: Vec<RoleEntry>,
    pub accounts: Vec<AccountEntry>,
}

async fn account_entry(state: &AppState, account: &Account) -> AppResult<AccountEntry> {
    let subscribers = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM email_subscriptions WHERE account_id = $1"#,
        account.id
    )
    .fetch_one(&state.db)
    .await?;
    let last_status_at = sqlx::query_scalar!(
        "SELECT last_status_at FROM account_stats WHERE account_id = $1",
        account.id
    )
    .fetch_optional(&state.db)
    .await?
    .flatten();
    Ok(AccountEntry {
        account: crate::api::mastodon::accounts::account_to_api(state, account).await,
        status: subs::admin_status(state, account.id).await,
        subscribers,
        last_status_at: last_status_at.map(|t| t.and_utc().to_rfc3339()),
    })
}

async fn overview(state: &AppState) -> AppResult<Overview> {
    // `UserRole.where('permissions & ? != 0', manage_email_subscriptions | administrator)`
    let roles = sqlx::query!(
        r#"SELECT r.id, r.name, r.color,
                  (SELECT count(*) FROM users u WHERE u.role_id = r.id) AS "accounts!"
           FROM user_roles r
           WHERE r.permissions & $1 <> 0
           ORDER BY r.position DESC, r.id"#,
        perm::MANAGE_EMAIL_SUBSCRIPTIONS | perm::ADMINISTRATOR,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| RoleEntry {
        id: r.id.to_string(),
        name: r.name,
        color: r.color,
        accounts: r.accounts,
    })
    .collect();
    // `Account.local.where.associated(:email_subscriptions)`
    let accounts = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts a
         WHERE a.domain IS NULL
           AND EXISTS (SELECT 1 FROM email_subscriptions s WHERE s.account_id = a.id)
         ORDER BY a.id"
    )
    .fetch_all(&state.db)
    .await?;
    let mut entries = vec![];
    for account in &accounts {
        entries.push(account_entry(state, account).await?);
    }
    Ok(Overview {
        available: subs::available(state),
        enabled: crate::settings::boolean(state, subs::SETTING).await,
        email_footer_text: crate::settings::string(state, subs::FOOTER_SETTING).await,
        roles,
        accounts: entries,
    })
}

/// GET /api/v1/admin/email_subscriptions
pub async fn show_email_subscriptions(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Overview>> {
    authorize(&state, &auth, false).await?;
    Ok(Json(overview(&state).await?))
}

/// `Form::EmailSubscriptionsConfirmation`.
#[derive(Debug, Deserialize)]
pub struct SetupForm {
    pub agreement_email_volume: Option<FlexBool>,
    pub agreement_privacy_and_terms: Option<FlexBool>,
}

/// POST /api/v1/admin/email_subscriptions/setup: `SetupsController#create`,
/// which enables the feature once both agreements are accepted.
pub async fn setup_email_subscriptions(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<SetupForm>,
) -> AppResult<Response> {
    require_available(&state)?;
    authorize(&state, &auth, true).await?;
    // `validates ..., acceptance: true`
    let mut errors = subs::ValidationErrors::default();
    if !form.agreement_email_volume.is_some_and(|b| b.0) {
        errors.add("agreement_email_volume", "accepted", "must be accepted");
    }
    if !form.agreement_privacy_and_terms.is_some_and(|b| b.0) {
        errors.add(
            "agreement_privacy_and_terms",
            "accepted",
            "must be accepted",
        );
    }
    if !errors.is_empty() {
        return Ok(errors.into_response());
    }
    crate::settings::set(&state, subs::SETTING, true.into()).await?;
    Ok(Json(overview(&state).await?).into_response())
}

/// POST /api/v1/admin/email_subscriptions/disable
pub async fn disable_email_subscriptions(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Overview>> {
    authorize(&state, &auth, true).await?;
    crate::settings::set(&state, subs::SETTING, false.into()).await?;
    Ok(Json(overview(&state).await?))
}

/// POST /api/v1/admin/email_subscriptions/purge: every subscriber of every
/// account, erased.
pub async fn purge_email_subscriptions(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Overview>> {
    authorize(&state, &auth, true).await?;
    subs::purge(&state).await?;
    Ok(Json(overview(&state).await?))
}

/// DELETE /api/v1/admin/email_subscriptions/:id: one subscriber removed.
pub async fn delete_email_subscription(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    authorize(&state, &auth, true).await?;
    let deleted = sqlx::query!("DELETE FROM email_subscriptions WHERE id = $1", id)
        .execute(&state.db)
        .await?
        .rows_affected();
    if deleted == 0 {
        return Err(AppError::NotFound);
    }
    Ok(Json(serde_json::json!({})))
}

#[derive(Debug, Deserialize)]
pub struct FooterForm {
    pub email_footer_text: Option<String>,
}

/// PUT /api/v1/admin/email_subscriptions/additional_footer_text
pub async fn update_email_footer_text(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<FooterForm>,
) -> AppResult<Json<Overview>> {
    authorize(&state, &auth, true).await?;
    let text = form.email_footer_text.unwrap_or_default();
    crate::settings::set(&state, subs::FOOTER_SETTING, text.into()).await?;
    Ok(Json(overview(&state).await?))
}

async fn find_account(state: &AppState, id: i64) -> AppResult<Account> {
    sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

/// GET /api/v1/admin/email_subscriptions/accounts/:id
pub async fn show_email_subscription_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AccountEntry>> {
    require_available(&state)?;
    let account = find_account(&state, id).await?;
    authorize(&state, &auth, false).await?;
    Ok(Json(account_entry(&state, &account).await?))
}

/// A subscriber, as the account page lists it.
#[derive(Debug, Serialize)]
pub struct Subscriber {
    pub id: String,
    pub email: String,
    pub created_at: String,
    pub confirmed_at: Option<String>,
}

/// `LIMIT` per page of subscribers, at most twice it.
const LIMIT: i64 = 40;

/// GET /api/v1/admin/email_subscriptions/accounts/:id/subscriptions
pub async fn list_email_subscription_subscribers(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    uri: Uri,
    req_headers: HeaderMap,
    Params(p): Params<PageParams>,
) -> AppResult<Response> {
    require_available(&state)?;
    let account = find_account(&state, id).await?;
    authorize(&state, &auth, false).await?;
    let rows = sqlx::query!(
        r#"SELECT id, email, created_at, confirmed_at FROM email_subscriptions
           WHERE account_id = $1
             AND ($2::bigint IS NULL OR id < $2) AND ($3::bigint IS NULL OR id > $3)
             AND ($4::bigint IS NULL OR id > $4)
           ORDER BY CASE WHEN $4::bigint IS NULL THEN -id ELSE id END LIMIT $5"#,
        account.id,
        p.max_id.map(|i| i.0),
        p.since_id.map(|i| i.0),
        p.min_id.map(|i| i.0),
        p.limit(LIMIT, LIMIT * 2),
    )
    .fetch_all(&state.db)
    .await?;
    let mut items: Vec<Subscriber> = rows
        .into_iter()
        .map(|r| Subscriber {
            id: r.id.to_string(),
            email: r.email,
            created_at: r.created_at.and_utc().to_rfc3339(),
            confirmed_at: r.confirmed_at.map(|t| t.and_utc().to_rfc3339()),
        })
        .collect();
    if p.min_id.is_some() {
        items.reverse();
    }
    let first = items.first().map(|s| s.id.clone());
    let last = items.last().map(|s| s.id.clone());
    let headers =
        super::super::link_headers(&req_headers, &uri, first.as_deref().zip(last.as_deref()));
    Ok((headers, Json(items)).into_response())
}

async fn set_account_enabled(
    state: AppState,
    auth: AuthenticatedUser,
    id: i64,
    on: bool,
) -> AppResult<Json<AccountEntry>> {
    require_available(&state)?;
    let account = find_account(&state, id).await?;
    authorize(&state, &auth, true).await?;
    if !subs::set_user_enabled(&state, account.id, on).await? {
        // `@account.user.settings`: an account without a user cannot.
        return Err(AppError::NotFound);
    }
    Ok(Json(account_entry(&state, &account).await?))
}

/// POST /api/v1/admin/email_subscriptions/accounts/:id/enable
pub async fn enable_email_subscription_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AccountEntry>> {
    set_account_enabled(state, auth, id, true).await
}

/// POST /api/v1/admin/email_subscriptions/accounts/:id/disable
pub async fn disable_email_subscription_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AccountEntry>> {
    set_account_enabled(state, auth, id, false).await
}
