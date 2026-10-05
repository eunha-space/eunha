//! eunha-specific HTTP APIs that have no Mastodon C2S equivalent. Kept separate
//! from `api::mastodon` so the Mastodon-compatible surface stays clean.
use axum::{routing::get, Router};

pub mod account_email;
pub mod email_subscriptions;
pub mod health;
pub mod invite_grants;
pub mod invite_tree;
pub mod portability;
pub mod preferences;
pub mod sessions;
pub mod terms_of_service;
pub mod two_factor;

pub fn router() -> Router {
    Router::new()
        .route("/api/eunha/v1/health", get(health::health))
        .route(
            "/api/eunha/v1/invite",
            get(crate::api::mastodon::signup::invite_lookup),
        )
        .route("/api/eunha/v1/invite_tree", get(invite_tree::invite_tree))
        .route(
            "/api/eunha/v1/terms_of_service/interstitial",
            get(terms_of_service::show).delete(terms_of_service::dismiss),
        )
        .merge(invite_grants::routes())
        .merge(email_subscriptions::routes())
        .merge(portability::routes())
        .merge(two_factor::routes())
        .merge(sessions::routes())
        .merge(preferences::routes())
        .merge(account_email::routes())
}
