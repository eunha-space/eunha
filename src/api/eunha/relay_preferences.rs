//! A member's relay preferences, for the privacy settings: the relays that take
//! only the posts of members who opt in, and which of them this member has.
//! `Settings::RelayPreferencesController` is a page Mastodon has no API for;
//! [`crate::relays`] keeps the preferences.
//!
//! Mastodon: app/controllers/settings/relay_preferences_controller.rb

use axum::{routing::get, Extension, Json, Router};
use serde::Deserialize;

use crate::{
    api::{eunha::two_factor::signed_in, mastodon::extractors::Params},
    error::AppResult,
    middleware::AuthenticatedUser,
    relays::{self, OptInRelay},
    state::AppState,
};

pub fn routes() -> Router {
    Router::new().route(
        "/api/eunha/v1/relay_preferences",
        get(show).put(update).patch(update),
    )
}

/// `GET /api/eunha/v1/relay_preferences`: `#show`.
pub async fn show(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Vec<OptInRelay>>> {
    let (account_id, _) = signed_in(auth, "read:accounts")?;
    Ok(Json(relays::opt_in_relays(&state, account_id).await?))
}

#[derive(Debug, Deserialize)]
pub struct Update {
    /// The relays to be opted in to; every other is opted out of.
    #[serde(default)]
    pub relay_ids: Vec<serde_json::Value>,
}

/// `PUT /api/eunha/v1/relay_preferences`: `#update`, with
/// `Form::RelayPreferencesBatch`, which drops blank ids and `0`, the value
/// an unticked box sends.
pub async fn update(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Params(form): Params<Update>,
) -> AppResult<Json<Vec<OptInRelay>>> {
    let (account_id, _) = signed_in(auth, "write:accounts")?;
    let relay_ids: Vec<i64> = form
        .relay_ids
        .iter()
        .map(|id| match id {
            serde_json::Value::String(id) => id.trim().to_owned(),
            id => id.to_string(),
        })
        .filter(|id| !id.is_empty() && id != "0")
        .filter_map(|id| id.parse().ok())
        .collect();
    relays::save_preferences(&state, account_id, &relay_ids).await?;
    Ok(Json(relays::opt_in_relays(&state, account_id).await?))
}
