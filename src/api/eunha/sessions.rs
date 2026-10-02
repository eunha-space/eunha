//! An account's own sessions, authorized applications and sign-in history:
//! Mastodon's `Settings::SessionsController` (the sessions listed on its
//! account settings page), `OAuth::AuthorizedApplicationsController` and
//! `Settings::LoginActivitiesController`, which it has only as web pages.

use axum::{
    extract::{Path, Query},
    routing::{delete, get},
    Extension, Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    api::eunha::two_factor::{signed_in, timestamp},
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

pub fn routes() -> Router {
    Router::new()
        .route("/api/eunha/v1/sessions", get(list_sessions))
        .route("/api/eunha/v1/sessions/{id}", delete(revoke_session))
        .route(
            "/api/eunha/v1/authorized_applications",
            get(list_applications),
        )
        .route(
            "/api/eunha/v1/authorized_applications/{id}",
            delete(revoke_application),
        )
        .route("/api/eunha/v1/login_activities", get(list_login_activities))
}

fn token_id(auth: &Option<Extension<AuthenticatedUser>>) -> Option<i64> {
    auth.as_ref().map(|Extension(a)| a.token_id)
}

#[derive(Debug, Serialize)]
pub struct Session {
    pub id: String,
    pub ip: Option<String>,
    pub user_agent: String,
    /// `sessions.browsers.*`.
    pub browser: &'static str,
    /// `sessions.platforms.*`.
    pub platform: &'static str,
    /// `sessions.description`: "Firefox on macOS".
    pub description: String,
    pub created_at: String,
    /// Mastodon's "last activity".
    pub updated_at: String,
    /// The session behind the token asking.
    pub current: bool,
}

/// GET /api/eunha/v1/sessions
///
/// `Auth::RegistrationsController#set_sessions`, newest activity first.
pub async fn list_sessions(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Vec<Session>>> {
    let current = token_id(&auth);
    let (_, user_id) = signed_in(auth, "read:accounts")?;
    let rows = sqlx::query!(
        r#"SELECT id, host(ip) AS ip, user_agent, access_token_id, created_at, updated_at
           FROM session_activations WHERE user_id = $1
           ORDER BY updated_at DESC, id DESC"#,
        user_id,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let (browser, platform) = crate::browser_detection::detect(&r.user_agent);
                Session {
                    id: r.id.to_string(),
                    ip: r.ip,
                    description: crate::browser_detection::describe(&r.user_agent),
                    user_agent: r.user_agent,
                    browser,
                    platform,
                    created_at: timestamp(r.created_at),
                    updated_at: timestamp(r.updated_at),
                    current: r.access_token_id.is_some() && r.access_token_id == current,
                }
            })
            .collect(),
    ))
}

/// DELETE /api/eunha/v1/sessions/:id
///
/// `Settings::SessionsController#destroy`: the session and its token gone.
pub async fn revoke_session(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let (_, user_id) = signed_in(auth, "write:accounts")?;
    let owned = sqlx::query_scalar!(
        "SELECT id FROM session_activations WHERE id = $1 AND user_id = $2",
        id,
        user_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let tokens = crate::sessions::destroy_where_ids(&state.db, &[owned]).await?;
    crate::sessions::kill_streams(&state, tokens).await;
    Ok(Json(serde_json::json!({})))
}

#[derive(Debug, Serialize)]
pub struct AuthorizedApplication {
    pub id: String,
    pub name: String,
    pub website: Option<String>,
    pub scopes: Vec<String>,
    /// The instance's own web app, which is revoked by ending its sessions.
    pub superapp: bool,
    /// `User#applications_last_used`.
    pub last_used_at: Option<String>,
    /// What Mastodon's page shows as "authorized on": the app's creation.
    pub created_at: String,
}

/// GET /api/eunha/v1/authorized_applications
///
/// `Doorkeeper::Application.authorized_for(user)`: every app holding a token
/// the user has not revoked.
pub async fn list_applications(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Vec<AuthorizedApplication>>> {
    let (_, user_id) = signed_in(auth, "read:accounts")?;
    let rows = sqlx::query!(
        r#"SELECT a.id, a.name, a.website, a.scopes, a.superapp, a.created_at,
                  (SELECT max(t.last_used_at) FROM oauth_access_tokens t
                   WHERE t.application_id = a.id AND t.resource_owner_id = $1) AS last_used_at
           FROM oauth_applications a
           WHERE a.id IN (SELECT application_id FROM oauth_access_tokens
                          WHERE resource_owner_id = $1 AND revoked_at IS NULL
                            AND application_id IS NOT NULL)
           ORDER BY a.id"#,
        user_id,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| AuthorizedApplication {
                id: r.id.to_string(),
                name: r.name,
                website: r.website.filter(|w| !w.is_empty()),
                scopes: r.scopes.split_whitespace().map(str::to_owned).collect(),
                superapp: r.superapp,
                last_used_at: r.last_used_at.map(timestamp),
                created_at: r.created_at.map(timestamp).unwrap_or_default(),
            })
            .collect(),
    ))
}

/// DELETE /api/eunha/v1/authorized_applications/:id
///
/// `OAuth::AuthorizedApplicationsController#destroy`: the app's tokens and
/// grants for this user revoked, their push subscriptions removed and their
/// streams closed. The web app is not offered for revoking, as Mastodon's
/// page does not offer it.
pub async fn revoke_application(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let (_, user_id) = signed_in(auth, "write:accounts")?;
    let superapp = sqlx::query_scalar!("SELECT superapp FROM oauth_applications WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    if superapp {
        return Err(AppError::Forbidden);
    }
    crate::sessions::revoke_application(&state, id, user_id).await?;
    Ok(Json(serde_json::json!({})))
}

#[derive(Debug, Deserialize)]
pub struct Page {
    pub max_id: Option<i64>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct LoginActivity {
    pub id: String,
    /// `password`, `otp`, `webauthn`, `sign_in_token` or an omniauth one.
    pub authentication_method: Option<String>,
    pub provider: Option<String>,
    pub success: bool,
    pub failure_reason: Option<String>,
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    pub browser: &'static str,
    pub platform: &'static str,
    pub created_at: Option<String>,
}

/// GET /api/eunha/v1/login_activities
///
/// `Settings::LoginActivitiesController#index`, newest first. Mastodon pages
/// it 25 at a time; this takes `max_id` and `limit` (at most 80).
pub async fn list_login_activities(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Query(page): Query<Page>,
) -> AppResult<Json<Vec<LoginActivity>>> {
    let (_, user_id) = signed_in(auth, "read:accounts")?;
    let limit = page.limit.unwrap_or(25).clamp(1, 80);
    let rows = sqlx::query!(
        r#"SELECT id, authentication_method, provider, success, failure_reason,
                  host(ip) AS ip, user_agent, created_at
           FROM login_activities
           WHERE user_id = $1 AND ($2::bigint IS NULL OR id < $2)
           ORDER BY id DESC LIMIT $3"#,
        user_id,
        page.max_id,
        limit,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let (browser, platform) =
                    crate::browser_detection::detect(r.user_agent.as_deref().unwrap_or(""));
                LoginActivity {
                    id: r.id.to_string(),
                    authentication_method: r.authentication_method,
                    provider: r.provider,
                    success: r.success.unwrap_or(false),
                    failure_reason: r.failure_reason,
                    ip: r.ip,
                    user_agent: r.user_agent,
                    browser,
                    platform,
                    created_at: r.created_at.map(timestamp),
                }
            })
            .collect(),
    ))
}
