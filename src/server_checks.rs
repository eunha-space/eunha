//! `Admin::ServerCheck`: the health checks of the admin's Server Health page.
//! A scheduler runs the slow ones every fifteen minutes and keeps what they
//! found in `admin_server_checks`; the quick ones run when the page is read.
//!
//! Mastodon: app/models/admin/server_check.rb
//! Mastodon: app/models/admin/server_check/database_schema_runner.rb
//! Mastodon: app/models/admin/server_check/elastic_search_runner.rb
//! Mastodon: app/models/admin/server_check/media_privacy_runner.rb
//! Mastodon: app/models/admin/server_check/noop_runner.rb
//! Mastodon: app/models/admin/server_check/result.rb
//! Mastodon: app/models/admin/server_check/rules_runner.rb
//! Mastodon: app/models/admin/server_check/runner.rb
//! Mastodon: app/models/admin/server_check/sidekiq_process_runner.rb
//! Mastodon: app/models/admin/server_check/software_version_runner.rb
//! Mastodon: app/workers/scheduler/server_checks_scheduler.rb

use std::cmp::Ordering;

use anyhow::Result;
use chrono::NaiveDateTime;
use serde_json::{json, Value};

use crate::{
    api::mastodon::admin::{check_enabled, compare_versions, pending as pending_updates},
    moderation::role::{flag, Role},
    search::elasticsearch::{deploy::mapping_changed, deploy::specification_changed, Index},
    state::AppState,
};

pub mod status {
    pub const UNPROCESSED: i16 = 0;
    pub const RUNNING: i16 = 1;
    pub const OK: i16 = 2;
    pub const PENDING: i16 = 3;
    pub const ERROR: i16 = 4;
}

pub mod severity {
    pub const CRITICAL: i16 = 0;
    pub const WARNING: i16 = 1;
}

/// `Admin::ServerCheck::RUNNERS`, by the key each stores its row under,
/// sorted as `checks` sorts them.
pub const KEYS: [&str; 6] = [
    "database_schema",
    "elastic_search",
    "media_privacy",
    "rules",
    "sidekiq_process",
    "software_version",
];

/// `Runner#authorized?`; false for a key no runner has, as `NoopRunner` says.
pub fn authorized(key: &str, role: &Role) -> bool {
    match key {
        "rules" => role.can(&[flag::MANAGE_RULES]),
        "database_schema" | "elastic_search" | "media_privacy" | "sidekiq_process"
        | "software_version" => role.can(&[flag::VIEW_DEVOPS]),
        _ => false,
    }
}

/// `Runner#async?`.
fn is_async(key: &str) -> bool {
    key != "sidekiq_process"
}

/// `Admin::ServerCheck::Result`.
struct Outcome {
    status: i16,
    data: Value,
    severity: i16,
}

fn ok() -> Outcome {
    Outcome {
        status: status::OK,
        data: json!({}),
        severity: severity::CRITICAL,
    }
}

fn pending(data: Value, severity: i16) -> Outcome {
    Outcome {
        status: status::PENDING,
        data,
        severity,
    }
}

fn error(message: impl ToString) -> Outcome {
    Outcome {
        status: status::ERROR,
        data: json!({ "error": message.to_string() }),
        severity: severity::CRITICAL,
    }
}

/// `ServerCheck#run`: an exception is what the check found.
async fn run(state: &AppState, key: &str) -> Outcome {
    let outcome = match key {
        "database_schema" => database_schema(state).await,
        "elastic_search" => Ok(elastic_search(state).await),
        "media_privacy" => Ok(media_privacy(state).await),
        "rules" => rules(state).await,
        "sidekiq_process" => sidekiq_process(state).await,
        "software_version" => software_version(state).await,
        _ => Ok(pending(json!({}), severity::CRITICAL)),
    };
    outcome.unwrap_or_else(|e| {
        tracing::error!(check = key, error = %e, "server check failed");
        error(e)
    })
}

/// `DatabaseSchemaRunner`: migrations waiting, which `eunha migrate` applies.
async fn database_schema(state: &AppState) -> Result<Outcome> {
    Ok(match crate::migrate::pending(&state.db).await? {
        Some(_) => pending(json!({}), severity::CRITICAL),
        None => ok(),
    })
}

/// `RulesRunner`.
async fn rules(state: &AppState) -> Result<Outcome> {
    let any = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM rules WHERE deleted_at IS NULL) AS "e!""#
    )
    .fetch_one(&state.db)
    .await?;
    Ok(if any {
        ok()
    } else {
        pending(json!({}), severity::WARNING)
    })
}

/// `SoftwareVersionRunner`.
async fn software_version(state: &AppState) -> Result<Outcome> {
    let mut outcome = ok();
    outcome.severity = severity::WARNING;
    if !check_enabled(state) {
        return Ok(outcome);
    }
    let updates = pending_updates(state).await?;
    if updates.is_empty() {
        return Ok(outcome);
    }
    let (reason, severity) = if updates.iter().any(|u| u.urgent) {
        ("urgent", severity::CRITICAL)
    } else if updates.iter().any(|u| u.kind == "patch") {
        ("patch", severity::WARNING)
    } else {
        ("regular", severity::WARNING)
    };
    Ok(pending(json!({ "reason": reason }), severity))
}

/// `SidekiqProcessRunner`, for a queue Sidekiq is not behind. Every Eunha
/// process works every queue, so there is no configuration to read; what can
/// still go wrong is that nothing is working a queue, and that shows as jobs
/// long due in it with none being run.
async fn sidekiq_process(state: &AppState) -> Result<Outcome> {
    let missing: Vec<String> = sqlx::query_scalar!(
        r#"SELECT DISTINCT waiting.queue AS "queue!"
           FROM eunha.jobs waiting
           WHERE waiting.dead_at IS NULL
             AND waiting.locked_at IS NULL
             AND waiting.run_at < now() - interval '10 minutes'
             AND NOT EXISTS (
               SELECT 1 FROM eunha.jobs running
               WHERE running.queue = waiting.queue AND running.locked_at IS NOT NULL
             )
           ORDER BY 1"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(if missing.is_empty() {
        ok()
    } else {
        let missing_queues_str = missing.join(", ");
        pending(
            json!({ "missing_queues": missing, "missing_queues_str": missing_queues_str }),
            severity::CRITICAL,
        )
    })
}

/// `MediaPrivacyRunner#run_s3`: whether asking the media bucket for a listing
/// gets one, which would let anyone enumerate every upload. Eunha keeps media
/// in object storage only, so `run_default` has nothing to look at.
async fn media_privacy(state: &AppState) -> Outcome {
    match media_listing_exposed(state).await {
        Ok(true) => pending(
            json!({ "reason": "upload_check_privacy_error_object_storage" }),
            severity::CRITICAL,
        ),
        Ok(false) => ok(),
        Err(e) => error(e),
    }
}

/// `check_s3_listing!`, for the bucket's public alias and its address on the
/// storage host.
pub async fn media_listing_exposed(state: &AppState) -> Result<bool> {
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
        let body = state
            .http
            .get(&url)
            .query(&[("max-keys", "1"), ("x-random", &random[..20])])
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await?
            .text()
            .await?;
        if body.contains("ListBucketResult") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `ElasticSearchRunner::REQUIRED_VERSION`.
const REQUIRED_VERSION: &str = "7.x";

/// `ElasticSearchRunner::INDEXES`.
const INDEXES: [Index; 5] = [
    Index::Instances,
    Index::Accounts,
    Index::Tags,
    Index::Statuses,
    Index::PublicStatuses,
];

/// `ElasticSearchRunner#run`. Its `reset_chewy` reason is about Chewy's own
/// system index, which Eunha does not keep.
async fn elastic_search(state: &AppState) -> Outcome {
    let Some(client) = state.search.as_deref() else {
        return ok();
    };
    let warn = |data: Value| pending(data, severity::WARNING);

    // `running_version` rescues a failed connection into its own reason.
    let Ok(info) = client.send(reqwest::Method::GET, "", None).await else {
        return warn(json!({ "reason": "running_check", "running_version": "missing" }));
    };
    let Some(running_version) = info["version"]["number"].as_str().filter(|v| !v.is_empty()) else {
        return warn(json!({ "reason": "running_check", "running_version": "missing" }));
    };
    let compatible = |version: Option<&str>| {
        version.is_some_and(|v| compare_versions(v, REQUIRED_VERSION) != Ordering::Less)
    };
    if !compatible(Some(running_version))
        && !compatible(info["version"]["minimum_wire_compatibility_version"].as_str())
    {
        return warn(json!({
            "reason": "version_check",
            "running_version": running_version,
            "required_version": REQUIRED_VERSION,
        }));
    }

    let health = match client
        .send(reqwest::Method::GET, "_cluster/health", None)
        .await
    {
        Ok(health) => health,
        Err(e) => return error(e),
    };
    let health_status = health["status"].as_str().unwrap_or_default();
    if health_status == "red" {
        return warn(json!({ "reason": "cluster_health_red" }));
    }

    let mut mismatched = vec![];
    let mut mismatched_specifications = vec![];
    for index in INDEXES {
        match mapping_changed(client, index).await {
            Ok(true) => mismatched.push(index.base_name()),
            Ok(false) => {}
            Err(e) => return error(e),
        }
    }
    if !mismatched.is_empty() {
        return warn(json!({
            "reason": "index_mismatch",
            "mismatched_indexes": mismatched,
            "mismatched_indexes_str": mismatched.join(","),
        }));
    }
    for index in INDEXES {
        match specification_changed(client, index).await {
            Ok(true) => mismatched_specifications.push(index.base_name()),
            Ok(false) => {}
            Err(e) => return error(e),
        }
    }
    if !mismatched_specifications.is_empty() {
        return warn(json!({
            "reason": "analysis_index_mismatch",
            "mismatched_specifications_indexes": mismatched_specifications,
            "mismatched_specifications_indexes_str": mismatched_specifications.join(","),
        }));
    }

    let single_node_preset =
        client.preset().unwrap_or("single_node_cluster") == "single_node_cluster";
    let nodes = health["number_of_nodes"].as_i64().unwrap_or(0);
    if nodes < 2 && !single_node_preset {
        return warn(json!({ "reason": "preset_single_node" }));
    }
    if health_status == "yellow" {
        return warn(json!({ "reason": "cluster_health_yellow" }));
    }
    if (single_node_preset && nodes != 1) || (!single_node_preset && nodes <= 1) {
        return warn(json!({ "reason": "preset" }));
    }
    ok()
}

/// `Scheduler::ServerChecksScheduler#perform`.
pub async fn run_scheduled(state: &AppState) -> Result<()> {
    for key in KEYS.into_iter().filter(|key| is_async(key)) {
        // A run another process has under way, unless it is long dead.
        let started = sqlx::query_scalar!(
            r#"INSERT INTO admin_server_checks (key, status, created_at, updated_at)
               VALUES ($1, $2, now(), now())
               ON CONFLICT (key) DO UPDATE
                 SET status = EXCLUDED.status, updated_at = now()
                 WHERE admin_server_checks.status <> EXCLUDED.status
                    OR admin_server_checks.updated_at <= now() - interval '1 hour'
               RETURNING id"#,
            key,
            status::RUNNING,
        )
        .fetch_optional(&state.db)
        .await?;
        if started.is_none() {
            continue;
        }
        let outcome = run(state, key).await;
        sqlx::query!(
            r#"UPDATE admin_server_checks
               SET status = $2, data = $3, severity = $4,
                   checked_at = now(), updated_at = now()
               WHERE key = $1"#,
            key,
            outcome.status,
            outcome.data,
            outcome.severity,
        )
        .execute(&state.db)
        .await?;
    }
    Ok(())
}

/// A check, as `ServerCheck.checks_for` prepares it for someone.
#[derive(Debug, Clone)]
pub struct Check {
    pub key: &'static str,
    pub status: i16,
    pub severity: i16,
    pub ignored: bool,
    pub data: Value,
    pub checked_at: Option<NaiveDateTime>,
}

impl Check {
    /// `ServerCheck#category`.
    pub fn category(&self) -> &'static str {
        if self.ignored {
            return "ignored";
        }
        match self.status {
            status::OK => "completed",
            status::UNPROCESSED | status::RUNNING => "unprocessed",
            status::ERROR => "error",
            _ => match self.severity {
                severity::CRITICAL => "critical",
                severity::WARNING => "warning",
                _ => "suggestion",
            },
        }
    }
}

/// `ServerCheck.checks_for(user)`: what this role may see, a check never run
/// as a fresh row would be, and the synchronous ones run now (not saved).
pub async fn checks_for(state: &AppState, role: &Role) -> Result<Vec<Check>> {
    let keys: Vec<String> = KEYS.iter().map(|k| (*k).to_owned()).collect();
    let rows = sqlx::query!(
        r#"SELECT key, status, severity, ignored, data, checked_at
           FROM admin_server_checks WHERE key = ANY($1)"#,
        &keys,
    )
    .fetch_all(&state.db)
    .await?;
    let mut checks = vec![];
    for key in KEYS.into_iter().filter(|key| authorized(key, role)) {
        let row = rows.iter().find(|row| row.key == key);
        let mut check = Check {
            key,
            status: row.map_or(status::UNPROCESSED, |r| r.status),
            severity: row.map_or(severity::CRITICAL, |r| r.severity),
            ignored: row.is_some_and(|r| r.ignored),
            data: row
                .and_then(|r| r.data.clone())
                .unwrap_or_else(|| json!({})),
            checked_at: row.and_then(|r| r.checked_at),
        };
        if !is_async(key) {
            let outcome = run(state, key).await;
            check.status = outcome.status;
            check.severity = outcome.severity;
            check.data = outcome.data;
            check.checked_at = Some(chrono::Utc::now().naive_utc());
        }
        checks.push(check);
    }
    Ok(checks)
}

/// `check.update_attribute(:ignored, ...)` on `find_or_initialize_by(key:)`.
pub async fn set_ignored(state: &AppState, key: &str, ignored: bool) -> Result<()> {
    sqlx::query!(
        r#"INSERT INTO admin_server_checks (key, ignored, created_at, updated_at)
           VALUES ($1, $2, now(), now())
           ON CONFLICT (key) DO UPDATE SET ignored = EXCLUDED.ignored, updated_at = now()"#,
        key,
        ignored,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(status: i16, severity: i16, ignored: bool) -> Check {
        Check {
            key: "rules",
            status,
            severity,
            ignored,
            data: json!({}),
            checked_at: None,
        }
    }

    #[test]
    fn categories_follow_status_then_severity() {
        assert_eq!(check(status::OK, 0, true).category(), "ignored");
        assert_eq!(check(status::OK, 0, false).category(), "completed");
        assert_eq!(check(status::RUNNING, 0, false).category(), "unprocessed");
        assert_eq!(
            check(status::UNPROCESSED, 0, false).category(),
            "unprocessed"
        );
        assert_eq!(check(status::ERROR, 1, false).category(), "error");
        assert_eq!(check(status::PENDING, 0, false).category(), "critical");
        assert_eq!(check(status::PENDING, 1, false).category(), "warning");
        assert_eq!(check(status::PENDING, 2, false).category(), "suggestion");
    }
}
