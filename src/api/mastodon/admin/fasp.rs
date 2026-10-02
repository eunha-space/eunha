//! Mastodon's FASP admin pages over REST: `Admin::Fasp::ProvidersController`,
//! `RegistrationsController`, `DebugCallsController` and
//! `Debug::CallbacksController`. Every action is `Admin::Fasp::ProviderPolicy`,
//! which is `manage_federation`, and writes no audit log entry, as upstream's
//! do not. While the `fasp` feature is off, they answer 404.

use axum::{
    extract::{Extension, Path},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    error::{AppError, AppResult},
    fasp::{self, Capability, Provider},
    middleware::AuthenticatedUser,
    moderation::role::flag,
    state::AppState,
};

#[derive(Debug, Serialize)]
pub struct AdminFaspProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub confirmed: bool,
    pub sign_in_url: Option<String>,
    pub remote_identifier: String,
    pub capabilities: Vec<Capability>,
    pub privacy_policy: Option<Value>,
    pub contact_email: Option<String>,
    pub fediverse_account: Option<String>,
    /// `#provider_public_key_fingerprint`, which an administrator compares
    /// with the provider's own before confirming its registration.
    pub provider_public_key_fingerprint: Option<String>,
    pub delivery_last_failed_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl From<&Provider> for AdminFaspProvider {
    fn from(p: &Provider) -> Self {
        use super::super::convert::mastodon_date;
        Self {
            id: p.id.to_string(),
            name: p.name.clone(),
            base_url: p.base_url.clone(),
            confirmed: p.confirmed,
            sign_in_url: p.sign_in_url.clone(),
            remote_identifier: p.remote_identifier.clone(),
            capabilities: p.capabilities(),
            privacy_policy: p.privacy_policy.clone(),
            contact_email: p.contact_email.clone(),
            fediverse_account: p.fediverse_account.clone(),
            provider_public_key_fingerprint: p.provider_public_key_fingerprint(),
            delivery_last_failed_at: p.delivery_last_failed_at.map(mastodon_date),
            created_at: mastodon_date(p.created_at),
            updated_at: mastodon_date(p.updated_at),
        }
    }
}

async fn authorize(state: &AppState, auth: &AuthenticatedUser, write: bool) -> AppResult<()> {
    if !fasp::enabled(state) {
        return Err(AppError::NotFound);
    }
    auth.require_scope(if write { "admin:write" } else { "admin:read" })?;
    super::require_permission(state, auth.account_id, flag::MANAGE_FEDERATION).await
}

async fn find(state: &AppState, id: i64) -> AppResult<Provider> {
    Provider::find(state, id).await?.ok_or(AppError::NotFound)
}

fn provider_error(error: fasp::request::Error) -> AppError {
    AppError::Internal(anyhow::anyhow!("{error}"))
}

/// `GET /api/v1/admin/fasp/providers`: unconfirmed registrations first,
/// newest first.
pub async fn list_admin_fasp_providers(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminFaspProvider>>> {
    authorize(&state, &auth, false).await?;
    let providers = sqlx::query_as!(
        Provider,
        "SELECT * FROM fasp_providers ORDER BY confirmed ASC, created_at DESC"
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(providers.iter().map(Into::into).collect()))
}

/// `GET /api/v1/admin/fasp/providers/:id`.
pub async fn get_admin_fasp_provider(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminFaspProvider>> {
    authorize(&state, &auth, false).await?;
    Ok(Json((&find(&state, id).await?).into()))
}

#[derive(Debug, Deserialize)]
pub struct CapabilitiesForm {
    #[serde(default)]
    pub capabilities: Vec<Capability>,
}

/// `PUT`/`PATCH /api/v1/admin/fasp/providers/:id`: `#update` with
/// `capabilities_attributes`, which replaces the provider's capabilities with
/// those given. Each that changed tells the provider: an enabled one is
/// activated (`POST /capabilities/:id/:major/activation`), a disabled one
/// deactivated (`DELETE`), as `Fasp::Provider#update_remote_capabilities`
/// does after the save.
pub async fn update_admin_fasp_provider(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Json(form): Json<CapabilitiesForm>,
) -> AppResult<Json<AdminFaspProvider>> {
    authorize(&state, &auth, true).await?;
    let provider = find(&state, id).await?;
    let current: Vec<Value> = form
        .capabilities
        .iter()
        .map(|c| json!({ "id": c.id, "version": c.version, "enabled": c.enabled }))
        .collect();
    save_capabilities(&state, &provider, Value::Array(current)).await?;
    Ok(Json((&find(&state, id).await?).into()))
}

/// Save `capabilities`, then `#update_remote_capabilities`: each one with an
/// `enabled` key that was not there before is activated or deactivated.
async fn save_capabilities(
    state: &AppState,
    provider: &Provider,
    capabilities: Value,
) -> AppResult<()> {
    sqlx::query!(
        "UPDATE fasp_providers SET capabilities = $2, updated_at = now() WHERE id = $1",
        provider.id,
        capabilities,
    )
    .execute(&state.db)
    .await?;
    let old = match &provider.capabilities {
        Value::Array(items) => items.clone(),
        _ => vec![],
    };
    let Value::Array(current) = &capabilities else {
        return Ok(());
    };
    if current == &old {
        return Ok(());
    }
    let mut saved = provider.clone();
    saved.capabilities = capabilities.clone();
    for capability in current {
        let Some(enabled) = capability.get("enabled").and_then(Value::as_bool) else {
            continue;
        };
        if old.contains(capability) {
            continue;
        }
        let id = capability
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let version = match capability.get("version") {
            Some(Value::String(v)) => v.clone(),
            Some(other) if !other.is_null() => other.to_string(),
            _ => String::new(),
        };
        let major = version.split('.').next().unwrap_or_default();
        let path = format!("/capabilities/{id}/{major}/activation");
        if enabled {
            fasp::request::post(state, &saved, &path, None).await
        } else {
            fasp::request::delete(state, &saved, &path).await
        }
        .map_err(provider_error)?;
    }
    Ok(())
}

/// `DELETE /api/v1/admin/fasp/providers/:id`: the provider and its
/// subscriptions, backfill requests and debug callbacks.
pub async fn delete_admin_fasp_provider(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    authorize(&state, &auth, true).await?;
    let provider = find(&state, id).await?;
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "DELETE FROM fasp_backfill_requests WHERE fasp_provider_id = $1",
        provider.id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM fasp_debug_callbacks WHERE fasp_provider_id = $1",
        provider.id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM fasp_subscriptions WHERE fasp_provider_id = $1",
        provider.id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM fasp_providers WHERE id = $1", provider.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({})))
}

/// `POST /api/v1/admin/fasp/providers/:id/registration`:
/// `Admin::Fasp::RegistrationsController#create`, `update_info!(confirm:
/// true)` — confirm the provider, and record what its `/provider_info` says
/// about itself. Nothing is saved if the provider cannot be asked.
pub async fn confirm_admin_fasp_registration(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminFaspProvider>> {
    authorize(&state, &auth, true).await?;
    let provider = find(&state, id).await?;
    let mut confirmed = provider.clone();
    confirmed.confirmed = true;
    let info = fasp::request::get(&state, &confirmed, "/provider_info")
        .await
        .map_err(provider_error)?
        .unwrap_or(Value::Null);
    let string = |name: &str| info.get(name).and_then(Value::as_str).map(str::to_owned);
    let privacy_policy = info.get("privacyPolicy").filter(|v| !v.is_null()).cloned();
    let capabilities = match info.get("capabilities") {
        Some(Value::Array(items)) => Value::Array(items.clone()),
        _ => json!([]),
    };
    sqlx::query!(
        r#"UPDATE fasp_providers
              SET confirmed = true, privacy_policy = $2, sign_in_url = $3,
                  contact_email = $4, fediverse_account = $5, updated_at = now()
            WHERE id = $1"#,
        provider.id,
        privacy_policy,
        string("signInUrl"),
        string("contactEmail"),
        string("fediverseAccount"),
    )
    .execute(&state.db)
    .await?;
    save_capabilities(&state, &confirmed, capabilities).await?;
    Ok(Json((&find(&state, id).await?).into()))
}

/// `POST /api/v1/admin/fasp/providers/:id/debug_calls`:
/// `#perform_debug_call`, which asks the provider to call back.
pub async fn create_admin_fasp_debug_call(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    authorize(&state, &auth, true).await?;
    let provider = find(&state, id).await?;
    fasp::workers::perform_debug_call(&state, &provider)
        .await
        .map_err(provider_error)?;
    Ok(Json(json!({})))
}

#[derive(Debug, Serialize)]
pub struct AdminFaspDebugCallback {
    pub id: String,
    pub provider: AdminFaspCallbackProvider,
    pub ip: String,
    pub request_body: String,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct AdminFaspCallbackProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
}

/// `GET /api/v1/admin/fasp/debug/callbacks`, newest first.
pub async fn list_admin_fasp_debug_callbacks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminFaspDebugCallback>>> {
    authorize(&state, &auth, true).await?;
    let rows = sqlx::query!(
        r#"SELECT c.id, c.ip, c.request_body, c.created_at,
                  p.id AS provider_id, p.name, p.base_url
             FROM fasp_debug_callbacks c
             JOIN fasp_providers p ON p.id = c.fasp_provider_id
            ORDER BY c.created_at DESC"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| AdminFaspDebugCallback {
                id: r.id.to_string(),
                provider: AdminFaspCallbackProvider {
                    id: r.provider_id.to_string(),
                    name: r.name,
                    base_url: r.base_url,
                },
                ip: r.ip,
                request_body: r.request_body,
                created_at: super::super::convert::mastodon_date(r.created_at),
            })
            .collect(),
    ))
}

/// `DELETE /api/v1/admin/fasp/debug/callbacks/:id`.
pub async fn delete_admin_fasp_debug_callback(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    authorize(&state, &auth, true).await?;
    let deleted = sqlx::query!("DELETE FROM fasp_debug_callbacks WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(Json(json!({})))
}
