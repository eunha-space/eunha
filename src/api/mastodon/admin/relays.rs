//! `Admin::RelaysController`: every action is `RelayPolicy#update?`, which is
//! `manage_federation`.

use axum::{
    extract::{Extension, Path},
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::Params;
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::{
        action_log::{self, Target},
        role::flag,
    },
    relays,
    state::AppState,
};

#[derive(Debug, Serialize)]
pub struct AdminRelay {
    pub id: String,
    pub inbox_url: String,
    /// `idle`, `pending`, `accepted` or `rejected`.
    pub state: &'static str,
    /// `enabled?`, which is `accepted?`.
    pub enabled: bool,
    pub follow_activity_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

struct Row {
    id: i64,
    inbox_url: String,
    state: i32,
    follow_activity_id: Option<String>,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

impl From<Row> for AdminRelay {
    fn from(r: Row) -> Self {
        Self {
            id: r.id.to_string(),
            inbox_url: r.inbox_url,
            state: relays::state::to_str(r.state),
            enabled: r.state == relays::state::ACCEPTED,
            follow_activity_id: r.follow_activity_id,
            created_at: super::super::convert::mastodon_date(r.created_at),
            updated_at: super::super::convert::mastodon_date(r.updated_at),
        }
    }
}

async fn authorize(state: &AppState, auth: &AuthenticatedUser, write: bool) -> AppResult<()> {
    auth.require_scope(if write { "admin:write" } else { "admin:read" })?;
    super::require_permission(state, auth.account_id, flag::MANAGE_FEDERATION).await
}

async fn find(state: &AppState, id: i64) -> AppResult<Row> {
    sqlx::query_as!(
        Row,
        r#"SELECT id, inbox_url, state, follow_activity_id, created_at, updated_at
           FROM relays WHERE id = $1"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

/// `GET /api/v1/admin/relays`: `Relay.all`.
pub async fn list_admin_relays(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminRelay>>> {
    authorize(&state, &auth, false).await?;
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT id, inbox_url, state, follow_activity_id, created_at, updated_at
           FROM relays ORDER BY id"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

#[derive(Debug, Deserialize)]
pub struct RelayForm {
    pub inbox_url: Option<String>,
}

/// `URLValidator`: an absolute http or https URL with a host.
fn compliant_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty())
    })
}

/// `POST /api/v1/admin/relays`: `Relays#create`, which saves the relay, logs
/// it, and enables it.
pub async fn create_admin_relay(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<RelayForm>,
) -> AppResult<Json<AdminRelay>> {
    authorize(&state, &auth, true).await?;
    // `normalizes :inbox_url, with: strip`, then `presence`, `uniqueness` and
    // `url`.
    let inbox_url = form.inbox_url.unwrap_or_default().trim().to_owned();
    let mut errors = vec![];
    if inbox_url.is_empty() {
        errors.push("Inbox url can't be blank");
    } else {
        let taken = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM relays WHERE inbox_url = $1) AS "e!""#,
            inbox_url
        )
        .fetch_one(&state.db)
        .await?;
        if taken {
            errors.push("Inbox url has already been taken");
        }
    }
    if !compliant_url(&inbox_url) {
        errors.push("Inbox url is invalid");
    }
    if !errors.is_empty() {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )));
    }
    let id = sqlx::query_scalar!(
        r#"INSERT INTO relays (inbox_url, state, created_at, updated_at)
           VALUES ($1, $2, now(), now()) RETURNING id"#,
        inbox_url,
        relays::state::IDLE,
    )
    .fetch_one(&state.db)
    .await?;
    action_log::log(
        &state.db,
        auth.account_id,
        "create",
        &Target::relay(id, &inbox_url),
    )
    .await?;
    relays::enable(&state, id).await?;
    Ok(Json(find(&state, id).await?.into()))
}

/// `DELETE /api/v1/admin/relays/:id`: `Relays#destroy`; an enabled relay is
/// disabled first (`ensure_disabled`).
pub async fn delete_admin_relay(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let row = find(&state, id).await?;
    authorize(&state, &auth, true).await?;
    if row.state == relays::state::ACCEPTED {
        relays::disable(&state, id).await?;
    }
    sqlx::query!("DELETE FROM relays WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    action_log::log(
        &state.db,
        auth.account_id,
        "destroy",
        &Target::relay(id, &row.inbox_url),
    )
    .await?;
    Ok(Json(serde_json::json!({})))
}

/// `POST /api/v1/admin/relays/:id/enable`.
pub async fn enable_admin_relay(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminRelay>> {
    let row = find(&state, id).await?;
    authorize(&state, &auth, true).await?;
    relays::enable(&state, id).await?;
    action_log::log(
        &state.db,
        auth.account_id,
        "enable",
        &Target::relay(id, &row.inbox_url),
    )
    .await?;
    Ok(Json(find(&state, id).await?.into()))
}

/// `POST /api/v1/admin/relays/:id/disable`.
pub async fn disable_admin_relay(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminRelay>> {
    let row = find(&state, id).await?;
    authorize(&state, &auth, true).await?;
    relays::disable(&state, id).await?;
    action_log::log(
        &state.db,
        auth.account_id,
        "disable",
        &Target::relay(id, &row.inbox_url),
    )
    .await?;
    Ok(Json(find(&state, id).await?.into()))
}
