//! Mastodon's FASP admin pages over REST: `Admin::Fasp::ProvidersController`,
//! `CapabilityActivationsController`, `RegistrationsController`,
//! `DebugCallsController` and `Debug::CallbacksController`. Every action is `Admin::Fasp::ProviderPolicy`,
//! which is `manage_federation`, and writes no audit log entry, as upstream's
//! do not. While the `fasp` feature is off, they answer 404.
//!
//! Mastodon: app/controllers/admin/fasp/
//! Mastodon: app/policies/admin/fasp/provider_policy.rb
//! Mastodon: app/models/fasp/provider.rb

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

impl AdminFaspProvider {
    async fn load(state: &AppState, p: &Provider) -> AppResult<Self> {
        use super::super::convert::mastodon_date;
        Ok(Self {
            id: p.id.to_string(),
            name: p.name.clone(),
            base_url: p.base_url.clone(),
            confirmed: p.confirmed,
            sign_in_url: p.sign_in_url.clone(),
            remote_identifier: p.remote_identifier.clone(),
            capabilities: p.capabilities(state).await?,
            privacy_policy: p.privacy_policy.clone(),
            contact_email: p.contact_email.clone(),
            fediverse_account: p.fediverse_account.clone(),
            provider_public_key_fingerprint: p.provider_public_key_fingerprint(),
            delivery_last_failed_at: p.delivery_last_failed_at.map(mastodon_date),
            created_at: mastodon_date(p.created_at),
            updated_at: mastodon_date(p.updated_at),
        })
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
    let mut list = Vec::with_capacity(providers.len());
    for provider in &providers {
        list.push(AdminFaspProvider::load(&state, provider).await?);
    }
    Ok(Json(list))
}

/// `GET /api/v1/admin/fasp/providers/:id`.
pub async fn get_admin_fasp_provider(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminFaspProvider>> {
    authorize(&state, &auth, false).await?;
    Ok(Json(
        AdminFaspProvider::load(&state, &find(&state, id).await?).await?,
    ))
}

/// One `fasp_capabilities_attributes` entry, naming a capability the provider
/// has.
#[derive(Debug, Deserialize)]
pub struct CapabilityAttributes {
    pub name: String,
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct CapabilitiesForm {
    #[serde(default)]
    pub capabilities: Vec<CapabilityAttributes>,
}

/// `PUT`/`PATCH /api/v1/admin/fasp/providers/:id`: `#update` with
/// `fasp_capabilities_attributes`. Each that changes tells the provider first.
pub async fn update_admin_fasp_provider(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Json(form): Json<CapabilitiesForm>,
) -> AppResult<Json<AdminFaspProvider>> {
    authorize(&state, &auth, true).await?;
    let provider = find(&state, id).await?;
    let capabilities = provider.capabilities(&state).await?;
    for attributes in &form.capabilities {
        let capability = capabilities
            .iter()
            .find(|c| c.name == attributes.name)
            .ok_or(AppError::NotFound)?;
        set_capability_enabled(&state, &provider, capability, attributes.enabled).await?;
    }
    Ok(Json(AdminFaspProvider::load(&state, &provider).await?))
}

/// `POST /api/v1/admin/fasp/capabilities/:id/activation`.
pub async fn activate_admin_fasp_capability(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminFaspProvider>> {
    change_activation(state, auth, id, true).await
}

/// `DELETE /api/v1/admin/fasp/capabilities/:id/activation`.
pub async fn deactivate_admin_fasp_capability(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminFaspProvider>> {
    change_activation(state, auth, id, false).await
}

async fn change_activation(
    state: AppState,
    auth: AuthenticatedUser,
    id: i64,
    enabled: bool,
) -> AppResult<Json<AdminFaspProvider>> {
    authorize(&state, &auth, true).await?;
    let capability = Capability::find(&state, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let provider = find(&state, capability.fasp_provider_id).await?;
    set_capability_enabled(&state, &provider, &capability, enabled).await?;
    Ok(Json(AdminFaspProvider::load(&state, &provider).await?))
}

/// `Fasp::Capability#update_remote_capability`, a `before_update`: the provider
/// hears of the change first, and a provider that cannot be told leaves it
/// unsaved. One that did not change tells nobody.
async fn set_capability_enabled(
    state: &AppState,
    provider: &Provider,
    capability: &Capability,
    enabled: bool,
) -> AppResult<()> {
    if capability.enabled == enabled {
        return Ok(());
    }
    let path = format!(
        "/capabilities/{}/{}/activation",
        capability.name,
        capability.major_version()
    );
    if enabled {
        fasp::request::post(state, provider, &path, None).await
    } else {
        fasp::request::delete(state, provider, &path).await
    }
    .map_err(provider_error)?;
    sqlx::query!(
        "UPDATE fasp_capabilities SET enabled = $2, updated_at = now() WHERE id = $1",
        capability.id,
        enabled,
    )
    .execute(&state.db)
    .await?;
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
    sqlx::query!(
        "DELETE FROM fasp_capabilities WHERE fasp_provider_id = $1",
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
/// about itself. Nothing is saved if the provider cannot be asked. Capabilities
/// it no longer lists go; those it still does keep whether they are enabled.
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
    let capabilities: Vec<(String, String)> = info
        .get("capabilities")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|c| {
            let name = c.get("id")?.as_str()?.to_owned();
            // `find_or_create_by!` on a string column casts a number to one.
            let version = match c.get("version") {
                Some(Value::String(v)) => v.clone(),
                Some(Value::Null) | None => String::new(),
                Some(other) => other.to_string(),
            };
            Some((name, version))
        })
        .collect();
    let names: Vec<String> = capabilities.iter().map(|(n, _)| n.clone()).collect();
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "DELETE FROM fasp_capabilities WHERE fasp_provider_id = $1 AND NOT (name = ANY($2))",
        provider.id,
        &names,
    )
    .execute(&mut *tx)
    .await?;
    for (name, version) in &capabilities {
        sqlx::query!(
            r#"INSERT INTO fasp_capabilities
                 (fasp_provider_id, name, version, created_at, updated_at)
               VALUES ($1, $2, $3, now(), now())
               ON CONFLICT (fasp_provider_id, name, version) DO NOTHING"#,
            provider.id,
            name,
            version,
        )
        .execute(&mut *tx)
        .await?;
    }
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
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(
        AdminFaspProvider::load(&state, &find(&state, id).await?).await?,
    ))
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
