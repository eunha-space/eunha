//! `Admin::ServerChecksController` and `Admin::IgnoredServerChecksController`:
//! the Server Health page, as an API, each check worded as its page words it.
//!
//! Mastodon: app/controllers/admin/server_checks_controller.rb
//! Mastodon: app/controllers/admin/ignored_server_checks_controller.rb
//! Mastodon: app/helpers/admin/server_checks_helper.rb
//! Mastodon: app/policies/admin/server_check_policy.rb
//! Mastodon: app/views/admin/server_checks/index.html.haml

use axum::{
    extract::{Extension, Path},
    Json,
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::role::{self, flag},
    server_checks::{self, status, Check},
    state::AppState,
};

#[derive(Debug, Serialize)]
pub struct ServerCheck {
    pub key: &'static str,
    /// `critical`, `warning`, `suggestion`, `error`, `unprocessed`,
    /// `completed` or `ignored`.
    pub category: &'static str,
    pub ignored: bool,
    pub title: &'static str,
    pub lead: Option<String>,
    /// `admin_server_check_setup_path`: a page here, or documentation.
    pub setup_url: Option<&'static str>,
    pub checked_at: Option<String>,
    pub data: Value,
}

/// `admin.server_checks.checks.<key>.<status>.title`.
fn title(key: &str, check_status: i16) -> &'static str {
    match (key, check_status) {
        ("database_schema", status::ERROR) => "There was an error checking the database schema",
        ("database_schema", status::OK) => "There are no database migrations pending",
        ("database_schema", status::PENDING) => "There are pending database migrations",
        ("database_schema", status::RUNNING) => "Your server is checking the database schema",
        ("database_schema", _) => "Your server hasn't checked the database schema yet",
        ("elastic_search", status::ERROR) => {
            "There was an error checking Elasticsearch configuration"
        }
        ("elastic_search", status::OK) => "Elasticsearch is configured without issues",
        ("elastic_search", status::PENDING) => "There are issues with Elasticsearch",
        ("elastic_search", status::RUNNING) => "The server is checking Elasticsearch configuration",
        ("elastic_search", _) => "The server hasn't checked Elasticsearch configuration yet",
        ("media_privacy", status::ERROR) => {
            "There was an error checking your server's media privacy"
        }
        ("media_privacy", status::OK) => "Your server media privacy is secure",
        ("media_privacy", status::PENDING) => "The privacy of your users is at risk",
        ("media_privacy", status::RUNNING) => "The server is checking its privacy status",
        ("media_privacy", _) => "The server hasn't checked its privacy status yet",
        ("rules", status::ERROR) => "There was an error checking your server rules",
        ("rules", status::OK) => "Your server rules are set",
        ("rules", status::PENDING) => "Set up server rules",
        ("rules", status::RUNNING) => "The server is checking your rules",
        ("rules", _) => "The server hasn't checked if there are any rules yet",
        ("sidekiq_process", status::ERROR) => "There was an error checking the job queues",
        ("sidekiq_process", status::OK) => "The job queues are being worked",
        ("sidekiq_process", status::PENDING) => "Please review your job workers",
        ("sidekiq_process", status::RUNNING) => "The server is checking the job queues",
        ("sidekiq_process", _) => "The server hasn't checked the job queues yet",
        ("software_version", status::ERROR) => "There was an error checking for software updates",
        ("software_version", status::OK) => "Your instance is up to date",
        ("software_version", status::PENDING) => "Update your instance",
        ("software_version", status::RUNNING) => {
            "The server is checking if there is any software update pending"
        }
        _ => "The server hasn't checked software updates yet",
    }
}

fn data_str<'a>(data: &'a Value, key: &str) -> &'a str {
    data[key].as_str().unwrap_or_default()
}

/// `admin_server_check_lead`, with the commands eunha's own.
fn lead(check: &Check) -> Option<String> {
    if check.status == status::ERROR {
        return Some(data_str(&check.data, "error").to_owned());
    }
    if check.status != status::PENDING {
        return None;
    }
    let data = &check.data;
    let text = match (check.key, data_str(data, "reason")) {
        ("database_schema", _) => {
            "Please run them to ensure the application behaves as expected".to_owned()
        }
        ("rules", _) => {
            "Your server has no published rules. Clear rules help moderation and set expectations"
                .to_owned()
        }
        ("sidekiq_process", _) => format!(
            "Jobs have been waiting in the {} queue(s) with nothing working them",
            data_str(data, "missing_queues_str")
        ),
        ("media_privacy", _) => "Your object storage is misconfigured".to_owned(),
        ("software_version", "urgent") => {
            "A critical update is available, please update as quickly as possible".to_owned()
        }
        ("software_version", "patch") => "There is a bugfix update available".to_owned(),
        ("software_version", _) => "There is a new version available".to_owned(),
        (_, "analysis_index_mismatch") => format!(
            "Elasticsearch index analyzer settings are outdated. Please run eunha search deploy --only-mapping --only {}",
            data_str(data, "mismatched_specifications_indexes_str")
        ),
        (_, "cluster_health_red") => {
            "Elasticsearch cluster is unhealthy (red status), search features are unavailable"
                .to_owned()
        }
        (_, "cluster_health_yellow") => {
            "Elasticsearch cluster is unhealthy (yellow status), you may want to investigate the reason"
                .to_owned()
        }
        (_, "index_mismatch") => format!(
            "Elasticsearch index mappings are outdated. Please run eunha search deploy --only {}",
            data_str(data, "mismatched_indexes_str")
        ),
        (_, "preset") => {
            "Your Elasticsearch cluster has more than one node, but eunha is not configured to use them"
                .to_owned()
        }
        (_, "preset_single_node") => {
            "Your Elasticsearch cluster has only one node, the preset should be set to single_node_cluster"
                .to_owned()
        }
        (_, "version_check") => format!(
            "Elasticsearch {} is running while {} is required",
            data_str(data, "running_version"),
            data_str(data, "required_version")
        ),
        _ => "Could not connect to Elasticsearch. Please check that it is running, or disable full-text search"
            .to_owned(),
    };
    Some(text)
}

/// `admin_server_check_setup_path`.
fn setup_url(check: &Check) -> Option<&'static str> {
    match check.key {
        "rules" => Some("/admin/rules"),
        "software_version" => Some("/admin/software_updates"),
        "media_privacy" => Some("https://docs.joinmastodon.org/admin/optional/object-storage/#S3"),
        "elastic_search"
            if matches!(
                data_str(&check.data, "reason"),
                "preset_single_node" | "preset"
            ) =>
        {
            Some("https://docs.joinmastodon.org/admin/elasticsearch/#scaling")
        }
        _ => None,
    }
}

/// `GET /api/v1/admin/server_checks`.
pub async fn list_server_checks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<ServerCheck>>> {
    auth.require_scope("admin:read")?;
    let acting = role::acting(&state.db, auth.account_id).await?;
    role::authorize(acting.can(&[flag::VIEW_DASHBOARD]))?;
    let checks = server_checks::checks_for(&state, &acting)
        .await
        .map_err(AppError::Internal)?;
    Ok(Json(
        checks
            .iter()
            .map(|check| ServerCheck {
                key: check.key,
                category: check.category(),
                ignored: check.ignored,
                title: title(check.key, check.status),
                lead: lead(check),
                setup_url: setup_url(check),
                checked_at: check
                    .checked_at
                    .map(|at| at.and_utc().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()),
                data: check.data.clone(),
            })
            .collect(),
    ))
}

async fn set_ignored(
    state: AppState,
    auth: AuthenticatedUser,
    key: String,
    ignored: bool,
) -> AppResult<Json<Value>> {
    auth.require_scope("admin:write")?;
    let acting = role::acting(&state.db, auth.account_id).await?;
    role::authorize(server_checks::authorized(&key, &acting))?;
    server_checks::set_ignored(&state, &key, ignored)
        .await
        .map_err(AppError::Internal)?;
    Ok(Json(serde_json::json!({})))
}

/// `POST /api/v1/admin/server_checks/:key/ignore`.
pub async fn ignore_server_check(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(key): Path<String>,
) -> AppResult<Json<Value>> {
    set_ignored(state, auth, key, true).await
}

/// `DELETE /api/v1/admin/server_checks/:key/ignore`.
pub async fn restore_server_check(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(key): Path<String>,
) -> AppResult<Json<Value>> {
    set_ignored(state, auth, key, false).await
}
