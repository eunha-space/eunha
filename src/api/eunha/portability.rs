//! A member's data export, imports and archive takeout over REST.
//!
//! Mastodon serves these as web settings pages (`Settings::ExportsController`
//! and `Settings::Exports::*`, `Settings::ImportsController`,
//! `BackupsController`), and has no API for them. Eunha's settings are part
//! of a single-page app, so they are served here (the
//! `data-portability-rest-api` divergence). The work is
//! [`crate::portability`]; these only translate.

use axum::{
    extract::{DefaultBodyLimit, Multipart, Path},
    http::{header, HeaderValue},
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use serde::Serialize;

use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    portability::{export, import},
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

/// GET /api/eunha/v1/imports — `Settings::ImportsController#index`'s recent
/// imports.
async fn list_imports(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Vec<import::BulkImport>>> {
    let auth = signed_in(auth)?;
    auth.require_scope("read")?;
    Ok(Json(import::recent(&state, auth.account_id).await?))
}

/// The multipart form `Settings::ImportsController#create` reads: `type`,
/// `mode` and the file as `data`, bare or as Rails names them
/// (`form_import[data]`).
async fn read_form(mut multipart: Multipart) -> AppResult<import::ImportForm> {
    let mut form = import::ImportForm::default();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(e.to_string()))?
    {
        let name = field.name().unwrap_or_default().to_owned();
        let name = name
            .strip_prefix("form_import[")
            .and_then(|n| n.strip_suffix(']'))
            .unwrap_or(&name)
            .to_owned();
        match name.as_str() {
            "data" => {
                let filename = field.file_name().map(str::to_owned);
                let content_type = field.content_type().map(str::to_owned);
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::BadRequest(e.to_string()))?;
                form.data = Some(import::Upload {
                    filename,
                    content_type,
                    bytes: bytes.to_vec(),
                });
            }
            "type" | "mode" => {
                let value = field
                    .text()
                    .await
                    .map_err(|e| AppError::BadRequest(e.to_string()))?;
                if name == "type" {
                    form.r#type = Some(value);
                } else {
                    form.mode = Some(value);
                }
            }
            _ => {}
        }
    }
    Ok(form)
}

/// POST /api/eunha/v1/imports — `Settings::ImportsController#create`: the
/// upload is checked and stored, unconfirmed.
async fn create_import(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    multipart: Multipart,
) -> AppResult<Json<import::BulkImport>> {
    let auth = signed_in(auth)?;
    crate::middleware::require_user(Some(&auth))?;
    let form = read_form(multipart).await?;
    match form.r#type.as_deref().and_then(import::ImportType::parse) {
        Some(import_type) => auth.require_scope(import_type.scope())?,
        None => auth.require_scope("write")?,
    }
    Ok(Json(import::create(&state, auth.account_id, form).await?))
}

/// GET /api/eunha/v1/imports/{id}
async fn show_import(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Path(id): Path<i64>,
) -> AppResult<Json<import::BulkImport>> {
    let auth = signed_in(auth)?;
    auth.require_scope("read")?;
    Ok(Json(import::show(&state, auth.account_id, id).await?))
}

/// DELETE /api/eunha/v1/imports/{id} — `Settings::ImportsController#destroy`.
async fn destroy_import(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let auth = signed_in(auth)?;
    auth.require_scope("write")?;
    crate::middleware::require_user(Some(&auth))?;
    import::destroy(&state, auth.account_id, id).await?;
    Ok(Json(serde_json::json!({})))
}

/// POST /api/eunha/v1/imports/{id}/confirm — `Settings::ImportsController#confirm`.
async fn confirm_import(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Path(id): Path<i64>,
) -> AppResult<Json<import::BulkImport>> {
    let auth = signed_in(auth)?;
    crate::middleware::require_user(Some(&auth))?;
    let pending = import::show(&state, auth.account_id, id).await?;
    if let Some(import_type) = import::ImportType::parse(pending.r#type) {
        auth.require_scope(import_type.scope())?;
    }
    Ok(Json(import::confirm(&state, auth.account_id, id).await?))
}

/// GET /api/eunha/v1/imports/{id}/failures — `Settings::ImportsController#failures`.
async fn import_failures(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let auth = signed_in(auth)?;
    auth.require_scope("read")?;
    let (filename, content_type, body) = import::failures(&state, auth.account_id, id).await?;
    Ok(attachment(filename, content_type, body))
}

pub fn routes() -> Router {
    Router::new()
        .route("/api/eunha/v1/exports", get(show_exports))
        .route("/api/eunha/v1/exports/{file}", get(download_export))
        .route(
            "/api/eunha/v1/imports",
            get(list_imports)
                .post(create_import)
                // `Form::Import::FILE_SIZE_LIMIT` is checked on the upload, so
                // the body may be a little larger than it.
                .layer(DefaultBodyLimit::max(import::FILE_SIZE_LIMIT + 1024 * 1024)),
        )
        .route(
            "/api/eunha/v1/imports/{id}",
            get(show_import).delete(destroy_import),
        )
        .route("/api/eunha/v1/imports/{id}/confirm", post(confirm_import))
        .route("/api/eunha/v1/imports/{id}/failures", get(import_failures))
}
