use super::extractors::QueryOrJson;
use crate::{
    error::{AppError, AppResult},
    middleware::{AuthenticatedUser, ResolvedInstance},
    state::AppState,
};
use axum::{
    extract::{Extension, Multipart, Path, Query},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

mod accounts;
mod blocks;
mod federation;
mod reports;
mod trends;

pub use accounts::*;
pub use blocks::*;
pub use federation::*;
pub use reports::*;
pub use trends::*;

/// `limit` and the id bounds of `to_a_paginated_by_id`.
#[derive(Debug, Deserialize)]
pub struct PageParams {
    pub limit: Option<super::extractors::FlexId>,
    pub max_id: Option<super::extractors::FlexId>,
    pub since_id: Option<super::extractors::FlexId>,
    pub min_id: Option<super::extractors::FlexId>,
}

impl PageParams {
    /// `limit_param(default, max)`.
    pub(super) fn limit(&self, default: i64, max: i64) -> i64 {
        self.limit.map_or(default, |l| l.0.abs().min(max))
    }
}

// ── Admin auth guard ──────────────────────────────────────────────────────

pub use crate::moderation::role::flag as perm;

/// Mastodon's `UserRole#computed_permissions` of an account's role, as
/// `(position, permissions)`; see [`crate::moderation::role`].
pub(super) async fn computed_permissions(
    state: &AppState,
    account_id: i64,
) -> AppResult<(i32, i64)> {
    let role = crate::moderation::role::of_account(&state.db, account_id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    Ok((role.position, role.computed))
}

/// `authorize` against a policy that is a single `role.can?(flag)`, judged by
/// the acting role (nobody's when the user is disabled).
pub(crate) async fn require_permission(
    state: &AppState,
    account_id: i64,
    flag: i64,
) -> AppResult<()> {
    let role = crate::moderation::role::acting(&state.db, account_id).await?;
    crate::moderation::role::authorize(role.can(&[flag]))
}

// ── POST /api/v1/admin/measures ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MeasuresRequest {
    pub keys: Vec<String>,
    pub start_at: Option<String>,
    pub end_at: Option<String>,
}

pub async fn get_measures(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    QueryOrJson(body): QueryOrJson<MeasuresRequest>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
    require_permission(&state, auth.account_id, perm::VIEW_DASHBOARD).await?;

    let start: chrono::NaiveDateTime = body
        .start_at
        .as_deref()
        .and_then(parse_admin_date)
        .unwrap_or_else(|| chrono::Utc::now().naive_utc() - chrono::Duration::days(7));
    let end: chrono::NaiveDateTime = body
        .end_at
        .as_deref()
        .and_then(parse_admin_date)
        .unwrap_or_else(|| chrono::Utc::now().naive_utc());
    let prev_start = start - (end - start);

    let mut result = Vec::new();

    for key in &body.keys {
        let measure = match key.as_str() {
            "new_users" => {
                let total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM users WHERE created_at BETWEEN $1 AND $2",
                    start,
                    end,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let previous_total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM users WHERE created_at BETWEEN $1 AND $2",
                    prev_start,
                    start,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let data = sqlx::query!(
                    r#"SELECT axis.day::timestamp,
                              (SELECT COUNT(*) FROM users
                               WHERE date_trunc('day', created_at)::date = axis.day) AS n
                       FROM (SELECT generate_series($1::timestamp, $2::timestamp, '1 day')::date AS day) AS axis
                       ORDER BY axis.day"#,
                    start, end,
                ).fetch_all(&state.db).await?;
                serde_json::json!({
                    "key": key, "unit": null,
                    "total": total.to_string(),
                    "human_value": total.to_string(),
                    "previous_total": previous_total.to_string(),
                    "data": data.iter().map(|r| serde_json::json!({
                        "date": r.day.map(super::convert::mastodon_date).unwrap_or_default(),
                        "value": r.n.unwrap_or(0).to_string(),
                    })).collect::<Vec<_>>(),
                })
            }
            "active_users" => {
                // Matches Mastodon: counts users by current_sign_in_at (updated on every login).
                let total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM users WHERE current_sign_in_at BETWEEN $1 AND $2",
                    start,
                    end,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let previous_total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM users WHERE current_sign_in_at BETWEEN $1 AND $2",
                    prev_start,
                    start,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let data = sqlx::query!(
                    r#"SELECT axis.day::timestamp,
                              (SELECT COUNT(*) FROM users
                               WHERE date_trunc('day', current_sign_in_at)::date = axis.day) AS n
                       FROM (SELECT generate_series($1::timestamp, $2::timestamp, '1 day')::date AS day) AS axis
                       ORDER BY axis.day"#,
                    start, end,
                ).fetch_all(&state.db).await?;
                serde_json::json!({
                    "key": key, "unit": null,
                    "total": total.to_string(),
                    "human_value": total.to_string(),
                    "previous_total": previous_total.to_string(),
                    "data": data.iter().map(|r| serde_json::json!({
                        "date": r.day.map(super::convert::mastodon_date).unwrap_or_default(),
                        "value": r.n.unwrap_or(0).to_string(),
                    })).collect::<Vec<_>>(),
                })
            }
            "new_statuses" => {
                let total = sqlx::query_scalar!(
                    r#"SELECT COUNT(*) FROM statuses s JOIN accounts a ON a.id = s.account_id
                       WHERE a.domain IS NULL AND s.created_at BETWEEN $1 AND $2 AND s.deleted_at IS NULL"#,
                    start, end,
                ).fetch_one(&state.db).await?.unwrap_or(0);
                let previous_total = sqlx::query_scalar!(
                    r#"SELECT COUNT(*) FROM statuses s JOIN accounts a ON a.id = s.account_id
                       WHERE a.domain IS NULL AND s.created_at BETWEEN $1 AND $2 AND s.deleted_at IS NULL"#,
                    prev_start, start,
                ).fetch_one(&state.db).await?.unwrap_or(0);
                let data = sqlx::query!(
                    r#"SELECT axis.day::timestamp,
                              (SELECT COUNT(*) FROM statuses s JOIN accounts a ON a.id = s.account_id
                               WHERE a.domain IS NULL AND s.deleted_at IS NULL
                                 AND date_trunc('day', s.created_at)::date = axis.day) AS n
                       FROM (SELECT generate_series($1::timestamp, $2::timestamp, '1 day')::date AS day) AS axis
                       ORDER BY axis.day"#,
                    start, end,
                ).fetch_all(&state.db).await?;
                serde_json::json!({
                    "key": key, "unit": null,
                    "total": total.to_string(),
                    "human_value": total.to_string(),
                    "previous_total": previous_total.to_string(),
                    "data": data.iter().map(|r| serde_json::json!({
                        "date": r.day.map(super::convert::mastodon_date).unwrap_or_default(),
                        "value": r.n.unwrap_or(0).to_string(),
                    })).collect::<Vec<_>>(),
                })
            }
            "opened_reports" => {
                let total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM reports WHERE created_at BETWEEN $1 AND $2",
                    start,
                    end,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let previous_total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM reports WHERE created_at BETWEEN $1 AND $2",
                    prev_start,
                    start,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let data = sqlx::query!(
                    r#"SELECT axis.day::timestamp,
                              (SELECT COUNT(*) FROM reports
                               WHERE date_trunc('day', created_at)::date = axis.day) AS n
                       FROM (SELECT generate_series($1::timestamp, $2::timestamp, '1 day')::date AS day) AS axis
                       ORDER BY axis.day"#,
                    start, end,
                ).fetch_all(&state.db).await?;
                serde_json::json!({
                    "key": key, "unit": null,
                    "total": total.to_string(), "human_value": total.to_string(),
                    "previous_total": previous_total.to_string(),
                    "data": data.iter().map(|r| serde_json::json!({
                        "date": r.day.map(super::convert::mastodon_date).unwrap_or_default(),
                        "value": r.n.unwrap_or(0).to_string(),
                    })).collect::<Vec<_>>(),
                })
            }
            "resolved_reports" => {
                let total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM reports WHERE action_taken_at BETWEEN $1 AND $2",
                    start,
                    end,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let previous_total = sqlx::query_scalar!(
                    "SELECT COUNT(*) FROM reports WHERE action_taken_at BETWEEN $1 AND $2",
                    prev_start,
                    start,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(0);
                let data = sqlx::query!(
                    r#"SELECT axis.day::timestamp,
                              (SELECT COUNT(*) FROM reports
                               WHERE date_trunc('day', action_taken_at)::date = axis.day) AS n
                       FROM (SELECT generate_series($1::timestamp, $2::timestamp, '1 day')::date AS day) AS axis
                       ORDER BY axis.day"#,
                    start, end,
                ).fetch_all(&state.db).await?;
                serde_json::json!({
                    "key": key, "unit": null,
                    "total": total.to_string(), "human_value": total.to_string(),
                    "previous_total": previous_total.to_string(),
                    "data": data.iter().map(|r| serde_json::json!({
                        "date": r.day.map(super::convert::mastodon_date).unwrap_or_default(),
                        "value": r.n.unwrap_or(0).to_string(),
                    })).collect::<Vec<_>>(),
                })
            }
            "interactions" => {
                // Approximates Mastodon's Redis-backed interactions counter:
                // statuses posted + favourites + follows by local users.
                macro_rules! count_interactions {
                    ($s:expr, $e:expr) => {
                        sqlx::query_scalar!(
                            r#"SELECT
                               (SELECT COUNT(*) FROM statuses s JOIN accounts a ON a.id = s.account_id
                                WHERE a.domain IS NULL AND s.deleted_at IS NULL AND s.created_at BETWEEN $1 AND $2)
                               +
                               (SELECT COUNT(*) FROM favourites f JOIN accounts a ON a.id = f.account_id
                                WHERE a.domain IS NULL AND f.created_at BETWEEN $1 AND $2)
                               +
                               (SELECT COUNT(*) FROM follows f JOIN accounts a ON a.id = f.account_id
                                WHERE a.domain IS NULL AND f.created_at BETWEEN $1 AND $2)"#,
                            $s, $e,
                        ).fetch_one(&state.db).await?.unwrap_or(0)
                    };
                }
                let total = count_interactions!(start, end);
                let previous_total = count_interactions!(prev_start, start);
                let data = sqlx::query!(
                    r#"SELECT axis.day::timestamp,
                              (SELECT COUNT(*) FROM statuses s JOIN accounts a ON a.id = s.account_id
                               WHERE a.domain IS NULL AND s.deleted_at IS NULL
                                 AND date_trunc('day', s.created_at)::date = axis.day)
                              +
                              (SELECT COUNT(*) FROM favourites f JOIN accounts a ON a.id = f.account_id
                               WHERE a.domain IS NULL
                                 AND date_trunc('day', f.created_at)::date = axis.day)
                              +
                              (SELECT COUNT(*) FROM follows f2 JOIN accounts a ON a.id = f2.account_id
                               WHERE a.domain IS NULL
                                 AND date_trunc('day', f2.created_at)::date = axis.day)
                              AS n
                       FROM (SELECT generate_series($1::timestamp, $2::timestamp, '1 day')::date AS day) AS axis
                       ORDER BY axis.day"#,
                    start, end,
                ).fetch_all(&state.db).await?;
                serde_json::json!({
                    "key": key, "unit": null,
                    "total": total.to_string(),
                    "human_value": total.to_string(),
                    "previous_total": previous_total.to_string(),
                    "data": data.iter().map(|r| serde_json::json!({
                        "date": r.day.map(super::convert::mastodon_date).unwrap_or_default(),
                        "value": r.n.unwrap_or(0).to_string(),
                    })).collect::<Vec<_>>(),
                })
            }
            _ => serde_json::json!({
                "key": key, "unit": null, "total": "0",
                "human_value": "0", "previous_total": "0", "data": [],
            }),
        };
        result.push(measure);
    }

    Ok(Json(result))
}

fn locale_name(code: &str) -> &'static str {
    match code {
        "ko" => "Korean",
        "en" => "English",
        "ja" => "Japanese",
        "zh" => "Chinese",
        "fr" => "French",
        "de" => "German",
        "es" => "Spanish",
        "pt" => "Portuguese",
        "ru" => "Russian",
        "ar" => "Arabic",
        _ => "Unknown",
    }
}

fn parse_admin_date(s: &str) -> Option<chrono::NaiveDateTime> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&chrono::Utc).naive_utc());
    }
    // Mastodon sends date-only strings like "2026-04-27"
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return date.and_hms_opt(0, 0, 0);
    }
    None
}

fn human_size(bytes: i64) -> String {
    const KB: i64 = 1024;
    const MB: i64 = 1024 * KB;
    const GB: i64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

fn parse_redis_info_field(info: &str, field: &str) -> Option<String> {
    info.lines()
        .find(|l| l.starts_with(field))
        .and_then(|l| l.split_once(':').map(|x| x.1))
        .map(|v| v.trim().to_string())
}

// ── POST /api/v1/admin/dimensions ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DimensionsRequest {
    pub keys: Vec<String>,
    pub start_at: Option<String>,
    pub end_at: Option<String>,
    pub limit: Option<i64>,
}

pub async fn get_dimensions(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    QueryOrJson(body): QueryOrJson<DimensionsRequest>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
    require_permission(&state, auth.account_id, perm::VIEW_DASHBOARD).await?;

    let start: chrono::NaiveDateTime = body
        .start_at
        .as_deref()
        .and_then(parse_admin_date)
        .unwrap_or_else(|| chrono::Utc::now().naive_utc() - chrono::Duration::days(7));
    let end: chrono::NaiveDateTime = body
        .end_at
        .as_deref()
        .and_then(parse_admin_date)
        .unwrap_or_else(|| chrono::Utc::now().naive_utc());
    let limit = body.limit.unwrap_or(10).clamp(1, 50);

    let mut result = Vec::new();

    for key in &body.keys {
        let dimension = match key.as_str() {
            "servers" => {
                let rows = sqlx::query!(
                    r#"SELECT COALESCE(a.domain, 'local') AS server, COUNT(*) AS n
                       FROM statuses s JOIN accounts a ON a.id = s.account_id
                       WHERE s.created_at BETWEEN $1 AND $2 AND s.deleted_at IS NULL
                       GROUP BY COALESCE(a.domain, 'local') ORDER BY n DESC LIMIT $3"#,
                    start,
                    end,
                    limit,
                )
                .fetch_all(&state.db)
                .await?;
                serde_json::json!({
                    "key": key,
                    "data": rows.iter().map(|r| {
                        let v = r.n.unwrap_or(0).to_string();
                        serde_json::json!({
                            "key": r.server,
                            "human_key": r.server,
                            "value": v,
                            "unit": null,
                            "human_value": v,
                        })
                    }).collect::<Vec<_>>(),
                })
            }
            "sources" => {
                let rows = sqlx::query!(
                    r#"SELECT COALESCE(a.name, 'web') AS name, COUNT(*) AS n
                       FROM users u
                       LEFT JOIN oauth_applications a ON a.id = u.created_by_application_id
                       WHERE u.created_at BETWEEN $1 AND $2
                       GROUP BY a.name ORDER BY n DESC LIMIT $3"#,
                    start,
                    end,
                    limit,
                )
                .fetch_all(&state.db)
                .await?;
                serde_json::json!({
                    "key": key,
                    "data": rows.iter().map(|r| {
                        let v = r.n.unwrap_or(0).to_string();
                        serde_json::json!({
                            "key": r.name, "human_key": r.name,
                            "value": v, "unit": null, "human_value": v,
                        })
                    }).collect::<Vec<_>>(),
                })
            }
            "languages" => {
                let rows = sqlx::query!(
                    r#"SELECT COALESCE(u.locale, 'und') AS locale, COUNT(*) AS n
                       FROM users u
                       WHERE u.current_sign_in_at BETWEEN $1 AND $2
                         AND u.locale IS NOT NULL
                       GROUP BY u.locale ORDER BY n DESC LIMIT $3"#,
                    start,
                    end,
                    limit,
                )
                .fetch_all(&state.db)
                .await?;
                serde_json::json!({
                    "key": key,
                    "data": rows.iter().map(|r| {
                        let v = r.n.unwrap_or(0).to_string();
                        let human = locale_name(r.locale.as_deref().unwrap_or("und"));
                        serde_json::json!({
                            "key": r.locale, "human_key": human,
                            "value": v, "unit": null, "human_value": v,
                        })
                    }).collect::<Vec<_>>(),
                })
            }
            "software_versions" => {
                let pg_version_raw: String = sqlx::query_scalar!("SELECT version()")
                    .fetch_one(&state.db)
                    .await?
                    .unwrap_or_default();
                let pg_version = pg_version_raw
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("unknown")
                    .to_string();

                let mut redis = state.redis.clone();
                let redis_info: String = redis::cmd("INFO")
                    .arg("server")
                    .query_async(&mut redis)
                    .await
                    .unwrap_or_default();
                let redis_version = parse_redis_info_field(&redis_info, "redis_version")
                    .unwrap_or_else(|| "unknown".to_string());

                let eunha_version = crate::version::EUNHA_FULL.to_string();

                serde_json::json!({
                    "key": key,
                    "data": [
                        {"key": "mastodon", "human_key": "Eunha", "value": eunha_version.clone(), "human_value": eunha_version},
                        {"key": "postgresql", "human_key": "PostgreSQL", "value": pg_version.clone(), "human_value": pg_version},
                        {"key": "redis", "human_key": "Redis", "value": redis_version.clone(), "human_value": redis_version},
                    ],
                })
            }
            "space_usage" => {
                let pg_size: i64 =
                    sqlx::query_scalar!("SELECT pg_database_size(current_database())")
                        .fetch_one(&state.db)
                        .await?
                        .unwrap_or(0);

                let redis_size = if state.config.redis_process_metrics
                    && !state.redis_keys.is_shared()
                    && state.config.redis_coordination_url.is_none()
                {
                    let mut redis = state.redis.clone();
                    let redis_mem_info: String = redis::cmd("INFO")
                        .arg("memory")
                        .query_async(&mut redis)
                        .await
                        .unwrap_or_default();
                    parse_redis_info_field(&redis_mem_info, "used_memory")
                        .and_then(|v| v.parse::<i64>().ok())
                } else {
                    None
                };

                let media_size: i64 = sqlx::query_scalar!(
                    r#"SELECT
                       COALESCE((SELECT SUM(COALESCE(file_file_size,0) + COALESCE(thumbnail_file_size,0)) FROM media_attachments), 0)
                       + COALESCE((SELECT SUM(COALESCE(image_file_size,0)) FROM custom_emojis), 0)
                       + COALESCE((SELECT SUM(COALESCE(image_file_size,0)) FROM preview_cards), 0)
                       + COALESCE((SELECT SUM(COALESCE(avatar_file_size,0) + COALESCE(header_file_size,0)) FROM accounts), 0)"#
                ).fetch_one(&state.db).await?.unwrap_or(0);

                serde_json::json!({
                    "key": key,
                    "data": [
                        {
                            "key": "postgresql", "human_key": "PostgreSQL",
                            "value": pg_size.to_string(), "unit": "bytes",
                            "human_value": human_size(pg_size),
                        },
                        redis_size.map(|bytes| serde_json::json!({
                            "key": "redis", "human_key": "Redis",
                            "value": bytes.to_string(), "unit": "bytes",
                            "human_value": human_size(bytes),
                        })).unwrap_or_else(|| serde_json::json!({
                            "key": "redis", "human_key": "Redis",
                            "value": null, "unit": "bytes",
                            "human_value": "Unavailable for shared Redis",
                        })),
                        {
                            "key": "media", "human_key": "Media storage",
                            "value": media_size.to_string(), "unit": "bytes",
                            "human_value": human_size(media_size),
                        },
                    ],
                })
            }
            _ => serde_json::json!({"key": key, "data": []}),
        };
        result.push(dimension);
    }

    Ok(Json(result))
}

// ── POST /api/v1/admin/retention ─────────────────────────────────────────
//
// Matches Mastodon: cohorts are groups of users who signed up in the same
// period; a user is "retained" in a later period if current_sign_in_at
// falls within that period (i.e. they logged in again).

#[derive(Debug, Deserialize)]
pub struct RetentionRequest {
    pub start_at: Option<String>,
    pub end_at: Option<String>,
    pub frequency: Option<String>, // "day" or "month"
}

pub async fn get_retention(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    QueryOrJson(body): QueryOrJson<RetentionRequest>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
    require_permission(&state, auth.account_id, perm::VIEW_DASHBOARD).await?;

    let start: chrono::NaiveDateTime = body
        .start_at
        .as_deref()
        .and_then(parse_admin_date)
        .unwrap_or_else(|| chrono::Utc::now().naive_utc() - chrono::Duration::days(30));
    let end: chrono::NaiveDateTime = body
        .end_at
        .as_deref()
        .and_then(parse_admin_date)
        .unwrap_or_else(|| chrono::Utc::now().naive_utc());
    let frequency = match body.frequency.as_deref().unwrap_or("day") {
        "month" => "month",
        _ => "day",
    };

    // Mirrors Mastodon's retention SQL exactly: for every (cohort_period,
    // retention_period) pair where retention_period >= cohort_period, count
    // users whose signup was in cohort_period and whose current_sign_in_at
    // is >= retention_period.
    let rows = sqlx::query!(
        r#"SELECT
               axis.cohort_period::timestamp,
               axis.retention_period::timestamp,
               (
                 WITH new_users AS (
                   SELECT users.id FROM users
                   WHERE date_trunc($3, users.created_at)::date = axis.cohort_period
                 ),
                 retained_users AS (
                   SELECT users.id FROM users
                   INNER JOIN new_users ON new_users.id = users.id
                   WHERE date_trunc($3, users.current_sign_in_at) >= axis.retention_period
                 )
                 SELECT ARRAY[
                   count(*)::bigint,
                   (count(*)::float /
                    GREATEST((SELECT count(*) FROM new_users), 1) * 1000000)::bigint
                 ]
                 FROM retained_users
               ) AS retention_value_and_rate
           FROM (
             WITH cohort_periods AS (
               SELECT generate_series(
                 date_trunc($3, $1::timestamp)::date,
                 date_trunc($3, $2::timestamp)::date,
                 ('1 ' || $3)::interval
               ) AS cohort_period
             ),
             retention_periods AS (
               SELECT cohort_period AS retention_period FROM cohort_periods
             )
             SELECT * FROM cohort_periods, retention_periods
             WHERE retention_period >= cohort_period
           ) AS axis
           ORDER BY axis.cohort_period, axis.retention_period"#,
        start,
        end,
        frequency,
    )
    .fetch_all(&state.db)
    .await?;

    let mut cohorts: indexmap::IndexMap<chrono::NaiveDateTime, Vec<serde_json::Value>> =
        indexmap::IndexMap::new();

    for row in &rows {
        let cohort_period = match row.cohort_period {
            Some(p) => p,
            None => continue,
        };
        let retention_period = match row.retention_period {
            Some(p) => p,
            None => continue,
        };
        let (value, rate_millionths) = match row.retention_value_and_rate.as_deref() {
            Some([v, r]) => (*v, *r),
            _ => (0, 0),
        };
        let rate = rate_millionths as f64 / 1_000_000.0;

        cohorts
            .entry(cohort_period)
            .or_default()
            .push(serde_json::json!({
                "date": super::convert::mastodon_date(retention_period),
                "rate": rate,
                "value": value.to_string(),
            }));
    }

    let data: Vec<serde_json::Value> = cohorts
        .into_iter()
        .map(|(period, entries)| {
            let cohort_size = entries
                .first()
                .and_then(|e| e["value"].as_str())
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            serde_json::json!({
                "period": super::convert::mastodon_date(period),
                "frequency": frequency,
                "cohort_size": cohort_size,
                "data": entries,
            })
        })
        .collect();

    Ok(Json(data))
}

// ── Admin CustomEmoji type ────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AdminCustomEmoji {
    pub id: String,
    pub shortcode: String,
    pub url: String,
    pub static_url: String,
    pub visible_in_picker: bool,
    pub disabled: bool,
    pub category: Option<String>,
}

// ── GET /api/v1/admin/custom_emojis ──────────────────────────────────────

pub async fn list_admin_custom_emojis(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminCustomEmoji>>> {
    require_permission(&state, auth.account_id, perm::MANAGE_CUSTOM_EMOJIS).await?;

    let rows = sqlx::query!(
        "SELECT id, shortcode, image_remote_url, visible_in_picker, disabled
         FROM custom_emojis WHERE domain IS NULL ORDER BY shortcode",
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let url = r.image_remote_url.unwrap_or_default();
                AdminCustomEmoji {
                    id: r.id.to_string(),
                    shortcode: r.shortcode,
                    url: url.clone(),
                    static_url: url,
                    visible_in_picker: r.visible_in_picker,
                    disabled: r.disabled,
                    category: None,
                }
            })
            .collect(),
    ))
}

// ── POST /api/v1/admin/custom_emojis ─────────────────────────────────────

pub async fn create_admin_custom_emoji(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    mut multipart: Multipart,
) -> AppResult<Json<AdminCustomEmoji>> {
    require_permission(&state, auth.account_id, perm::MANAGE_CUSTOM_EMOJIS).await?;

    let mut shortcode = String::new();
    let mut image_bytes: Option<Vec<u8>> = None;
    let mut content_type = "image/png".to_string();

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "shortcode" => {
                shortcode = field.text().await.unwrap_or_default();
            }
            "image" => {
                content_type = field.content_type().unwrap_or("image/png").to_string();
                image_bytes = field.bytes().await.ok().map(|b| b.to_vec());
            }
            _ => {}
        }
    }

    if shortcode.is_empty() {
        return Err(AppError::Unprocessable("shortcode is required".into()));
    }
    let image_data =
        image_bytes.ok_or_else(|| AppError::Unprocessable("image is required".into()))?;

    // Upload to storage
    let ext = match content_type.as_str() {
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    };
    let key = format!("emoji/{}.{}", shortcode, ext);
    state
        .storage
        .store(&image_data, &key, &content_type)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("storage: {e}")))?;
    let url = state.storage.public_url(&key);

    let row = if let Some(row) = sqlx::query!(
        r#"UPDATE custom_emojis
           SET image_remote_url = $2, disabled = false, visible_in_picker = true, updated_at = now()
           WHERE shortcode = $1 AND domain IS NULL
           RETURNING id, shortcode, image_remote_url, visible_in_picker, disabled"#,
        shortcode,
        url,
    )
    .fetch_optional(&state.db)
    .await?
    {
        (
            row.id,
            row.shortcode,
            row.image_remote_url,
            row.visible_in_picker,
            row.disabled,
        )
    } else {
        let row = sqlx::query!(
            r#"INSERT INTO custom_emojis (shortcode, image_remote_url, visible_in_picker, created_at, updated_at)
               VALUES ($1, $2, true, now(), now())
               RETURNING id, shortcode, image_remote_url, visible_in_picker, disabled"#,
            shortcode, url,
        )
        .fetch_one(&state.db)
        .await?;
        (
            row.id,
            row.shortcode,
            row.image_remote_url,
            row.visible_in_picker,
            row.disabled,
        )
    };

    let url = row.2.unwrap_or_default();
    Ok(Json(AdminCustomEmoji {
        id: row.0.to_string(),
        shortcode: row.1,
        url: url.clone(),
        static_url: url,
        visible_in_picker: row.3,
        disabled: row.4,
        category: None,
    }))
}

// ── DELETE /api/v1/admin/custom_emojis/:id ───────────────────────────────

pub async fn delete_admin_custom_emoji(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_permission(&state, auth.account_id, perm::MANAGE_CUSTOM_EMOJIS).await?;
    sqlx::query!("DELETE FROM custom_emojis WHERE id = $1", id,)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::OK)
}

// ── PATCH /api/v1/admin/custom_emojis/:id ────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PatchEmojiForm {
    pub shortcode: Option<String>,
    pub visible_in_picker: Option<bool>,
    pub disabled: Option<bool>,
}

pub async fn update_admin_custom_emoji(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Json(form): Json<PatchEmojiForm>,
) -> AppResult<Json<AdminCustomEmoji>> {
    require_permission(&state, auth.account_id, perm::MANAGE_CUSTOM_EMOJIS).await?;
    if let Some(sc) = &form.shortcode {
        sqlx::query!(
            "UPDATE custom_emojis SET shortcode = $1 WHERE id = $2",
            sc,
            id
        )
        .execute(&state.db)
        .await?;
    }
    if let Some(v) = form.visible_in_picker {
        sqlx::query!(
            "UPDATE custom_emojis SET visible_in_picker = $1 WHERE id = $2",
            v,
            id
        )
        .execute(&state.db)
        .await?;
    }
    if let Some(d) = form.disabled {
        sqlx::query!(
            "UPDATE custom_emojis SET disabled = $1 WHERE id = $2",
            d,
            id
        )
        .execute(&state.db)
        .await?;
    }
    let row = sqlx::query!(
        "SELECT id, shortcode, image_remote_url, visible_in_picker, disabled FROM custom_emojis WHERE id = $1",
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let url = row.image_remote_url.unwrap_or_default();
    Ok(Json(AdminCustomEmoji {
        id: row.id.to_string(),
        shortcode: row.shortcode,
        url: url.clone(),
        static_url: url,
        visible_in_picker: row.visible_in_picker,
        disabled: row.disabled,
        category: None,
    }))
}

// ── Admin Tags ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AdminTag {
    pub id: String,
    pub name: String,
    pub url: String,
    /// `REST::TagSerializer#history`, which the admin serializer extends.
    pub history: Vec<super::types::TagHistory>,
    pub trendable: bool,
    pub usable: bool,
    pub requires_review: bool,
    pub listable: bool,
}

fn admin_tag_url(domain: &str, name: &str) -> String {
    format!("https://{domain}/tags/{name}")
}

#[derive(Debug, Deserialize)]
pub struct UpdateAdminTagForm {
    pub trendable: Option<bool>,
    pub usable: Option<bool>,
    pub listable: Option<bool>,
}

#[derive(serde::Deserialize)]
pub struct AdminTagsParams {
    #[serde(flatten)]
    pub pagination: super::types::PaginationParams,
    pub name: Option<String>,
}

pub async fn list_admin_tags(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Query(params): Query<AdminTagsParams>,
) -> AppResult<Json<Vec<AdminTag>>> {
    require_permission(&state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let trendable_by_default = crate::settings::boolean(&state, "trendable_by_default").await;
    let domain = &instance.domain;
    let limit = params.pagination.limit_clamped(100, 100);
    let max_id = params
        .pagination
        .max_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let since_id = params
        .pagination
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let min_id = params
        .pagination
        .min_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let name_filter = params.name.as_deref().map(|s| s.to_lowercase());

    let rows = sqlx::query!(
        r#"SELECT id, name, trendable, usable, listable, reviewed_at
           FROM tags
           WHERE ($2::bigint IS NULL OR id < $2)
             AND ($3::bigint IS NULL OR id > $3)
             AND ($4::bigint IS NULL OR id > $4)
             AND ($5::text IS NULL OR name = $5)
           ORDER BY id DESC
           LIMIT $1"#,
        limit,
        max_id,
        since_id,
        min_id,
        name_filter,
    )
    .fetch_all(&state.db)
    .await?;
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let mut histories = super::tags::fetch_tags_histories(&state.db, &ids).await;

    Ok(Json(
        rows.into_iter()
            .map(|r| AdminTag {
                id: r.id.to_string(),
                history: histories.remove(&r.id).unwrap_or_default(),
                name: r.name.clone(),
                url: admin_tag_url(domain, &r.name),
                // `Tag#trendable`: the column, else `trendable_by_default`.
                trendable: r.trendable.unwrap_or(trendable_by_default),
                usable: r.usable.unwrap_or(true),
                listable: r.listable.unwrap_or(true),
                requires_review: r.reviewed_at.is_none(),
            })
            .collect(),
    ))
}

pub async fn get_admin_tag(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminTag>> {
    require_permission(&state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let trendable_by_default = crate::settings::boolean(&state, "trendable_by_default").await;
    let domain = &instance.domain;
    let r = sqlx::query!(
        "SELECT id, name, trendable, usable, listable, reviewed_at FROM tags WHERE id = $1",
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(AdminTag {
        id: r.id.to_string(),
        history: super::tags::fetch_tags_histories(&state.db, &[r.id])
            .await
            .remove(&r.id)
            .unwrap_or_default(),
        name: r.name.clone(),
        url: admin_tag_url(domain, &r.name),
        trendable: r.trendable.unwrap_or(trendable_by_default),
        usable: r.usable.unwrap_or(true),
        listable: r.listable.unwrap_or(true),
        requires_review: r.reviewed_at.is_none(),
    }))
}

pub async fn update_admin_tag(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Path(id): Path<i64>,
    Json(form): Json<UpdateAdminTagForm>,
) -> AppResult<Json<AdminTag>> {
    require_permission(&state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let trendable_by_default = crate::settings::boolean(&state, "trendable_by_default").await;
    let domain = &instance.domain;
    let r = sqlx::query!(
        r#"UPDATE tags SET
               trendable   = COALESCE($2, trendable),
               usable      = COALESCE($3, usable),
               listable    = COALESCE($4, listable),
               reviewed_at = now(),
               updated_at  = now()
           WHERE id = $1
           RETURNING id, name, trendable, usable, listable, reviewed_at"#,
        id,
        form.trendable,
        form.usable,
        form.listable,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(AdminTag {
        id: r.id.to_string(),
        history: super::tags::fetch_tags_histories(&state.db, &[r.id])
            .await
            .remove(&r.id)
            .unwrap_or_default(),
        name: r.name.clone(),
        url: admin_tag_url(domain, &r.name),
        trendable: r.trendable.unwrap_or(trendable_by_default),
        usable: r.usable.unwrap_or(true),
        listable: r.listable.unwrap_or(true),
        requires_review: r.reviewed_at.is_none(),
    }))
}

pub(super) fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}
