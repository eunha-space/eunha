//! eunha-specific HTTP APIs that have no Mastodon C2S equivalent. Kept separate
//! from `api::mastodon` so the Mastodon-compatible surface stays clean.
use axum::{routing::get, Router};

pub mod email_subscriptions;
pub mod health;
pub mod invite_grants;
pub mod invite_tree;

pub fn router() -> Router {
    Router::new()
        .route("/api/eunha/v1/health", get(health::health))
        .route("/api/eunha/v1/invite_tree", get(invite_tree::invite_tree))
        .merge(invite_grants::routes())
        .merge(email_subscriptions::routes())
}
