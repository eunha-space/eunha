//! Account migration over REST: moving to a new account, redirecting to one,
//! and managing the `alsoKnownAs` aliases. Mastodon serves these as web
//! settings forms; the work is [`crate::moves`], and these only translate.

use super::*;

// ── POST /api/v1/accounts/move ────────────────────────────────────────────

/// `Settings::MigrationsController#create`.
pub async fn move_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<crate::moves::MoveForm>,
) -> AppResult<Json<crate::moves::Migration>> {
    auth.require_scope("write:accounts")?;
    crate::moves::create_migration(&state, auth.account_id, &form)
        .await
        .map(Json)
}

// ── POST/DELETE /api/v1/accounts/redirect ─────────────────────────────────

/// `Settings::Migration::RedirectsController#create`.
pub async fn create_redirect(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<crate::moves::MoveForm>,
) -> AppResult<Json<ApiAccount>> {
    auth.require_scope("write:accounts")?;
    crate::moves::create_redirect(&state, auth.account_id, &form).await?;
    get_account(state, Path(auth.account_id)).await
}

/// `Settings::Migration::RedirectsController#destroy`.
pub async fn cancel_redirect(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<ApiAccount>> {
    auth.require_scope("write:accounts")?;
    crate::moves::cancel_redirect(&state, auth.account_id).await?;
    get_account(state, Path(auth.account_id)).await
}

// ── GET/POST/DELETE /api/v1/profile/aliases ───────────────────────────────

pub async fn list_aliases(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<crate::moves::Alias>>> {
    auth.require_scope("read:accounts")?;
    crate::moves::list_aliases(&state, auth.account_id)
        .await
        .map(Json)
}

#[derive(Debug, Deserialize)]
pub struct CreateAliasForm {
    #[serde(default)]
    pub acct: String,
}

pub async fn create_alias(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<CreateAliasForm>,
) -> AppResult<Json<crate::moves::Alias>> {
    auth.require_scope("write:accounts")?;
    crate::moves::create_alias(&state, auth.account_id, &form.acct)
        .await
        .map(Json)
}

pub async fn delete_alias(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:accounts")?;
    crate::moves::destroy_alias(&state, auth.account_id, id).await?;
    Ok(Json(serde_json::json!({})))
}
