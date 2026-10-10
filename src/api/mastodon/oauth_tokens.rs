//! The rest of what `use_doorkeeper` mounts: `GET /oauth/token/info`
//! (`Doorkeeper::TokenInfoController`), `POST /oauth/introspect`
//! (`TokensController#introspect`, RFC 7662), and the applications pages,
//! which Mastodon's `admin_authenticator` closes to everyone.

use axum::{
    extract::Query,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use super::extractors::Params;
use crate::state::AppState;

/// A row of `oauth_access_tokens`, with its application's uid.
struct AccessToken {
    application_id: Option<i64>,
    resource_owner_id: Option<i64>,
    scopes: Option<String>,
    created_at: chrono::NaiveDateTime,
    expires_in: Option<i32>,
    revoked_at: Option<chrono::NaiveDateTime>,
    uid: Option<String>,
}

impl AccessToken {
    /// `Doorkeeper::Models::Revocable#revoked?`.
    fn revoked(&self, now: chrono::NaiveDateTime) -> bool {
        self.revoked_at.is_some_and(|at| at <= now)
    }

    fn expires_at(&self) -> Option<chrono::NaiveDateTime> {
        self.expires_in
            .map(|s| self.created_at + chrono::Duration::seconds(s.into()))
    }

    /// `Doorkeeper::Models::Expirable#expired?`.
    fn expired(&self, now: chrono::NaiveDateTime) -> bool {
        self.expires_at().is_some_and(|at| now >= at)
    }

    /// `AccessTokenMixin#accessible?`.
    fn accessible(&self, now: chrono::NaiveDateTime) -> bool {
        !self.expired(now) && !self.revoked(now)
    }

    fn scopes(&self) -> Vec<&str> {
        self.scopes
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .collect()
    }
}

/// `AccessToken.by_token` (or `by_refresh_token`).
async fn find_token(state: &AppState, token: &str) -> Option<AccessToken> {
    if token.is_empty() {
        return None;
    }
    sqlx::query_as!(
        AccessToken,
        r#"SELECT t.application_id, t.resource_owner_id, t.scopes, t.created_at,
                  t.expires_in, t.revoked_at, a.uid AS "uid?"
           FROM oauth_access_tokens t
           LEFT JOIN oauth_applications a ON a.id = t.application_id
           WHERE t.token = $1 OR t.refresh_token = $1
           ORDER BY (t.token = $1) DESC
           LIMIT 1"#,
        token,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
}

fn now() -> chrono::NaiveDateTime {
    chrono::Utc::now().naive_utc()
}

/// The bearer token of the request: Doorkeeper's `access_token_methods`,
/// the `Authorization` header, then the `access_token` and `bearer_token`
/// parameters, and none when more than one of them carries one.
fn bearer(headers: &HeaderMap, params: &TokenParams) -> Option<String> {
    crate::middleware::doorkeeper_token(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
        &crate::middleware::TokenParams {
            access_token: params.access_token.clone(),
            bearer_token: params.bearer_token.clone(),
        },
        &crate::middleware::TokenParams::default(),
    )
}

#[derive(Debug, Default, Deserialize)]
pub struct TokenParams {
    pub access_token: Option<String>,
    pub bearer_token: Option<String>,
}

/// `OAuth::InvalidTokenResponse.from_access_token`: a 401 saying why the
/// token is no good.
fn invalid_token(token: Option<&AccessToken>) -> Response {
    let now = now();
    let description = match token {
        Some(t) if t.revoked(now) => "The access token was revoked",
        Some(t) if t.expired(now) => "The access token expired",
        _ => "The access token is invalid",
    };
    error_response(StatusCode::UNAUTHORIZED, "invalid_token", description)
}

/// `OAuth::ErrorResponse`: its body, status and headers.
fn error_response(status: StatusCode, error: &str, description: &str) -> Response {
    // `sanitize_error_values`: what may stand in a quoted header value.
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| match c {
                '\u{20}'..='\u{21}' | '\u{23}'..='\u{5B}' | '\u{5D}'..='\u{7E}' => c,
                _ => '_',
            })
            .collect()
    };
    let authenticate = format!(
        r#"Bearer realm="Doorkeeper", error="{}", error_description="{}""#,
        sanitize(error),
        sanitize(description)
    );
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store, no-cache".to_owned()),
            (header::WWW_AUTHENTICATE, authenticate),
        ],
        Json(serde_json::json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

/// `GET /oauth/token/info`: the bearer token itself, as
/// `AccessTokenMixin#as_json` renders it, while it is accessible.
pub async fn token_info(
    state: AppState,
    headers: HeaderMap,
    Query(params): Query<TokenParams>,
) -> Response {
    let token = match bearer(&headers, &params) {
        Some(bearer) => find_token(&state, &bearer).await,
        None => None,
    };
    let now = now();
    match token {
        Some(t) if t.accessible(now) => {
            // `expires_in_seconds`: what is left, never below zero.
            let expires_in = t
                .expires_at()
                .map(|at| (at - now).num_milliseconds().max(0) as f64 / 1000.0)
                .map(|s| s.round() as i64);
            Json(serde_json::json!({
                "resource_owner_id": t.resource_owner_id,
                "scope": t.scopes(),
                "expires_in": expires_in,
                "application": { "uid": t.uid },
                "created_at": t.created_at.and_utc().timestamp(),
            }))
            .into_response()
        }
        other => invalid_token(other.as_ref()),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct IntrospectParams {
    pub token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub access_token: Option<String>,
    pub bearer_token: Option<String>,
}

/// `POST /oauth/introspect`: `OAuth::TokenIntrospection`. The caller is a
/// client, by HTTP Basic or its id and secret in the request, or a bearer
/// token other than the one asked about. Either may ask about a token of its
/// own application (`allow_token_introspection`'s default), and is told
/// `active: false` about any other or any token no longer accessible.
pub async fn introspect(
    state: AppState,
    headers: HeaderMap,
    Params(params): Params<IntrospectParams>,
) -> Response {
    let token = find_token(&state, params.token.as_deref().unwrap_or_default()).await;
    let now = now();

    // `server.credentials`, as `/oauth/token` reads them.
    let credentials = match crate::api::mastodon::oauth_client::from_request(
        &headers,
        params.client_id.as_deref(),
        params.client_secret.as_deref(),
    ) {
        Ok(credentials) => credentials,
        Err(_) => return crate::api::mastodon::oauth_client::multiple_methods_response(),
    };

    let active = if let Some(credentials) = credentials {
        // `authorize_using_basic_auth!`: `server.client`, a public client
        // by its id alone.
        let client = crate::api::mastodon::oauth_client::authenticate(&state, &credentials)
            .await
            .ok()
            .flatten()
            .map(|app| app.id);
        let Some(client) = client else {
            return crate::api::mastodon::oauth_client::invalid_client_response();
        };
        token
            .as_ref()
            .is_some_and(|t| t.accessible(now) && t.application_id.is_none_or(|app| app == client))
    } else {
        let token_params = TokenParams {
            access_token: params.access_token.clone(),
            bearer_token: params.bearer_token.clone(),
        };
        let Some(bearer) = bearer(&headers, &token_params) else {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Request need to be authorized. Required parameter for authorizing request is missing or invalid.",
            );
        };
        // `authorize_using_bearer_token!`.
        let authorized = find_token(&state, &bearer).await;
        let valid = authorized.as_ref().is_some_and(|a| {
            Some(bearer.as_str()) != params.token.as_deref()
                && a.accessible(now)
                && a.application_id == token.as_ref().and_then(|t| t.application_id)
        });
        if !valid {
            return invalid_token(authorized.as_ref());
        }
        token.as_ref().is_some_and(|t| t.accessible(now))
    };

    let Some(t) = token.filter(|_| active) else {
        return Json(serde_json::json!({ "active": false })).into_response();
    };
    let mut body = serde_json::json!({
        "active": true,
        "scope": t.scopes().join(" "),
        "client_id": t.uid,
        "token_type": "Bearer",
        "iat": t.created_at.and_utc().timestamp(),
    });
    if let Some(exp) = t.expires_at() {
        body["exp"] = exp.and_utc().timestamp().into();
    }
    Json(body).into_response()
}

/// The applications pages, `Doorkeeper::ApplicationsController`, whose
/// `authenticate_admin!` Mastodon configures as `head 403`.
pub async fn applications_forbidden() -> StatusCode {
    StatusCode::FORBIDDEN
}
