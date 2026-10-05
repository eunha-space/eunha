use crate::{
    error::{AppError, AppResult},
    middleware::{AuthenticatedUser, ResolvedInstance},
    state::AppState,
};
use axum::{
    extract::{Extension, Path},
    http::StatusCode,
    Json,
};
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct InviteResponse {
    pub id: String,
    pub code: String,
    pub expires_at: Option<NaiveDateTime>,
    pub max_uses: Option<i32>,
    pub uses: i32,
    pub url: String,
    pub autofollow: bool,
    pub comment: Option<String>,
    pub created_at: NaiveDateTime,
}

pub async fn list_invites(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<InviteResponse>>> {
    auth.require_scope("read:accounts")?;
    // No permission check, where Mastodon's `InvitesController#index`
    // authorizes `:invite, :create?`. There the list is the page you create
    // invites from, so losing the permission takes the page with it; here an
    // admin can mint codes *into* a member's account, and a member who cannot
    // create one still has to be able to read what they were given.
    let rows = sqlx::query!(
        r#"SELECT id, code, expires_at, max_uses, uses, autofollow, comment, created_at
           FROM invites
           WHERE user_id = (SELECT id FROM users WHERE account_id = $1)
           ORDER BY created_at DESC"#,
        auth.account_id,
    )
    .fetch_all(&state.db)
    .await?;

    let invites = rows
        .into_iter()
        .map(|r| InviteResponse {
            url: invite_url(&instance.domain, &r.code),
            id: r.id.to_string(),
            code: r.code,
            expires_at: r.expires_at,
            max_uses: r.max_uses,
            uses: r.uses,
            autofollow: r.autofollow,
            comment: r.comment,
            created_at: r.created_at,
        })
        .collect();

    Ok(Json(invites))
}

/// Mastodon Invite::COMMENT_SIZE_LIMIT.
const COMMENT_SIZE_LIMIT: usize = 420;

#[derive(Debug, Deserialize, Default)]
pub struct CreateInviteRequest {
    pub max_uses: Option<i32>,
    /// Seconds from now until expiry; None = never expires.
    pub expires_in: Option<i64>,
    /// Auto-follow the inviter when the new account is created.
    #[serde(default)]
    pub autofollow: bool,
    pub comment: Option<String>,
}

pub async fn create_invite(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    body: Option<Json<CreateInviteRequest>>,
) -> AppResult<Json<InviteResponse>> {
    auth.require_scope("write:accounts")?;
    require_invite_users(&state, auth.account_id).await?;
    let req = body.map(|Json(b)| b).unwrap_or_default();

    let comment = req.comment.filter(|c| !c.is_empty());
    if comment
        .as_ref()
        .is_some_and(|c| c.chars().count() > COMMENT_SIZE_LIMIT)
    {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: Comment is too long (maximum is {COMMENT_SIZE_LIMIT} characters)"
        )));
    }

    let code = generate_code();
    let expires_at = req
        .expires_in
        .map(|s| chrono::Utc::now().naive_utc() + chrono::Duration::seconds(s));

    let row = sqlx::query!(
        r#"INSERT INTO invites (code, user_id, max_uses, expires_at, autofollow, comment, created_at, updated_at)
           VALUES ($1, (SELECT id FROM users WHERE account_id = $2), $3, $4, $5, $6, now(), now())
           RETURNING id, code, expires_at, max_uses, uses, autofollow, comment, created_at"#,
        code,
        auth.account_id,
        req.max_uses,
        expires_at,
        req.autofollow,
        comment,
    )
    .fetch_one(&state.db)
    .await?;

    Ok(Json(InviteResponse {
        url: invite_url(&instance.domain, &row.code),
        id: row.id.to_string(),
        code: row.code,
        expires_at: row.expires_at,
        max_uses: row.max_uses,
        uses: row.uses,
        autofollow: row.autofollow,
        comment: row.comment,
        created_at: row.created_at,
    }))
}

pub async fn delete_invite(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    auth.require_scope("write:accounts")?;
    // Mastodon's `InvitePolicy#destroy?` is `owner? || role.can?(:manage_invites)`:
    // your own invites always, anyone's with the moderation permission.
    let manages_invites = super::admin::require_permission(
        &state,
        auth.account_id,
        super::admin::perm::MANAGE_INVITES,
    )
    .await
    .is_ok();

    // Match Mastodon's InvitesController#destroy, which calls Expireable#expire!
    // (`touch(:expires_at)`) rather than deleting the row — this keeps the invite
    // around so `users.invite_id` edges (and the invite tree) survive.
    let expired = sqlx::query!(
        "UPDATE invites SET expires_at = now(), updated_at = now()
         WHERE id = $1
           AND ($3::boolean OR user_id = (SELECT id FROM users WHERE account_id = $2))",
        id,
        auth.account_id,
        manages_invites,
    )
    .execute(&state.db)
    .await?;

    if expired.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }

    Ok(StatusCode::OK)
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Mastodon's `InvitePolicy#create?`: `role.can?(:invite_users)`.
///
/// The permission is on the everyone role by default (`Flags::DEFAULT`), so out
/// of the box every member may invite, as upstream. An instance that would
/// rather hand invites out itself clears that bit on the everyone role
/// (`user_roles` id -99) and leaves it to staff, whose own roles carry it —
/// this is a setting, not a feature that gets turned off.
async fn require_invite_users(state: &AppState, account_id: i64) -> AppResult<()> {
    super::admin::require_permission(state, account_id, super::admin::perm::INVITE_USERS).await
}

pub fn generate_code() -> String {
    use rand::Rng;
    // Mastodon VALID_CODE_CHARACTERS: a-z A-Z 0-9 minus the homoglyphs 0 1 I l O,
    // sampled into an 8-character code.
    const CHARS: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    (0..8)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

pub fn invite_url(domain: &str, code: &str) -> String {
    format!("https://{domain}/signup?invite={code}")
}
