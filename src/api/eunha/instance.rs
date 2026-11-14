//! What eunha says about the instance beyond Mastodon's `/api/v2/instance`,
//! for its own web client's About page.
//!
//! Mastodon: none

use axum::Json;
use serde::Serialize;

use crate::{config::HostingProvider, state::AppState};

#[derive(Debug, Serialize)]
pub struct InstanceExtras {
    /// Who hosts the instance, if the configuration credits anyone. A `url`
    /// that is not an `http(s)` address is left out.
    pub hosting_provider: Option<HostingProvider>,
}

/// GET /api/eunha/v1/instance: public, as `/api/v2/instance` is.
pub async fn show(state: AppState) -> Json<InstanceExtras> {
    let hosting_provider = state
        .instance
        .hosting_provider
        .clone()
        .filter(|p| !p.name.trim().is_empty())
        .map(|p| HostingProvider {
            url: p
                .url
                .filter(|u| u.starts_with("https://") || u.starts_with("http://")),
            ..p
        });
    Json(InstanceExtras { hosting_provider })
}
