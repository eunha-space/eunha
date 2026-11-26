//! `Admin::RelaysController`: every action is `RelayPolicy#update?`, which is
//! `manage_federation`.
//!
//! Mastodon: app/controllers/admin/relays_controller.rb, app/models/relay.rb
//! Mastodon: app/policies/relay_policy.rb
//! Mastodon: app/services/activitypub/fetch_relay_service.rb

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
    /// The relay actor's id.
    pub uri: String,
    pub inbox_url: String,
    pub account_id: String,
    /// The relay actor's `display_name`, which the admin page heads it with.
    pub display_name: String,
    /// `bidirectional` or `broadcast`.
    pub direction: &'static str,
    /// Whether only members who opt in have their posts sent to it.
    pub requires_opt_in: bool,
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
    uri: String,
    inbox_url: String,
    account_id: i64,
    display_name: String,
    direction: i32,
    requires_opt_in: bool,
    state: i32,
    follow_activity_id: Option<String>,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

impl From<Row> for AdminRelay {
    fn from(r: Row) -> Self {
        Self {
            id: r.id.to_string(),
            uri: r.uri,
            inbox_url: r.inbox_url,
            account_id: r.account_id.to_string(),
            display_name: r.display_name,
            direction: relays::direction::to_str(r.direction),
            requires_opt_in: r.requires_opt_in,
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
        r#"SELECT r.id, r.uri, r.inbox_url, r.account_id, a.display_name, r.direction,
                  r.requires_opt_in, r.state, r.follow_activity_id, r.created_at, r.updated_at
           FROM relays r JOIN accounts a ON a.id = r.account_id WHERE r.id = $1"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

/// `GET /api/v1/admin/relays`: `Relay.includes(:account)`.
pub async fn list_admin_relays(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminRelay>>> {
    authorize(&state, &auth, false).await?;
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT r.id, r.uri, r.inbox_url, r.account_id, a.display_name, r.direction,
                  r.requires_opt_in, r.state, r.follow_activity_id, r.created_at, r.updated_at
           FROM relays r JOIN accounts a ON a.id = r.account_id ORDER BY r.id"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

#[derive(Debug, Deserialize)]
pub struct RelayForm {
    /// The relay actor's handle, `username@domain`.
    pub acct: Option<String>,
    /// `bidirectional`, the default, or `broadcast`.
    pub direction: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    pub requires_opt_in: Option<bool>,
}

/// `ActivityPub::FetchRelayService::SUPPORTED_TYPES`.
const SUPPORTED_TYPES: [&str; 2] = ["Application", "Service"];

fn invalid(errors: &[&str]) -> AppError {
    AppError::Unprocessable(format!("Validation failed: {}", errors.join(", ")))
}

/// `POST /api/v1/admin/relays`: `Relays#create`, which checks the handle
/// (`valid?(:preflight)`), resolves it to the relay's actor
/// (`ActivityPub::FetchRelayService`), saves the relay, logs it, and enables
/// it.
pub async fn create_admin_relay(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<RelayForm>,
) -> AppResult<Json<AdminRelay>> {
    authorize(&state, &auth, true).await?;
    // `normalizes :acct, with: ->(str) { str.strip.gsub(/\A@/, '') }`.
    let acct = form.acct.unwrap_or_default();
    let acct = acct.trim();
    let acct = acct.strip_prefix('@').unwrap_or(acct);
    if acct.is_empty() {
        return Err(invalid(&["Acct can't be blank"]));
    }
    let direction = match form.direction.as_deref() {
        None | Some("") => relays::direction::BIDIRECTIONAL,
        Some(direction) => relays::direction::parse(direction)
            .ok_or_else(|| invalid(&["Direction is not included in the list"]))?,
    };
    let account = crate::moves::resolve_account(&state, acct, false)
        .await
        .ok()
        .flatten()
        .ok_or_else(|| invalid(&["Acct Could not resolve relay actor from handle"]))?;
    if !account
        .actor_type
        .as_deref()
        .is_some_and(|t| SUPPORTED_TYPES.contains(&t))
    {
        return Err(invalid(&[
            "Acct Unsupported actor type, must be one of: Application, Service",
        ]));
    }
    let uri = account.uri.clone().unwrap_or_default();
    // The validations `on: :create`.
    let mut errors = vec![];
    if uri.is_empty() {
        errors.push("Uri can't be blank");
    } else {
        let taken = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM relays WHERE uri = $1) AS "e!""#,
            uri
        )
        .fetch_one(&state.db)
        .await?;
        if taken {
            errors.push("Uri has already been taken");
        }
    }
    if account.inbox_url.is_empty() {
        errors.push("Inbox url can't be blank");
    }
    if !errors.is_empty() {
        return Err(invalid(&errors));
    }
    let id = sqlx::query_scalar!(
        r#"INSERT INTO relays (uri, inbox_url, account_id, direction, requires_opt_in, state,
                               created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, now(), now()) RETURNING id"#,
        uri,
        account.inbox_url,
        account.id,
        direction,
        form.requires_opt_in.unwrap_or(false),
        relays::state::IDLE,
    )
    .fetch_one(&state.db)
    .await?;
    action_log::log(
        &state.db,
        auth.account_id,
        "create",
        &Target::relay(id, &uri),
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
        &Target::relay(id, &row.uri),
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
        &Target::relay(id, &row.uri),
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
        &Target::relay(id, &row.uri),
    )
    .await?;
    Ok(Json(find(&state, id).await?.into()))
}
