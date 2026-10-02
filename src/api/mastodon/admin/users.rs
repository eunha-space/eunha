//! Managing a local user from its account page: `Admin::Users::RolesController`,
//! `Admin::Users::TwoFactorAuthenticationsController`,
//! `Admin::ChangeEmailsController`, `Admin::ConfirmationsController` and
//! `Admin::ResetsController`, judged by `UserPolicy`.

use axum::{
    extract::{Extension, Path},
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::Params;
use super::accounts::AdminAccount;
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::{
        action_log::{self, Target},
        role::{self, authorize, flag, Role, EVERYONE_ROLE_ID},
    },
    state::AppState,
};

fn require_scope(auth: &AuthenticatedUser) -> AppResult<()> {
    auth.require_scope("admin:write:accounts")
}

struct UserRow {
    id: i64,
    account_id: i64,
    email: String,
    confirmed: bool,
    approved: bool,
    disabled: bool,
}

/// The local user behind an account, its log target, the acting role and the
/// user's own role. `require_local_account!` refuses a remote account.
struct Subject {
    user: UserRow,
    target: Target,
    acting: Role,
    role: Option<Role>,
}

async fn subject(
    state: &AppState,
    auth: &AuthenticatedUser,
    account_id: i64,
) -> AppResult<Subject> {
    let account = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        account_id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let user = sqlx::query_as!(
        UserRow,
        r#"SELECT id, account_id, email, (confirmed_at IS NOT NULL) AS "confirmed!", approved,
                  disabled
           FROM users WHERE account_id = $1"#,
        account_id
    )
    .fetch_optional(&state.db)
    .await?;
    let user = match user {
        Some(user) if account.is_local() => user,
        _ => return Err(AppError::Forbidden),
    };
    Ok(Subject {
        target: Target::user(user.id, account.id, account.acct()),
        acting: role::acting(&state.db, auth.account_id).await?,
        role: role::of_account(&state.db, account_id).await?,
        user,
    })
}

async fn render(state: &AppState, account_id: i64) -> AppResult<Json<AdminAccount>> {
    let account = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        account_id
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(super::build_admin_account(state, &account).await?))
}

// ── GET /api/v1/admin/roles ───────────────────────────────────────────────

/// `REST::RoleSerializer`, with the position the role form orders by.
#[derive(Debug, Serialize)]
pub struct AssignableRole {
    #[serde(flatten)]
    pub role: super::accounts::RoleEntity,
    pub position: i32,
}

/// `UserRole.assignable`: every role but the everyone role, lowest first, as
/// the change-role form offers them. Asks for `manage_roles`, which is
/// `UserRolePolicy#index?` and what changing a role needs.
pub async fn list_assignable_roles(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AssignableRole>>> {
    auth.require_scope("admin:read")?;
    super::require_permission(&state, auth.account_id, flag::MANAGE_ROLES).await?;
    let rows = sqlx::query!(
        r#"SELECT id, name, color, position, permissions, highlighted, collection_limit
           FROM user_roles WHERE id <> $1 ORDER BY position ASC"#,
        EVERYONE_ROLE_ID
    )
    .fetch_all(&state.db)
    .await?;
    let everyone = sqlx::query_scalar!(
        "SELECT permissions FROM user_roles WHERE id = $1",
        EVERYONE_ROLE_ID
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(flag::DEFAULT);
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let computed = if r.permissions & flag::ADMINISTRATOR != 0 {
                    flag::ALL
                } else {
                    r.permissions | everyone
                };
                AssignableRole {
                    role: super::accounts::RoleEntity::of(&Role {
                        id: Some(r.id),
                        name: r.name,
                        color: r.color,
                        position: r.position,
                        permissions: r.permissions,
                        highlighted: r.highlighted,
                        collection_limit: r.collection_limit,
                        computed,
                    }),
                    position: r.position,
                }
            })
            .collect(),
    ))
}

// ── PUT /api/v1/admin/accounts/:id/role ───────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ChangeRoleForm {
    /// Empty or absent for no role.
    pub role_id: Option<serde_json::Value>,
}

/// `Admin::Users::RolesController#update`: `UserPolicy#change_role?` (a role
/// that may manage roles, over the user's current one), then
/// `validate_role_elevation`: the new role may not outrank the actor's.
pub async fn change_user_role(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<ChangeRoleForm>,
) -> AppResult<Json<AdminAccount>> {
    require_scope(&auth)?;
    let s = subject(&state, &auth, id).await?;
    authorize(s.acting.can(&[flag::MANAGE_ROLES]) && s.acting.overrides(s.role.as_ref()))?;
    let role_id: Option<i64> =
        match &form.role_id {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(v)) if v.trim().is_empty() => None,
            Some(serde_json::Value::String(v)) => Some(v.trim().parse().map_err(|_| {
                AppError::Unprocessable("Validation failed: Role must exist".into())
            })?),
            Some(serde_json::Value::Number(n)) => n.as_i64(),
            Some(_) => None,
        };
    // `sanitize_role`: the everyone role is no role.
    let role_id = role_id.filter(|id| *id != EVERYONE_ROLE_ID);
    if let Some(role_id) = role_id {
        let position =
            sqlx::query_scalar!("SELECT position FROM user_roles WHERE id = $1", role_id)
                .fetch_optional(&state.db)
                .await?
                .ok_or_else(|| {
                    AppError::Unprocessable("Validation failed: Role must exist".into())
                })?;
        // `role&.overrides?(@current_account&.user_role)`.
        if position > s.acting.position {
            return Err(AppError::Unprocessable(
                "Validation failed: Role cannot be higher than your current role".into(),
            ));
        }
    }
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "UPDATE users SET role_id = $2, updated_at = now() WHERE id = $1",
        s.user.id,
        role_id
    )
    .execute(&mut *tx)
    .await?;
    action_log::log(&mut *tx, auth.account_id, "change_role", &s.target).await?;
    tx.commit().await?;
    render(&state, id).await
}

// ── DELETE /api/v1/admin/accounts/:id/two_factor_authentication ───────────

/// `Admin::Users::TwoFactorAuthenticationsController#destroy`:
/// `User#disable_two_factor!`, logged, and the user told by mail. Eunha has no
/// two-factor sign-in of its own; this clears what a Mastodon database holds.
pub async fn disable_user_two_factor(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_scope(&auth)?;
    let s = subject(&state, &auth, id).await?;
    // `UserPolicy#disable_2fa?`
    authorize(s.acting.can(&[flag::MANAGE_USER_ACCESS]) && s.acting.overrides(s.role.as_ref()))?;
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        r#"UPDATE users SET otp_required_for_login = false, otp_secret = NULL,
                  otp_backup_codes = CASE WHEN otp_backup_codes IS NULL THEN NULL ELSE '{}'::varchar[] END,
                  updated_at = now()
           WHERE id = $1"#,
        s.user.id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM webauthn_credentials WHERE user_id = $1",
        s.user.id
    )
    .execute(&mut *tx)
    .await?;
    action_log::log(&mut *tx, auth.account_id, "disable_2fa", &s.target).await?;
    tx.commit().await?;
    // `UserMailer.two_factor_disabled`, which `active_for_authentication?`
    // keeps from a user who cannot sign in anyway.
    if s.user.confirmed && s.user.approved && !s.user.disabled {
        let email = state.email.clone();
        let to = s.user.email.clone();
        let domain = state.instance.domain.clone();
        crate::tenants::spawn(async move {
            if let Err(error) = email.send_two_factor_disabled(&to, &domain).await {
                tracing::warn!(%error, "could not send a two-factor disabled email");
            }
        });
    }
    render(&state, id).await
}

// ── POST /api/v1/admin/accounts/:id/change_email ──────────────────────────

#[derive(Debug, Deserialize)]
pub struct ChangeEmailForm {
    pub unconfirmed_email: Option<String>,
}

/// `Admin::ChangeEmailsController#update`: a different address waits in
/// `unconfirmed_email` until the user follows the link mailed to it.
pub async fn change_user_email(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<ChangeEmailForm>,
) -> AppResult<Json<AdminAccount>> {
    require_scope(&auth)?;
    let s = subject(&state, &auth, id).await?;
    // `UserPolicy#change_email?`
    authorize(s.acting.can(&[flag::MANAGE_USER_ACCESS]) && s.acting.overrides(s.role.as_ref()))?;
    // `resource_params.fetch(:unconfirmed_email)`.
    let new_email = form
        .unconfirmed_email
        .filter(|e| !e.trim().is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("param is missing or the value is empty: unconfirmed_email".into())
        })?;
    if new_email != s.user.email {
        let mut tx = state.db.begin().await?;
        sqlx::query!(
            r#"UPDATE users SET unconfirmed_email = $2, confirmation_token = NULL,
                      updated_at = now()
               WHERE id = $1"#,
            s.user.id,
            new_email
        )
        .execute(&mut *tx)
        .await?;
        action_log::log(&mut *tx, auth.account_id, "change_email", &s.target).await?;
        tx.commit().await?;
        crate::accounts::send_confirmation_instructions(&state, s.user.id).await?;
    }
    render(&state, id).await
}

// ── POST /api/v1/admin/accounts/:id/confirmation ──────────────────────────

/// `Admin::ConfirmationsController#create`: `UserPolicy#confirm?` (only an
/// unconfirmed user), then `mark_email_as_confirmed!`.
pub async fn confirm_user(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_scope(&auth)?;
    let s = subject(&state, &auth, id).await?;
    authorize(s.acting.can(&[flag::MANAGE_USER_ACCESS]) && !s.user.confirmed)?;
    crate::accounts::confirm_user(&state, s.user.id, false).await?;
    action_log::log(&state.db, auth.account_id, "confirm", &s.target).await?;
    render(&state, id).await
}

// ── POST /api/v1/admin/accounts/:id/confirmation/resend ───────────────────

/// `Admin::ConfirmationsController#resend`: an unconfirmed user is mailed the
/// confirmation link again.
pub async fn resend_user_confirmation(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_scope(&auth)?;
    let s = subject(&state, &auth, id).await?;
    // `redirect_confirmed_user`, before the policy is asked.
    if s.user.confirmed {
        return Err(AppError::Unprocessable(
            "This user is already confirmed".into(),
        ));
    }
    authorize(s.acting.can(&[flag::MANAGE_USER_ACCESS]))?;
    crate::accounts::send_confirmation_instructions(&state, s.user.id).await?;
    action_log::log(&state.db, auth.account_id, "resend", &s.target).await?;
    render(&state, id).await
}

// ── POST /api/v1/admin/accounts/:id/reset ─────────────────────────────────

/// `Admin::ResetsController#create`: `User#reset_password!` signs the user
/// out everywhere with a random password, then mails a reset link.
pub async fn reset_user_password(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAccount>> {
    require_scope(&auth)?;
    let s = subject(&state, &auth, id).await?;
    // `UserPolicy#reset_password?`
    authorize(s.acting.can(&[flag::MANAGE_USER_ACCESS]) && s.acting.overrides(s.role.as_ref()))?;
    crate::accounts::change_password(&state.db, s.user.id).await?;
    // `revoke_access!` kills the user's streaming connections.
    state.streaming.publish(crate::streaming::Event::Kill {
        account_id: s.user.account_id,
    });
    crate::accounts::send_reset_password_instructions(&state, s.user.id).await?;
    action_log::log(&state.db, auth.account_id, "reset_password", &s.target).await?;
    render(&state, id).await
}
