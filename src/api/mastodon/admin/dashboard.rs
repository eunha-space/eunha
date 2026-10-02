//! `Admin::DashboardController#index`: the counts of what waits for a
//! moderator, and `Admin::SystemCheck`, for `view_dashboard`.

use axum::{extract::Extension, Json};
use serde::Serialize;

use crate::{
    error::AppResult,
    middleware::AuthenticatedUser,
    moderation::role::{self, flag, Role},
    state::AppState,
};

/// `Admin::SystemCheck::Message`, with its text.
#[derive(Debug, Serialize)]
pub struct SystemCheckMessage {
    /// The check's message key, such as `rules_check`.
    pub key: &'static str,
    pub value: Option<String>,
    /// Where acting on it starts: a page here, or documentation.
    pub action: Option<String>,
    /// The label for `action`.
    pub action_label: Option<&'static str>,
    pub critical: bool,
    /// The message, as `admin.system_checks.<key>.message_html` says it.
    pub message: &'static str,
}

#[derive(Debug, Serialize)]
pub struct Dashboard {
    pub pending_appeals_count: i64,
    pub pending_reports_count: i64,
    pub pending_tags_count: i64,
    pub pending_users_count: i64,
    pub system_checks: Vec<SystemCheckMessage>,
}

/// `Admin::SystemCheck::SoftwareVersionCheck`.
async fn software_version_check(
    state: &AppState,
    role: &Role,
) -> AppResult<Option<SystemCheckMessage>> {
    if !role.can(&[flag::VIEW_DEVOPS]) || !super::software_updates::check_enabled(state) {
        return Ok(None);
    }
    let pending = super::software_updates::pending(state).await?;
    if pending.is_empty() {
        return Ok(None);
    }
    let (key, message, critical) = if pending.iter().any(|u| u.urgent) {
        (
            "software_version_critical_check",
            "A critical Mastodon update is available, please update as quickly as possible.",
            true,
        )
    } else if pending.iter().any(|u| u.kind == "patch") {
        (
            "software_version_patch_check",
            "A bugfix Mastodon update is available.",
            false,
        )
    } else {
        (
            "software_version_check",
            "A Mastodon update is available.",
            false,
        )
    };
    Ok(Some(SystemCheckMessage {
        key,
        value: None,
        action: Some("/admin/software_updates".into()),
        action_label: Some("See available updates"),
        critical,
        message,
    }))
}

/// `Admin::SystemCheck::MediaPrivacyCheck#check_media_listing_inaccessible_s3!`:
/// whether asking the media bucket for a listing gets one, which would let
/// anyone enumerate every upload.
async fn media_privacy_check(state: &AppState, role: &Role) -> Option<SystemCheckMessage> {
    if !role.can(&[flag::VIEW_DEVOPS]) {
        return None;
    }
    let storage = &state.config.media_storage;
    let mut urls = vec![format!("{}/", storage.base_url.trim_end_matches('/'))];
    if let Some(endpoint) = storage.endpoint.as_deref() {
        urls.push(format!(
            "{}/{}/",
            endpoint.trim_end_matches('/'),
            storage.bucket
        ));
    }
    urls.dedup();
    for url in urls {
        let random = uuid::Uuid::new_v4().simple().to_string();
        let response = state
            .http
            .get(&url)
            .query(&[("max-keys", "1"), ("x-random", &random[..20])])
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;
        let Ok(response) = response else { continue };
        let Ok(body) = response.text().await else {
            continue;
        };
        if body.contains("ListBucketResult") {
            return Some(SystemCheckMessage {
                key: "upload_check_privacy_error_object_storage",
                value: None,
                action: Some(
                    "https://docs.joinmastodon.org/admin/optional/object-storage/#S3".into(),
                ),
                action_label: Some("Check here for more information"),
                critical: true,
                message:
                    "Your object storage is misconfigured. The privacy of your users is at risk.",
            });
        }
    }
    None
}

/// `Admin::SystemCheck::DatabaseSchemaCheck`: migrations waiting, which
/// `eunha migrate` applies.
async fn database_schema_check(state: &AppState, role: &Role) -> Option<SystemCheckMessage> {
    if !role.can(&[flag::VIEW_DEVOPS]) {
        return None;
    }
    match crate::migrate::pending(&state.db).await {
        Ok(Some(_)) => Some(SystemCheckMessage {
            key: "database_schema_check",
            value: None,
            action: None,
            action_label: None,
            critical: false,
            message: "There are pending database migrations. Please run them to ensure the application behaves as expected",
        }),
        _ => None,
    }
}

/// `Admin::SystemCheck::RulesCheck`.
async fn rules_check(state: &AppState, role: &Role) -> AppResult<Option<SystemCheckMessage>> {
    if !role.can(&[flag::MANAGE_RULES]) {
        return Ok(None);
    }
    let any = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM rules WHERE deleted_at IS NULL) AS "e!""#
    )
    .fetch_one(&state.db)
    .await?;
    Ok((!any).then(|| SystemCheckMessage {
        key: "rules_check",
        value: None,
        action: Some("/admin/rules".into()),
        action_label: Some("Manage server rules"),
        critical: false,
        message: "You haven't defined any server rules.",
    }))
}

/// `GET /api/v1/admin/dashboard`.
pub async fn get_admin_dashboard(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Dashboard>> {
    auth.require_scope("admin:read")?;
    let acting = role::acting(&state.db, auth.account_id).await?;
    role::authorize(acting.can(&[flag::VIEW_DASHBOARD]))?;
    let counts = sqlx::query!(
        r#"SELECT
             (SELECT count(*) FROM appeals WHERE approved_at IS NULL AND rejected_at IS NULL)
               AS "appeals!",
             (SELECT count(*) FROM reports WHERE action_taken_at IS NULL) AS "reports!",
             (SELECT count(DISTINCT t.id) FROM tags t JOIN tag_trends tt ON tt.tag_id = t.id
              WHERE t.reviewed_at IS NULL AND t.requested_review_at IS NOT NULL) AS "tags!",
             (SELECT count(*) FROM users WHERE NOT approved) AS "users!""#
    )
    .fetch_one(&state.db)
    .await?;
    // `Admin::SystemCheck::ACTIVE_CHECKS`, in order, without the
    // Elasticsearch and Sidekiq checks, which have nothing to look at here.
    let mut system_checks = vec![];
    system_checks.extend(software_version_check(&state, &acting).await?);
    system_checks.extend(media_privacy_check(&state, &acting).await);
    system_checks.extend(database_schema_check(&state, &acting).await);
    system_checks.extend(rules_check(&state, &acting).await?);
    Ok(Json(Dashboard {
        pending_appeals_count: counts.appeals,
        pending_reports_count: counts.reports,
        pending_tags_count: counts.tags,
        pending_users_count: counts.users,
        system_checks,
    }))
}
