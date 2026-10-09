//! Handing invites to somebody else.
//!
//! Mastodon has no equivalent. There an invite is made by the person who will
//! hand it out, and `invite_users` is the whole of the question — either a
//! member may make as many as they like, or none at all. An instance that wants
//! to say "you may bring two people" has nothing to say it with.
//!
//! So an admin here mints the codes *into the member's own account*: they
//! appear on that member's invite page for them to pass on, and whoever signs
//! up through one lands under **them** in the invite tree rather than under the
//! admin who minted it. The count is the limit — there is no allowance to keep
//! books on, because the codes themselves are the allowance.

use axum::{
    routing::{get, post},
    Extension, Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    api::mastodon::{
        admin,
        extractors::{rails, Params},
        invites::generate_code,
    },
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

/// Codes minted per account in one request. Nothing wants more, and a mistyped
/// count across a whole userbase should not write ten thousand rows.
const MAX_COUNT: i32 = 25;
/// The largest of Mastodon's `Invite::MAX_USES_COUNTS`.
const MAX_USES: i32 = 100;

/// A form or a JSON body, read as Rails params are.
#[derive(Debug, Deserialize)]
pub struct GrantRequest {
    /// Whose account to mint them into. Absent means every local member.
    #[serde(default, deserialize_with = "rails::opt_string")]
    pub account_id: Option<String>,
    /// How many codes each of those accounts gets.
    #[serde(default, deserialize_with = "rails::opt_i32")]
    pub count: Option<i32>,
    /// Uses per code. One by default: "three invites" should mean three people.
    #[serde(default, deserialize_with = "rails::opt_i32")]
    pub max_uses: Option<i32>,
    /// Seconds until the codes expire; absent for never.
    #[serde(default, deserialize_with = "rails::opt_int")]
    pub expires_in: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct GrantResponse {
    /// Codes created.
    pub granted: i64,
    /// Accounts they were created for.
    pub accounts: i64,
}

/// POST /api/eunha/v1/invite_grants
pub async fn grant_invites(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Params(req): Params<GrantRequest>,
) -> AppResult<Json<GrantResponse>> {
    let Some(Extension(auth)) = auth else {
        return Err(AppError::Unauthorized);
    };
    auth.require_scope("write:accounts")?;
    // `manage_invites` is the permission Mastodon uses for acting on invites
    // that are not your own, which is what this does to the furthest extent:
    // it creates them.
    admin::require_permission(&state, auth.account_id, admin::perm::MANAGE_INVITES).await?;

    let count = req.count.unwrap_or(0);
    if !(1..=MAX_COUNT).contains(&count) {
        return Err(AppError::Unprocessable(format!(
            "Count must be between 1 and {MAX_COUNT}"
        )));
    }
    let max_uses = req.max_uses.unwrap_or(1);
    if !(1..=MAX_USES).contains(&max_uses) {
        return Err(AppError::Unprocessable(format!(
            "Uses per invite must be between 1 and {MAX_USES}"
        )));
    }
    let account_id: Option<i64> = match req.account_id.as_deref().map(str::trim) {
        None => None,
        Some(id) => Some(
            id.parse()
                .map_err(|_| AppError::Unprocessable("Invalid account id".into()))?,
        ),
    };
    if req.expires_in.is_some_and(|s| s <= 0 || s > 31_536_000) {
        return Err(AppError::Unprocessable(
            "Expiry must be between 1 second and 1 year".into(),
        ));
    }
    let expires_at = req
        .expires_in
        .map(|s| chrono::Utc::now().naive_utc() + chrono::Duration::seconds(s));

    // Functional local members only. An unconfirmed or
    // unapproved signup has not joined yet, and a suspended one has left.
    let targets = eligible_members(&state, account_id).await?;

    if targets.is_empty() {
        return Err(match account_id {
            Some(_) => AppError::NotFound,
            None => AppError::Unprocessable("This instance has no members yet".into()),
        });
    }

    // One row per code, built here rather than in a loop of statements: 25
    // codes across a whole userbase is a single insert either way.
    let mut user_ids = Vec::with_capacity(targets.len() * count as usize);
    let mut codes = Vec::with_capacity(targets.len() * count as usize);
    for user_id in &targets {
        for _ in 0..count {
            user_ids.push(user_id.0);
            codes.push(generate_code());
        }
    }

    // Membership, ownership, and bypass metadata are committed together. If a
    // random code collides, roll back rather than report a partial allowance.
    let mut tx = state.db.begin().await?;
    let grant_id: i64 = sqlx::query_scalar(
        "INSERT INTO eunha.invite_grants (granted_by_account_id) VALUES ($1) RETURNING id",
    )
    .bind(auth.account_id)
    .fetch_one(&mut *tx)
    .await?;
    let inserted: Vec<i64> = sqlx::query_scalar(
        r#"INSERT INTO public.invites
             (user_id, code, max_uses, expires_at, autofollow, created_at, updated_at)
           SELECT t.user_id, t.code, $3, $4, false, now(), now()
           FROM unnest($1::bigint[], $2::text[]) AS t(user_id, code)
           ON CONFLICT (code) DO NOTHING RETURNING id"#,
    )
    .bind(&user_ids)
    .bind(&codes)
    .bind(max_uses)
    .bind(expires_at)
    .fetch_all(&mut *tx)
    .await?;
    if inserted.len() != codes.len() {
        return Err(AppError::Unprocessable(
            "An invite code collided. Please retry the grant.".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO eunha.granted_invites (invite_id, grant_id) SELECT unnest($1::bigint[]), $2",
    )
    .bind(&inserted)
    .bind(grant_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let granted = inserted.len() as i64;

    Ok(Json(GrantResponse {
        granted,
        accounts: targets.len() as i64,
    }))
}

pub fn routes() -> Router {
    Router::new()
        .route("/api/eunha/v1/invite_grants", post(grant_invites))
        .route("/api/eunha/v1/invite_grants/recipients", get(recipients))
}

// Shared by the picker and the write: all functional local members, including staff.
async fn eligible_members(
    state: &AppState,
    account_id: Option<i64>,
) -> AppResult<Vec<(i64, i64, String)>> {
    Ok(sqlx::query_as(
        "SELECT u.id, a.id, a.username FROM public.users u JOIN public.accounts a ON a.id = u.account_id
         LEFT JOIN public.user_roles r ON r.id = COALESCE(u.role_id, -99)
         WHERE a.domain IS NULL AND u.approved AND u.confirmed_at IS NOT NULL AND NOT u.disabled
           AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
           AND NOT a.memorial AND a.moved_to_account_id IS NULL
           AND NOT (COALESCE(r.require_2fa, false) AND NOT u.otp_required_for_login
                    AND NOT EXISTS (SELECT 1 FROM public.webauthn_credentials w WHERE w.user_id = u.id))
           AND ($1::bigint IS NULL OR a.id = $1) ORDER BY a.username",
    ).bind(account_id).fetch_all(&state.db).await?)
}

#[derive(Serialize)]
struct Recipient {
    id: String,
    acct: String,
}

async fn recipients(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Vec<Recipient>>> {
    let Some(Extension(auth)) = auth else {
        return Err(AppError::Unauthorized);
    };
    auth.require_scope("read:accounts")?;
    admin::require_permission(&state, auth.account_id, admin::perm::MANAGE_INVITES).await?;
    Ok(Json(
        eligible_members(&state, None)
            .await?
            .into_iter()
            .map(|(_, id, acct)| Recipient {
                id: id.to_string(),
                acct,
            })
            .collect(),
    ))
}
