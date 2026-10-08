pub mod accounts;
pub mod activity_tracker;
pub mod announcements;
pub mod api;
pub mod async_refresh;
pub mod auto_close_registrations;
pub mod background;
pub mod browser_detection;
pub mod collection_item_cleanup;
pub mod config;
pub mod conversation;
pub mod counters;
pub mod crypto;
pub mod db;
pub mod delete_account;
pub mod divergence;
pub mod email;
pub mod email_subscriptions;
pub mod error;
pub mod fasp;
pub mod featured_tags;
pub mod federation;
pub mod feed;
pub mod formatter;
pub mod home_feed;
pub mod import;
pub mod ip_cleanup;
pub mod jobs;
pub mod languages;
pub mod link_verification;
pub mod locale;
pub mod markdown;
pub mod media;
pub mod middleware;
pub mod migrate;
pub mod moderation;
pub mod moves;
pub mod notification_mail;
pub mod open_files;
pub mod portability;
pub mod preview_card;
pub mod privacy_policy;
pub mod push;
pub mod quotes;
pub mod rails_encryption;
pub mod redis_keys;
pub mod redis_lock;
pub mod relays;
pub mod remote_ip;
pub mod remove_status;
pub mod schema_check;
pub mod search;
pub mod secret_key_base;
pub mod self_destruct;
pub mod sessions;
pub mod settings;
pub mod settings_import;
pub mod site_uploads;
pub mod snowflake;
pub mod software_updates;
pub mod state;
pub mod statuses_cleanup;
pub mod streaming;
pub mod suggestions;
pub mod tags;
pub mod telemetry;
pub mod templates;
pub mod tenants;
pub mod terms_of_service;
pub mod time_zones;
pub mod translation;
pub mod trends;
pub mod two_factor;
pub mod upstream;
pub mod user_standing;
pub mod vacuum;
pub mod version;
pub mod web;
pub mod webauthn;
pub mod worker_batch;

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
        // The FASP API signs each answer over its body's digest, which a
        // compressed body would no longer match.
        .merge(fasp::api::router())
        .layer(axum_middleware::from_fn(middleware::log_failures))
        .layer(axum_middleware::from_fn(middleware::authenticate))
        .layer(axum_middleware::from_fn(telemetry::observe))
        // `check_self_destruct!`, once the request's instance is known.
        .layer(axum_middleware::from_fn(self_destruct::gate))
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
        let viewer = req
            .extensions()
            .get::<middleware::AuthenticatedUser>()
            .cloned();
        web::serve(state, uri, viewer).await
    }
}
