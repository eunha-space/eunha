//! Mastodon's terms of service administration (`Admin::TermsOfServiceController`
//! and its `drafts`, `generates`, `histories`, `previews`, `tests` and
//! `distributions`), which upstream serves as web forms, over REST at the same
//! paths under `/api/v1/admin/`. Every one is `TermsOfServicePolicy`'s, which
//! asks for `manage_settings`; see the `terms-of-service-rest-api` divergence.

use axum::{
    extract::{Extension, Path},
    Json,
};
use chrono::{Duration, NaiveDate};
use serde::{Deserialize, Serialize};

use super::{perm, require_permission};
use crate::{
    api::mastodon::{convert::mastodon_date, extractors::FormOrJson},
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::action_log,
    state::AppState,
    terms_of_service::{self as tos, TermsOfService},
};

/// A version as the admin pages show it: its source, its rendered text, and
/// where it stands.
#[derive(Debug, Serialize)]
pub struct AdminTermsOfService {
    /// `null` for a draft not saved yet, and for the text the instance
    /// configuration supplies.
    pub id: Option<String>,
    pub text: String,
    pub changelog: String,
    pub effective_date: Option<String>,
    pub published_at: Option<String>,
    pub notification_sent_at: Option<String>,
    pub effective: bool,
    /// `markdown(text)` and `markdown(changelog)`, as the pages render them.
    pub text_html: String,
    pub changelog_html: String,
}

fn entity(t: &TermsOfService) -> AdminTermsOfService {
    AdminTermsOfService {
        id: (t.id != 0).then(|| t.id.to_string()),
        text: t.text.clone(),
        changelog: t.changelog.clone(),
        effective_date: t.effective_date.map(|d| d.to_string()),
        published_at: t.published_at.map(mastodon_date),
        notification_sent_at: t.notification_sent_at.map(mastodon_date),
        effective: t.effective(),
        text_html: crate::markdown::render(&t.text),
        changelog_html: crate::markdown::render(&t.changelog),
    }
}

async fn authorize(state: &AppState, auth: &AuthenticatedUser, scope: &str) -> AppResult<()> {
    auth.require_scope(scope)?;
    require_permission(state, auth.account_id, perm::MANAGE_SETTINGS).await
}

/// `distribute?`: a published version nobody has been told about yet.
async fn authorize_distribute(
    state: &AppState,
    auth: &AuthenticatedUser,
    scope: &str,
    id: i64,
) -> AppResult<TermsOfService> {
    authorize(state, auth, scope).await?;
    let t = tos::find(&state.db, id).await?;
    if !t.published() || t.notification_sent() {
        return Err(AppError::Forbidden);
    }
    Ok(t)
}

// ── GET /api/v1/admin/terms_of_service ────────────────────────────────────

/// `#index`: `TermsOfService.published.first`, a 404 when there is none.
pub async fn admin_terms_of_service(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<AdminTermsOfService>> {
    authorize(&state, &auth, "admin:read").await?;
    let t = tos::published_first(&state)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(entity(&t)))
}

// ── GET /api/v1/admin/terms_of_service/history ────────────────────────────

/// `Histories#show`: `TermsOfService.published.all`.
pub async fn admin_terms_of_service_history(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminTermsOfService>>> {
    authorize(&state, &auth, "admin:read").await?;
    Ok(Json(
        tos::published_all(&state)
            .await?
            .iter()
            .map(entity)
            .collect(),
    ))
}

// ── GET/PUT /api/v1/admin/terms_of_service/draft ──────────────────────────

/// `Drafts#set_terms_of_service`: the latest draft, or a new one starting
/// from the live text and effective ten days from now.
async fn draft(state: &AppState) -> AppResult<TermsOfService> {
    if let Some(t) = tos::draft_first(&state.db).await? {
        return Ok(t);
    }
    let text = tos::live_first(state)
        .await?
        .map(|t| t.text)
        .unwrap_or_default();
    Ok(TermsOfService::new_draft(
        text,
        Some(tos::today() + Duration::days(10)),
    ))
}

/// `Drafts#show`.
pub async fn admin_terms_of_service_draft(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<AdminTermsOfService>> {
    authorize(&state, &auth, "admin:read").await?;
    Ok(Json(entity(&draft(&state).await?)))
}

#[derive(Debug, Deserialize)]
pub struct DraftForm {
    pub text: Option<String>,
    pub changelog: Option<String>,
    /// `YYYY-MM-DD`; blank, absent or unreadable is nil, as Rails casts it.
    pub effective_date: Option<String>,
    /// `publish` publishes; anything else saves the draft.
    pub action_type: Option<String>,
}

/// `Drafts#update`: save the draft, or with `action_type=publish` publish it,
/// logging `publish` when it was.
pub async fn update_admin_terms_of_service_draft(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    FormOrJson(form): FormOrJson<DraftForm>,
) -> AppResult<Json<AdminTermsOfService>> {
    authorize(&state, &auth, "admin:write").await?;
    let mut t = draft(&state).await?;
    if form.action_type.as_deref() == Some("publish") {
        t.published_at = Some(chrono::Utc::now().naive_utc());
    }
    if let Some(text) = form.text {
        t.text = text;
    }
    if let Some(changelog) = form.changelog {
        t.changelog = changelog;
    }
    if let Some(date) = form.effective_date {
        t.effective_date = NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").ok();
    }
    let saved = tos::save(&state.db, t).await?;
    if saved.published() {
        action_log::log(
            &state.db,
            auth.account_id,
            "publish",
            &action_log::Target::bare("TermsOfService", saved.id),
        )
        .await?;
    }
    Ok(Json(entity(&saved)))
}

// ── GET/POST /api/v1/admin/terms_of_service/generate ──────────────────────

/// `TermsOfService::Generator::TEMPLATE`, Mastodon's
/// `config/templates/terms-of-service.md`.
const TEMPLATE: &str = include_str!("../../../templates/terms-of-service.md");

/// `TermsOfService::Generator::VARIABLES`, each with its
/// `human_attribute_name` for the presence errors.
const VARIABLES: [(&str, &str); 9] = [
    ("admin_email", "Admin email"),
    ("arbitration_address", "Arbitration address"),
    ("arbitration_website", "Arbitration website"),
    ("choice_of_law", "Choice of law"),
    ("dmca_address", "Dmca address"),
    ("dmca_email", "Dmca email"),
    ("domain", "Domain"),
    ("jurisdiction", "Jurisdiction"),
    ("min_age", "Min age"),
];

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Generator {
    pub admin_email: Option<String>,
    pub arbitration_address: Option<String>,
    pub arbitration_website: Option<String>,
    pub choice_of_law: Option<String>,
    pub dmca_address: Option<String>,
    pub dmca_email: Option<String>,
    pub domain: Option<String>,
    pub jurisdiction: Option<String>,
    pub min_age: Option<String>,
}

impl Generator {
    fn get(&self, name: &str) -> Option<&str> {
        match name {
            "admin_email" => self.admin_email.as_deref(),
            "arbitration_address" => self.arbitration_address.as_deref(),
            "arbitration_website" => self.arbitration_website.as_deref(),
            "choice_of_law" => self.choice_of_law.as_deref(),
            "dmca_address" => self.dmca_address.as_deref(),
            "dmca_email" => self.dmca_email.as_deref(),
            "domain" => self.domain.as_deref(),
            "jurisdiction" => self.jurisdiction.as_deref(),
            "min_age" => self.min_age.as_deref(),
            _ => None,
        }
    }

    /// `validates(*VARIABLES, presence: true)`.
    fn errors(&self) -> Vec<String> {
        VARIABLES
            .iter()
            .filter(|(name, _)| self.get(name).is_none_or(|v| v.trim().is_empty()))
            .map(|(_, human)| format!("{human} can't be blank"))
            .collect()
    }

    /// `format(TEMPLATE, VARIABLES.index_with { |key| public_send(key) })`.
    pub fn render(&self) -> String {
        let mut text = TEMPLATE.to_owned();
        for (name, _) in VARIABLES {
            text = text.replace(&format!("%{{{name}}}"), self.get(name).unwrap_or(""));
        }
        text
    }
}

/// `Generates#show`: the form, with the domain and contact address filled in.
pub async fn admin_terms_of_service_generator(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Generator>> {
    authorize(&state, &auth, "admin:read").await?;
    Ok(Json(Generator {
        domain: Some(state.instance.domain.clone()),
        admin_email: Some(crate::settings::site_contact_email(&state).await)
            .filter(|e| !e.is_empty()),
        ..Default::default()
    }))
}

/// `Generates#create`: a new draft from the template.
pub async fn generate_admin_terms_of_service(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    FormOrJson(form): FormOrJson<Generator>,
) -> AppResult<Json<AdminTermsOfService>> {
    authorize(&state, &auth, "admin:write").await?;
    let errors = form.errors();
    if !errors.is_empty() {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )));
    }
    let saved = tos::save(&state.db, TermsOfService::new_draft(form.render(), None)).await?;
    Ok(Json(entity(&saved)))
}

// ── GET /api/v1/admin/terms_of_service/{id}/preview ───────────────────────

#[derive(Debug, Serialize)]
pub struct Preview {
    pub terms_of_service: AdminTermsOfService,
    /// `scope_for_notification.count`: how many would be mailed.
    pub user_count: i64,
}

/// `Previews#show`.
pub async fn admin_terms_of_service_preview(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Preview>> {
    let t = authorize_distribute(&state, &auth, "admin:read", id).await?;
    Ok(Json(Preview {
        user_count: tos::notification_count(&state.db, &t).await?,
        terms_of_service: entity(&t),
    }))
}

// ── POST /api/v1/admin/terms_of_service/{id}/test ─────────────────────────

/// `Tests#create`: the notification, to the administrator alone.
pub async fn test_admin_terms_of_service(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let t = authorize_distribute(&state, &auth, "admin:write", id).await?;
    let email = sqlx::query_scalar!(
        "SELECT email FROM users WHERE account_id = $1",
        auth.account_id
    )
    .fetch_one(&state.db)
    .await?;
    let state = state.clone();
    crate::tenants::spawn(async move {
        tos::send_changed_email(&state, &email, &t).await;
    });
    Ok(Json(serde_json::json!({})))
}

// ── POST /api/v1/admin/terms_of_service/{id}/distribution ─────────────────

/// `Distributions#create`: record that users were told, then tell them.
pub async fn distribute_admin_terms_of_service(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminTermsOfService>> {
    authorize_distribute(&state, &auth, "admin:write", id).await?;
    // `touch(:notification_sent_at)` sets `updated_at` too.
    sqlx::query!(
        "UPDATE terms_of_services SET notification_sent_at = now() AT TIME ZONE 'UTC',
                updated_at = now() AT TIME ZONE 'UTC'
         WHERE id = $1",
        id
    )
    .execute(&state.db)
    .await?;
    let t = tos::find(&state.db, id).await?;
    tos::distribute(&state, t.clone()).await?;
    Ok(Json(entity(&t)))
}
