pub mod accounts;
pub mod announcements;
pub mod api;
pub mod async_refresh;
pub mod background;
pub mod browser_detection;
pub mod config;
pub mod counters;
pub mod crypto;
pub mod db;
pub mod delete_account;
pub mod divergence;
pub mod email;
pub mod email_subscriptions;
pub mod error;
pub mod federation;
pub mod feed;
pub mod import;
pub mod languages;
pub mod link_verification;
pub mod locale;
pub mod markdown;
pub mod media;
pub mod middleware;
pub mod migrate;
pub mod moderation;
pub mod moves;
pub mod open_files;
pub mod portability;
pub mod preview_card;
pub mod privacy_policy;
pub mod push;
pub mod rails_encryption;
pub mod redis_keys;
pub mod redis_lock;
pub mod remote_ip;
pub mod schema_check;
pub mod sessions;
pub mod settings;
pub mod site_uploads;
pub mod snowflake;
pub mod software_updates;
pub mod state;
pub mod streaming;
pub mod telemetry;
pub mod templates;
pub mod tenants;
pub mod terms_of_service;
pub mod trends;
pub mod two_factor;
pub mod upstream;
pub mod vacuum;
pub mod version;
pub mod web;
pub mod webauthn;

use axum::{extract::Request, middleware as axum_middleware, response::IntoResponse, Router};
use tower_http::{compression::CompressionLayer, cors::CorsLayer, trace::TraceLayer};

/// Every route eunha serves, built once for the whole process. The instance a
/// request belongs to rides on the request itself, put there by the tenant
/// dispatcher, so these routes are shared by every instance the process serves.
pub fn build_app() -> Router {
    let routes = Router::new()
        .merge(api::mastodon::router())
        .merge(api::account::router())
        .merge(api::eunha::router())
        .merge(api::ap::router())
        .fallback(axum::routing::any(fallback));
    // What other servers fetch, WebFinger and NodeInfo included, is ojak's,
    // for the instance the tenant dispatcher put on the request.
    let compressed = ojak_axum::wrap(routes, api::ap::serving::federation(), |parts| {
        parts.extensions.get::<state::AppState>().cloned()
    })
    .layer(CompressionLayer::new());

    Router::new()
        .merge(compressed)
        // Streaming WebSocket must be outside CompressionLayer to avoid body wrapping.
        .merge(api::mastodon::streaming_router())
        .layer(axum_middleware::from_fn(middleware::log_failures))
        .layer(axum_middleware::from_fn(middleware::authenticate))
        .layer(axum_middleware::from_fn(telemetry::observe))
        .layer(axum_middleware::from_fn(middleware::resolve_instance))
        .layer(axum_middleware::from_fn(remote_ip::deny_blocked))
        .layer(axum_middleware::from_fn(remote_ip::layer))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
}

/// What no route claimed: JSON 404 under `/api/`, and the web app elsewhere.
async fn fallback(state: state::AppState, req: Request) -> axum::response::Response {
    let uri = req.uri().clone();
    if uri.path().starts_with("/api/") {
        (
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "not found"})),
        )
            .into_response()
    } else {
        web::serve(state, uri).await
    }
}
