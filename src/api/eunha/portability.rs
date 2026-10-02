//! A member's data export, imports and archive takeout over REST.
//!
//! Mastodon serves these as web settings pages (`Settings::ExportsController`
//! and `Settings::Exports::*`, `Settings::ImportsController`,
//! `BackupsController`), and has no API for them. Eunha's settings are part
//! of a single-page app, so they are served here (the
//! `data-portability-rest-api` divergence). The work is
//! [`crate::portability`]; these only translate.

use axum::{
    extract::Path,
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
    routing::get,
    Extension, Json, Router,
};
use serde::Serialize;

use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    portability::export,
    state::AppState,
};

fn signed_in(auth: Option<Extension<AuthenticatedUser>>) -> AppResult<AuthenticatedUser> {
    auth.map(|Extension(auth)| auth)
        .ok_or(AppError::Unauthorized)
}

/// A file to save, as `send_data` sends one.
fn attachment(filename: &str, content_type: &str, body: String) -> Response {
    let disposition = format!("attachment; filename=\"{filename}\"; filename*=UTF-8''{filename}");
    let mut response = body.into_response();
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    response
}

#[derive(Serialize)]
struct ExportPage {
    #[serde(flatten)]
    summary: export::Summary,
}

/// GET /api/eunha/v1/exports — `Settings::ExportsController#show`.
async fn show_exports(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<ExportPage>> {
    let auth = signed_in(auth)?;
    auth.require_scope("read")?;
    Ok(Json(ExportPage {
        summary: export::summary(&state, auth.account_id).await?,
    }))
}

/// GET /api/eunha/v1/exports/{file} — `Settings::Exports::*#index`, at
/// upstream's file names (`follows.csv`, `custom_filters.json`, …).
async fn download_export(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Path(file): Path<String>,
) -> AppResult<Response> {
    let auth = signed_in(auth)?;
    let kind = export::ExportKind::from_path(&file).ok_or(AppError::NotFound)?;
    auth.require_scope(kind.scope())?;
    let body = export::generate(&state, auth.account_id, kind).await?;
    Ok(attachment(kind.filename(), kind.content_type(), body))
}

pub fn routes() -> Router {
    Router::new()
        .route("/api/eunha/v1/exports", get(show_exports))
        .route("/api/eunha/v1/exports/{file}", get(download_export))
}
