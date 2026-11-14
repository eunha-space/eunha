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
//!
//! Mastodon: none

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
    /// Whose account to mint them into.
    #[serde(default, deserialize_with = "rails::opt_string")]
    pub account_id: Option<String>,
    /// Every eligible local member instead. Saying so is required: a request
    /// that merely leaves `account_id` out names nobody.
    #[serde(default, deserialize_with = "rails::bool")]
    pub all: bool,
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
    let named = req
        .account_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let account_id: Option<i64> = match (named, req.all) {
        (Some(_), true) => {
            return Err(AppError::Unprocessable(
                "Choose one member or all of them, not both".into(),
            ))
        }
        (None, false) => {
            return Err(AppError::Unprocessable(
                "Choose a member, or set all to grant to every member".into(),
            ))
        }
        (None, true) => None,
        (Some(id), false) => Some(
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

    // Membership, ownership, and bypass metadata are committed together. A
    // code that collides with an existing one is drawn again, as
    // `Invite#set_code` does, so the allowance is never reported partially.
    let mut tx = state.db.begin().await?;
    let grant_id: i64 = sqlx::query_scalar(
        "INSERT INTO eunha.invite_grants (granted_by_account_id) VALUES ($1) RETURNING id",
    )
    .bind(auth.account_id)
    .fetch_one(&mut *tx)
    .await?;
    // How many codes each user is still owed.
    let mut owed: std::collections::HashMap<i64, usize> =
        targets.iter().map(|t| (t.0, count as usize)).collect();
    let mut inserted: Vec<i64> = Vec::with_capacity(targets.len() * count as usize);
    for _ in 0..crate::api::mastodon::invites::CODE_ATTEMPTS {
        let owners: Vec<i64> = owed
            .iter()
            .flat_map(|(&user_id, &n)| std::iter::repeat_n(user_id, n))
            .collect();
        if owners.is_empty() {
            break;
        }
        let codes: Vec<String> = owners.iter().map(|_| generate_code()).collect();
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            r#"INSERT INTO public.invites
                 (user_id, code, max_uses, expires_at, autofollow, created_at, updated_at)
               SELECT t.user_id, t.code, $3, $4, false, now(), now()
               FROM unnest($1::bigint[], $2::text[]) AS t(user_id, code)
               ON CONFLICT (code) DO NOTHING RETURNING id, user_id"#,
        )
        .bind(&owners)
        .bind(&codes)
        .bind(max_uses)
        .bind(expires_at)
        .fetch_all(&mut *tx)
        .await?;
        // Whoever is still owed a code (a collision, or a code drawn twice
        // in this batch) gets another draw.
        for (id, user_id) in rows {
            inserted.push(id);
            if let Some(n) = owed.get_mut(&user_id) {
                *n -= 1;
                if *n == 0 {
                    owed.remove(&user_id);
                }
            }
        }
    }
    if !owed.is_empty() {
        return Err(AppError::Internal(anyhow::anyhow!("no free invite codes")));
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
        .route(
            "/api/eunha/v1/invite_grants",
            get(list_grants).post(grant_invites),
        )
        .route(
            "/api/eunha/v1/invite_grants/{id}/revoke",
            post(revoke_grant),
        )
        .route("/api/eunha/v1/invite_grants/recipients", get(recipients))
}

// Shared by the picker and the write: all functional local members, including staff.
async fn eligible_members(
    state: &AppState,
    account_id: Option<i64>,
) -> AppResult<Vec<(i64, i64, String)>> {
    Ok(sqlx::query_as(
        "SELECT s.user_id, a.id, a.username
         FROM eunha.user_standings s JOIN public.accounts a ON a.id = s.account_id
         WHERE a.domain IS NULL AND s.functional
           AND ($1::bigint IS NULL OR a.id = $1) ORDER BY a.username",
    )
    .bind(account_id)
    .fetch_all(&state.db)
    .await?)
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

/// One handout, as staff review them: who gave it, when, to how many, and
/// what has become of its links.
#[derive(Serialize, sqlx::FromRow)]
struct GrantSummary {
    #[sqlx(try_from = "i64")]
    id: GrantId,
    created_at: chrono::NaiveDateTime,
    granted_by: Option<String>,
    revoked_at: Option<chrono::NaiveDateTime>,
    revoked_by: Option<String>,
    expires_at: Option<chrono::NaiveDateTime>,
    /// The member's handle when the grant went to one member.
    recipient: Option<String>,
    recipients: i64,
    links: i64,
    uses: i64,
    available: i64,
}

#[derive(Serialize)]
#[serde(transparent)]
struct GrantId(String);

impl From<i64> for GrantId {
    fn from(id: i64) -> Self {
        Self(id.to_string())
    }
}

#[derive(Deserialize)]
struct GrantsQuery {
    #[serde(default, deserialize_with = "rails::opt_int")]
    max_id: Option<i64>,
}

const GRANTS_PER_PAGE: i64 = 40;

/// GET /api/eunha/v1/invite_grants: every handout, newest first, forty a page
/// (`?max_id=` for the next). This is the record of staff admitting people
/// through other members: Mastodon logs nothing about invites, and its
/// `admin_action_logs` cannot hold a target it has no class for.
async fn list_grants(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    axum::extract::Query(q): axum::extract::Query<GrantsQuery>,
) -> AppResult<Json<Vec<GrantSummary>>> {
    let Some(Extension(auth)) = auth else {
        return Err(AppError::Unauthorized);
    };
    auth.require_scope("read:accounts")?;
    admin::require_permission(&state, auth.account_id, admin::perm::MANAGE_INVITES).await?;
    let rows: Vec<GrantSummary> = sqlx::query_as(
        r#"SELECT g.id, g.created_at, giver.username AS granted_by, g.revoked_at,
                  revoker.username AS revoked_by,
                  max(i.expires_at) AS expires_at,
                  CASE WHEN count(DISTINCT i.user_id) = 1 THEN min(owner.username) END AS recipient,
                  count(DISTINCT i.user_id) AS recipients,
                  count(i.id) AS links,
                  COALESCE(sum(i.uses), 0)::bigint AS uses,
                  count(i.id) FILTER (WHERE (i.max_uses IS NULL OR i.uses < i.max_uses)
                                        AND (i.expires_at IS NULL OR i.expires_at >= now()))
                    AS available
           FROM eunha.invite_grants g
           LEFT JOIN public.accounts giver ON giver.id = g.granted_by_account_id
           LEFT JOIN public.accounts revoker ON revoker.id = g.revoked_by_account_id
           LEFT JOIN eunha.granted_invites gi ON gi.grant_id = g.id
           LEFT JOIN public.invites i ON i.id = gi.invite_id
           LEFT JOIN public.users ou ON ou.id = i.user_id
           LEFT JOIN public.accounts owner ON owner.id = ou.account_id
           WHERE ($1::bigint IS NULL OR g.id < $1)
           GROUP BY g.id, giver.username, revoker.username
           ORDER BY g.id DESC
           LIMIT $2"#,
    )
    .bind(q.max_id)
    .bind(GRANTS_PER_PAGE)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// POST /api/eunha/v1/invite_grants/:id/revoke: take a handout back. Its
/// links that can still be used expire now, as `DELETE /api/v1/invites/:id`
/// expires one (`Expireable#expire!`), so every row, use and lineage edge
/// stays; the grant records who revoked it.
async fn revoke_grant(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let Some(Extension(auth)) = auth else {
        return Err(AppError::Unauthorized);
    };
    auth.require_scope("write:accounts")?;
    admin::require_permission(&state, auth.account_id, admin::perm::MANAGE_INVITES).await?;
    let mut tx = state.db.begin().await?;
    let found = sqlx::query(
        "UPDATE eunha.invite_grants
         SET revoked_at = COALESCE(revoked_at, now()),
             revoked_by_account_id = COALESCE(revoked_by_account_id, $2)
         WHERE id = $1",
    )
    .bind(id)
    .bind(auth.account_id)
    .execute(&mut *tx)
    .await?;
    if found.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    let expired = sqlx::query(
        "UPDATE public.invites i SET expires_at = now(), updated_at = now()
         FROM eunha.granted_invites gi
         WHERE gi.invite_id = i.id AND gi.grant_id = $1
           AND (i.expires_at IS NULL OR i.expires_at >= now())",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(
        serde_json::json!({ "expired": expired.rows_affected() }),
    ))
}
