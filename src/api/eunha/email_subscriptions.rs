//! An account's own switch for email subscriptions.
//!
//! Mastodon offers it on the web privacy settings page
//! (`Settings::PrivacyController`), to a user whose role carries
//! `manage_email_subscriptions` while the feature is enabled, beside the
//! count of confirmed subscribers. Mastodon has no API for it, and eunha's
//! settings page is part of a single-page app, so it is served here.

use axum::{routing::get, Extension, Json, Router};
use serde::{Deserialize, Serialize};

use crate::{
    api::mastodon::extractors::{FlexBool, Params},
    email_subscriptions as subs,
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

#[derive(Debug, Serialize)]
pub struct OwnSubscriptions {
    /// Whether the section is shown at all: the feature is enabled and the
    /// account's role may use it.
    pub available: bool,
    /// The user setting.
    pub enabled: bool,
    /// Confirmed subscribers.
    pub subscribers: i64,
}

async fn own(state: &AppState, account_id: i64) -> AppResult<OwnSubscriptions> {
    Ok(OwnSubscriptions {
        available: subs::enabled(state).await && subs::user_can(state, account_id).await,
        enabled: subs::user_enabled(state, account_id).await,
        subscribers: subs::confirmed_count(state, account_id).await?,
    })
}

/// GET /api/eunha/v1/email_subscriptions
pub async fn show(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<OwnSubscriptions>> {
    let Some(Extension(auth)) = auth else {
        return Err(AppError::Unauthorized);
    };
    auth.require_scope("read:accounts")?;
    Ok(Json(own(&state, auth.account_id).await?))
}

#[derive(Debug, Deserialize)]
pub struct UpdateForm {
    pub enabled: FlexBool,
}

/// PUT /api/eunha/v1/email_subscriptions
pub async fn update(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Params(form): Params<UpdateForm>,
) -> AppResult<Json<OwnSubscriptions>> {
    let Some(Extension(auth)) = auth else {
        return Err(AppError::Unauthorized);
    };
    auth.require_scope("write:accounts")?;
    crate::middleware::require_user(Some(&auth))?;
    let current = own(&state, auth.account_id).await?;
    if !current.available {
        return Err(AppError::NotFound);
    }
    subs::set_user_enabled(&state, auth.account_id, form.enabled.0).await?;
    Ok(Json(own(&state, auth.account_id).await?))
}

pub fn routes() -> Router {
    Router::new().route("/api/eunha/v1/email_subscriptions", get(show).put(update))
}
