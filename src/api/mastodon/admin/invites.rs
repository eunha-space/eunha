//! `Admin::InvitesController#index` and `#deactivate_all`: every invite on the
//! server, for `manage_invites`. Creating and expiring one are the invites
//! API's own (`/api/v1/invites`), which already asks what Mastodon's
//! controller asks.

use axum::{
    extract::{Extension, Query},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{
    error::AppResult,
    middleware::{AuthenticatedUser, ResolvedInstance},
    moderation::role::flag,
    state::AppState,
};

/// Kaminari's `default_per_page`.
const PER_PAGE: i64 = 40;

/// An invite as the admin invites page shows it.
#[derive(Debug, Serialize)]
pub struct AdminInvite {
    pub id: String,
    pub code: String,
    pub url: String,
    pub uses: i32,
    pub max_uses: Option<i32>,
    pub expires_at: Option<String>,
    pub expired: bool,
    /// `valid_for_use?`: uses left, unexpired, and its inviter functional.
    pub valid_for_use: bool,
    pub autofollow: bool,
    pub comment: Option<String>,
    pub created_at: String,
    /// Who made it.
    pub account: Option<super::super::types::Account>,
}

#[derive(Debug, Deserialize, Default)]
pub struct InviteFilter {
    pub available: Option<String>,
    pub expired: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_int"
    )]
    pub page: Option<i64>,
}

/// `GET /api/v1/admin/invites`: `InviteFilter`, newest first, forty a page.
pub async fn list_admin_invites(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(filter): Query<InviteFilter>,
) -> AppResult<Json<Vec<AdminInvite>>> {
    auth.require_scope("admin:read")?;
    super::require_permission(&state, auth.account_id, flag::MANAGE_INVITES).await?;
    let present = |v: &Option<String>| v.as_deref().is_some_and(|v| !v.trim().is_empty());
    let page = filter.page.unwrap_or(1).max(1);
    let rows = sqlx::query!(
        r#"SELECT i.id, i.code, i.uses, i.max_uses, i.expires_at, i.autofollow, i.comment,
                  i.created_at, u.account_id,
                  (i.expires_at IS NOT NULL AND i.expires_at < now()) AS "expired!",
                  (u.confirmed_at IS NOT NULL AND u.approved AND NOT u.disabled
                   AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                   AND NOT a.memorial AND a.moved_to_account_id IS NULL
                   AND NOT (COALESCE(r.require_2fa, false) AND NOT u.otp_required_for_login
                            AND NOT EXISTS (SELECT 1 FROM webauthn_credentials w
                                            WHERE w.user_id = u.id))) AS "functional!"
           FROM invites i
           JOIN users u ON u.id = i.user_id
           JOIN accounts a ON a.id = u.account_id
           LEFT JOIN user_roles r ON r.id = COALESCE(u.role_id, -99)
           WHERE (NOT $1 OR i.expires_at IS NULL OR i.expires_at >= now())
             AND (NOT $2 OR (i.expires_at IS NOT NULL AND i.expires_at < now()))
           ORDER BY i.created_at DESC
           LIMIT $3 OFFSET $4"#,
        present(&filter.available),
        present(&filter.expired),
        PER_PAGE,
        (page - 1) * PER_PAGE,
    )
    .fetch_all(&state.db)
    .await?;
    let mut invites = Vec::with_capacity(rows.len());
    for r in rows {
        invites.push(AdminInvite {
            id: r.id.to_string(),
            url: super::super::invites::invite_url(&instance.domain, &r.code),
            code: r.code,
            uses: r.uses,
            max_uses: r.max_uses,
            expires_at: r.expires_at.map(super::super::convert::mastodon_date),
            valid_for_use: r.max_uses.is_none_or(|max| r.uses < max) && !r.expired && r.functional,
            expired: r.expired,
            autofollow: r.autofollow,
            comment: r.comment,
            created_at: super::super::convert::mastodon_date(r.created_at),
            account: super::api_account(&state, r.account_id).await?,
        });
    }
    Ok(Json(invites))
}

/// `POST /api/v1/admin/invites/deactivate_all`: `Invite.available
/// .touch_all(:expires_at)`, every usable invite expired now.
pub async fn deactivate_all_invites(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("admin:write")?;
    super::require_permission(&state, auth.account_id, flag::MANAGE_INVITES).await?;
    sqlx::query!(
        r#"UPDATE invites SET expires_at = now(), updated_at = now()
           WHERE expires_at IS NULL OR expires_at >= now()"#
    )
    .execute(&state.db)
    .await?;
    Ok(Json(serde_json::json!({})))
}
