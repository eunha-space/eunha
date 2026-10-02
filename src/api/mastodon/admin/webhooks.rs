//! `Admin::WebhooksController` and `Admin::Webhooks::SecretsController`, under
//! `WebhookPolicy`. Like upstream's controllers, none of it is logged.

use axum::{
    extract::{Extension, Path},
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::Params;
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::role::{self, flag, Role},
    state::AppState,
};

/// `Webhook::EVENTS`.
pub const EVENTS: &[&str] = &[
    "account.approved",
    "account.created",
    "account.updated",
    "report.created",
    "report.updated",
    "status.created",
    "status.updated",
];

/// `Webhook::SECRET_SIZE`, in bytes before hex.
const SECRET_SIZE: usize = 20;

/// `Webhook.permission_for_event`.
fn permission_for_event(event: &str) -> Option<i64> {
    match event {
        "account.approved" | "account.created" | "account.updated" => Some(flag::MANAGE_USERS),
        "report.created" | "report.updated" => Some(flag::MANAGE_REPORTS),
        "status.created" | "status.updated" => Some(flag::VIEW_DEVOPS),
        _ => None,
    }
}

/// `Webhook#required_permissions`, all held by `role`.
fn holds_required_permissions(role: &Role, events: &[String]) -> bool {
    events
        .iter()
        .filter_map(|e| permission_for_event(e))
        .all(|permission| role.can(&[permission]))
}

#[derive(Debug, Serialize)]
pub struct AdminWebhook {
    pub id: String,
    pub url: String,
    pub events: Vec<String>,
    pub template: Option<String>,
    pub enabled: bool,
    pub secret: String,
    /// `WebhookPolicy#update?` and `destroy?` for the acting role: it holds
    /// what every event needs.
    pub can_update: bool,
    pub created_at: String,
    pub updated_at: String,
}

struct Row {
    id: i64,
    url: String,
    events: Vec<String>,
    template: Option<String>,
    enabled: bool,
    secret: String,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

impl Row {
    fn entity(self, acting: &Role) -> AdminWebhook {
        AdminWebhook {
            can_update: acting.can(&[flag::MANAGE_WEBHOOKS])
                && holds_required_permissions(acting, &self.events),
            id: self.id.to_string(),
            url: self.url,
            events: self.events,
            template: self.template,
            enabled: self.enabled,
            secret: self.secret,
            created_at: super::super::convert::mastodon_date(self.created_at),
            updated_at: super::super::convert::mastodon_date(self.updated_at),
        }
    }
}

async fn find(state: &AppState, id: i64) -> AppResult<Row> {
    sqlx::query_as!(
        Row,
        r#"SELECT id, url, events, template, enabled, secret, created_at, updated_at
           FROM webhooks WHERE id = $1"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

/// The acting role, having checked `manage_webhooks` and the scope.
async fn acting(state: &AppState, auth: &AuthenticatedUser, write: bool) -> AppResult<Role> {
    auth.require_scope(if write { "admin:write" } else { "admin:read" })?;
    let acting = role::acting(&state.db, auth.account_id).await?;
    role::authorize(acting.can(&[flag::MANAGE_WEBHOOKS]))?;
    Ok(acting)
}

/// `GET /api/v1/admin/webhooks`.
pub async fn list_admin_webhooks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminWebhook>>> {
    let acting = acting(&state, &auth, false).await?;
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT id, url, events, template, enabled, secret, created_at, updated_at
           FROM webhooks ORDER BY id"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows.into_iter().map(|r| r.entity(&acting)).collect()))
}

/// `GET /api/v1/admin/webhooks/:id`: `Webhooks#show`.
pub async fn get_admin_webhook(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminWebhook>> {
    let row = find(&state, id).await?;
    let acting = acting(&state, &auth, false).await?;
    Ok(Json(row.entity(&acting)))
}

#[derive(Debug, Deserialize)]
pub struct WebhookForm {
    pub url: Option<String>,
    pub events: Option<Vec<String>>,
    pub template: Option<String>,
}

/// `Webhooks::PayloadRenderer::TemplateParser`: text, and `{{…}}`
/// expressions of paths such as `object.account.username` (lower-case
/// property names, array indices after the first) between them.
pub fn template_parses(template: &str) -> bool {
    static VARIABLE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"\A(?:[a-z_]+(?:\.(?:[a-z_]+|[0-9]+))*)*\z").expect("valid regex")
    });
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return false;
        };
        if !VARIABLE.is_match(&after[..end]) {
            return false;
        }
        rest = &after[end + 2..];
    }
    true
}

/// `URLValidator`.
fn compliant_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty())
    })
}

/// `Webhook`'s validations with `current_account` set, and the values a save
/// writes. `events` are normalized, stripped and without blanks.
fn validate(
    url: &str,
    events: &[String],
    template: Option<&str>,
    actor: &Role,
    taken: bool,
) -> AppResult<()> {
    let mut errors = vec![];
    if url.trim().is_empty() {
        errors.push("Url can't be blank");
    }
    if !compliant_url(url) {
        errors.push("Url is invalid");
    }
    if events.is_empty() {
        errors.push("Events can't be blank");
    }
    if events.is_empty() || events.iter().any(|e| !EVENTS.contains(&e.as_str())) {
        errors.push("Events is invalid");
    }
    if !holds_required_permissions(actor, events) {
        errors.push("Events cannot include events you don't have the rights to");
    }
    if template.is_some_and(|t| !t.trim().is_empty() && !template_parses(t)) {
        errors.push("Template is invalid");
    }
    if taken {
        // The table's unique index on `url`, which upstream meets as a
        // database error rather than a validation.
        errors.push("Url has already been taken");
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(AppError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )))
    }
}

async fn url_taken(state: &AppState, url: &str, except: Option<i64>) -> AppResult<bool> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM webhooks WHERE url = $1 AND ($2::bigint IS NULL OR id <> $2)
           ) AS "e!""#,
        url,
        except,
    )
    .fetch_one(&state.db)
    .await?)
}

fn normalize_events(events: Vec<String>) -> Vec<String> {
    events
        .into_iter()
        .map(|e| e.trim().to_owned())
        .filter(|e| !e.is_empty())
        .collect()
}

/// `SecureRandom.hex(SECRET_SIZE)`.
fn random_secret() -> String {
    let bytes: [u8; SECRET_SIZE] = rand::random();
    hex::encode(bytes)
}

/// `POST /api/v1/admin/webhooks`: `Webhooks#create`, with a secret made for it.
pub async fn create_admin_webhook(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<WebhookForm>,
) -> AppResult<Json<AdminWebhook>> {
    let acting = acting(&state, &auth, true).await?;
    // The actor's own role, `current_account.user_role`, which the
    // permission validation reads.
    let actor = role::of_account(&state.db, auth.account_id)
        .await?
        .unwrap_or_else(Role::nobody);
    let url = form.url.unwrap_or_default();
    let events = normalize_events(form.events.unwrap_or_default());
    let template = form.template.filter(|t| !t.is_empty());
    let taken = url_taken(&state, &url, None).await?;
    validate(&url, &events, template.as_deref(), &actor, taken)?;
    let row = sqlx::query_as!(
        Row,
        r#"INSERT INTO webhooks (url, events, template, secret, enabled, created_at, updated_at)
           VALUES ($1, $2, $3, $4, true, now(), now())
           RETURNING id, url, events, template, enabled, secret, created_at, updated_at"#,
        url,
        &events,
        template,
        random_secret(),
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row.entity(&acting)))
}

/// `PATCH /api/v1/admin/webhooks/:id`: `Webhooks#update`, for a role holding
/// what the webhook's events need.
pub async fn update_admin_webhook(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<WebhookForm>,
) -> AppResult<Json<AdminWebhook>> {
    let current = find(&state, id).await?;
    let acting = acting(&state, &auth, true).await?;
    role::authorize(holds_required_permissions(&acting, &current.events))?;
    let actor = role::of_account(&state.db, auth.account_id)
        .await?
        .unwrap_or_else(Role::nobody);
    let url = form.url.unwrap_or(current.url);
    let events = form.events.map_or(current.events, normalize_events);
    let template = match form.template {
        Some(t) => Some(t).filter(|t| !t.is_empty()),
        None => current.template,
    };
    let taken = url_taken(&state, &url, Some(id)).await?;
    validate(&url, &events, template.as_deref(), &actor, taken)?;
    let row = sqlx::query_as!(
        Row,
        r#"UPDATE webhooks SET url = $2, events = $3, template = $4, updated_at = now()
           WHERE id = $1
           RETURNING id, url, events, template, enabled, secret, created_at, updated_at"#,
        id,
        url,
        &events,
        template,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row.entity(&acting)))
}

async fn set_enabled(
    state: AppState,
    auth: AuthenticatedUser,
    id: i64,
    enabled: bool,
) -> AppResult<Json<AdminWebhook>> {
    find(&state, id).await?;
    let acting = acting(&state, &auth, true).await?;
    let row = sqlx::query_as!(
        Row,
        r#"UPDATE webhooks SET enabled = $2, updated_at = now() WHERE id = $1
           RETURNING id, url, events, template, enabled, secret, created_at, updated_at"#,
        id,
        enabled,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row.entity(&acting)))
}

/// `POST /api/v1/admin/webhooks/:id/enable`.
pub async fn enable_admin_webhook(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminWebhook>> {
    set_enabled(state, auth, id, true).await
}

/// `POST /api/v1/admin/webhooks/:id/disable`.
pub async fn disable_admin_webhook(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminWebhook>> {
    set_enabled(state, auth, id, false).await
}

/// `POST /api/v1/admin/webhooks/:id/secret/rotate`: `rotate_secret!`.
pub async fn rotate_admin_webhook_secret(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminWebhook>> {
    find(&state, id).await?;
    let acting = acting(&state, &auth, true).await?;
    let row = sqlx::query_as!(
        Row,
        r#"UPDATE webhooks SET secret = $2, updated_at = now() WHERE id = $1
           RETURNING id, url, events, template, enabled, secret, created_at, updated_at"#,
        id,
        random_secret(),
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(row.entity(&acting)))
}

/// `DELETE /api/v1/admin/webhooks/:id`: for a role holding what its events
/// need.
pub async fn delete_admin_webhook(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let current = find(&state, id).await?;
    let acting = acting(&state, &auth, true).await?;
    role::authorize(holds_required_permissions(&acting, &current.events))?;
    sqlx::query!("DELETE FROM webhooks WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    Ok(Json(serde_json::json!({})))
}

#[cfg(test)]
mod tests {
    use super::template_parses;

    #[test]
    fn templates_parse_as_mastodon_parses_them() {
        assert!(template_parses("plain text"));
        assert!(template_parses(
            r#"{"text": "{{object.id}} by {{object.account.username}}"}"#
        ));
        assert!(template_parses("{{object.statuses.0.id}}"));
        assert!(template_parses("{{}}"));
        assert!(!template_parses("{{object.}}"));
        assert!(!template_parses("{{ object.id }}"));
        assert!(!template_parses("{{Object}}"));
        assert!(!template_parses("{{object.id"));
    }
}
