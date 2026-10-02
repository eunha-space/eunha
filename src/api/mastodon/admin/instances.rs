//! `Admin::InstancesController`, its moderation notes, the instance measures
//! and dimensions of its dashboard, and `Admin::ExportDomainBlocksController`
//! and `Admin::ExportDomainAllowsController`.

use std::collections::HashMap;

use axum::{
    extract::{Extension, Path, Query},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::super::extractors::{Part, Parts};
use super::federation::normalize_domain;
use crate::{
    db::models::domain_severity,
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::{
        action_log::{self, Target},
        role::{self, flag},
    },
    state::AppState,
};

/// Kaminari's `default_per_page`.
const PER_PAGE: i64 = 40;

/// `InstanceModerationNote::CONTENT_SIZE_LIMIT`.
const NOTE_LIMIT: usize = 2_000;

/// `Admin::Import::ROWS_PROCESSING_LIMIT`.
const ROWS_PROCESSING_LIMIT: usize = 20_000;

/// `InstancePolicy`, `DeliveryPolicy` and `InstanceModerationNotePolicy#create?`
/// are all `manage_federation`.
async fn authorize(state: &AppState, auth: &AuthenticatedUser, write: bool) -> AppResult<()> {
    auth.require_scope(if write { "admin:write" } else { "admin:read" })?;
    super::require_permission(state, auth.account_id, flag::MANAGE_FEDERATION).await
}

/// The `instances` view as it would read now: every domain with accounts,
/// blocks or allows, and how many accounts it has.
const INSTANCES: &str = r#"
    WITH domain_counts AS (
      SELECT domain, count(*) AS accounts_count FROM accounts
      WHERE domain IS NOT NULL GROUP BY domain
    ), instances AS (
      SELECT domain, accounts_count FROM domain_counts
      UNION
      SELECT b.domain, COALESCE(c.accounts_count, 0) FROM domain_blocks b
        LEFT JOIN domain_counts c ON c.domain = b.domain
      UNION
      SELECT a.domain, COALESCE(c.accounts_count, 0) FROM domain_allows a
        LEFT JOIN domain_counts c ON c.domain = a.domain
    )"#;

#[derive(Debug, Serialize)]
pub struct InstanceBlock {
    pub id: String,
    pub severity: &'static str,
    pub reject_media: bool,
    pub reject_reports: bool,
    pub private_comment: Option<String>,
    pub public_comment: Option<String>,
    pub obfuscate: bool,
}

#[derive(Debug, Serialize)]
pub struct InstanceAllow {
    pub id: String,
    pub created_at: String,
}

/// An `Instance`, with what the instances list shows of it.
#[derive(Debug, Serialize)]
pub struct AdminInstance {
    pub domain: String,
    pub accounts_count: i64,
    pub domain_block: Option<InstanceBlock>,
    pub domain_allow: Option<InstanceAllow>,
    /// `unavailable?`, and since when.
    pub unavailable: bool,
    pub unavailable_since: Option<String>,
    /// `failure_days`: the days with failures, while not yet unavailable.
    pub failure_days: Option<usize>,
}

#[derive(sqlx::FromRow)]
struct InstanceRow {
    domain: String,
    accounts_count: i64,
    block_id: Option<i64>,
    severity: Option<i32>,
    reject_media: Option<bool>,
    reject_reports: Option<bool>,
    private_comment: Option<String>,
    public_comment: Option<String>,
    obfuscate: Option<bool>,
    allow_id: Option<i64>,
    allow_created_at: Option<chrono::NaiveDateTime>,
    unavailable_since: Option<chrono::NaiveDateTime>,
}

impl InstanceRow {
    fn entity(self, failure_days: Option<usize>) -> AdminInstance {
        let date = super::super::convert::mastodon_date;
        AdminInstance {
            domain_block: self.block_id.map(|id| InstanceBlock {
                id: id.to_string(),
                severity: domain_severity::to_str(self.severity),
                reject_media: self.reject_media.unwrap_or(false),
                reject_reports: self.reject_reports.unwrap_or(false),
                private_comment: self.private_comment.clone(),
                public_comment: self.public_comment.clone(),
                obfuscate: self.obfuscate.unwrap_or(false),
            }),
            domain_allow: self.allow_id.map(|id| InstanceAllow {
                id: id.to_string(),
                created_at: self.allow_created_at.map(date).unwrap_or_default(),
            }),
            unavailable: self.unavailable_since.is_some(),
            unavailable_since: self.unavailable_since.map(date),
            failure_days: failure_days.filter(|_| self.unavailable_since.is_none()),
            domain: self.domain,
            accounts_count: self.accounts_count,
        }
    }
}

const SELECT: &str = r#"
    SELECT i.domain, i.accounts_count,
           b.id AS block_id, b.severity, b.reject_media, b.reject_reports,
           b.private_comment, b.public_comment, b.obfuscate,
           a.id AS allow_id, a.created_at AS allow_created_at,
           u.created_at AS unavailable_since
    FROM instances i
    LEFT JOIN domain_blocks b ON b.domain = i.domain
    LEFT JOIN domain_allows a ON a.domain = i.domain
    LEFT JOIN unavailable_domains u ON u.domain = i.domain"#;

#[derive(Debug, Deserialize, Default)]
pub struct InstanceFilter {
    pub limited: Option<String>,
    pub by_domain: Option<String>,
    pub availability: Option<String>,
    pub page: Option<i64>,
}

/// `GET /api/v1/admin/instances`: `InstanceFilter`, by most accounts, forty a
/// page; in limited federation mode, the allowed domains alone.
pub async fn list_admin_instances(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(filter): Query<InstanceFilter>,
) -> AppResult<Json<Vec<AdminInstance>>> {
    authorize(&state, &auth, false).await?;
    let present = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let limited_mode = state.instance.limited_federation_mode;
    let limited = !limited_mode && present(&filter.limited).is_some();
    let by_domain = if limited_mode {
        None
    } else {
        present(&filter.by_domain)
    };
    let availability = if limited_mode {
        None
    } else {
        present(&filter.availability)
    };
    let failing: Option<Vec<String>> = match availability.as_deref() {
        None | Some("unavailable") => None,
        Some("failing") => Some(
            state
                .delivery_failures
                .warning_domains_map(None)
                .await?
                .into_keys()
                .collect(),
        ),
        Some(other) => {
            return Err(AppError::BadRequest(format!(
                "Unknown availability: {other}"
            )));
        }
    };
    // `scope.merge!` keeps the last ordering: a block's or allow's newest first.
    let order = if limited_mode {
        "a.id DESC"
    } else if limited {
        "b.id DESC"
    } else {
        "i.accounts_count DESC, i.domain"
    };
    let sql = format!(
        "{INSTANCES} {SELECT}
         WHERE ($1::bool IS NOT TRUE OR b.id IS NOT NULL)
           AND ($2::bool IS NOT TRUE OR a.id IS NOT NULL)
           AND ($3::text IS NULL OR i.domain ILIKE '%' || $3 || '%')
           AND ($4::bool IS NOT TRUE OR u.id IS NOT NULL)
           AND ($5::text[] IS NULL OR i.domain = ANY($5))
         ORDER BY {order}
         LIMIT $6 OFFSET $7"
    );
    let page = filter.page.unwrap_or(1).max(1);
    let rows: Vec<InstanceRow> = sqlx::query_as(&sql)
        .bind(limited)
        .bind(limited_mode)
        .bind(by_domain)
        .bind(availability.as_deref() == Some("unavailable"))
        .bind(failing)
        .bind(PER_PAGE)
        .bind((page - 1) * PER_PAGE)
        .fetch_all(&state.db)
        .await?;
    // `preload_delivery_failures!`.
    let domains: Vec<String> = rows.iter().map(|r| r.domain.clone()).collect();
    let failures = state
        .delivery_failures
        .warning_domains_map(Some(&domains))
        .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let days = failures.get(&r.domain).copied();
                r.entity(days)
            })
            .collect(),
    ))
}

#[derive(Debug, Serialize)]
pub struct InstanceNote {
    pub id: String,
    pub content: String,
    pub account: Option<super::super::types::Account>,
    pub created_at: String,
}

#[derive(Debug, Serialize)]
pub struct AdminInstanceDetail {
    #[serde(flatten)]
    pub instance: AdminInstance,
    /// Whether the domain is in the `instances` view at all
    /// (`@instance.persisted?`).
    pub persisted: bool,
    /// `purgeable?`: unavailable, or suspended by a domain block.
    pub purgeable: bool,
    /// `availability_over_days(14)`: each day, and whether deliveries failed.
    pub availability: Vec<Value>,
    pub exhausted_deliveries_days: Vec<String>,
    pub moderation_notes: Vec<InstanceNote>,
}

/// `set_instance`: `Instance.find_or_initialize_by(domain: normalized_domain)`.
fn domain_param(domain: &str) -> AppResult<String> {
    normalize_domain(domain).ok_or(AppError::NotFound)
}

async fn detail(state: &AppState, domain: &str) -> AppResult<AdminInstanceDetail> {
    let sql = format!("{INSTANCES} {SELECT} WHERE i.domain = $1");
    let row: Option<InstanceRow> = sqlx::query_as(&sql)
        .bind(domain)
        .fetch_optional(&state.db)
        .await?;
    let persisted = row.is_some();
    // An instance not in the view still has its block, allow and mark looked
    // up by domain, as `belongs_to` would.
    let row = match row {
        Some(row) => row,
        None => {
            let sql = format!(
                "WITH instances AS (SELECT $1::text AS domain, 0::bigint AS accounts_count) {SELECT}"
            );
            sqlx::query_as(&sql)
                .bind(domain)
                .fetch_one(&state.db)
                .await?
        }
    };
    let tracker = &state.delivery_failures;
    let days = tracker.exhausted_deliveries_days(domain).await?;
    let failure_days = (!days.is_empty()).then_some(days.len());
    let purgeable = row.unavailable_since.is_some()
        || row.severity.is_some_and(|s| s == domain_severity::SUSPEND) && row.block_id.is_some();
    // `availability_over_days(14)`: up to the last failure, or today.
    let today = chrono::Utc::now().date_naive();
    let end = days.last().copied().unwrap_or(today);
    let start = end - chrono::Duration::days(14);
    let availability = start
        .iter_days()
        .take_while(|d| *d <= end)
        .map(|d| json!({"date": d.to_string(), "failing": days.contains(&d)}))
        .collect();
    let notes = sqlx::query!(
        r#"SELECT id, account_id, content, created_at FROM instance_moderation_notes
           WHERE domain = $1 ORDER BY id ASC"#,
        domain
    )
    .fetch_all(&state.db)
    .await?;
    let mut moderation_notes = vec![];
    for note in notes {
        moderation_notes.push(InstanceNote {
            id: note.id.to_string(),
            content: note.content.unwrap_or_default(),
            account: super::api_account(state, note.account_id).await?,
            created_at: super::super::convert::mastodon_date(note.created_at),
        });
    }
    Ok(AdminInstanceDetail {
        instance: row.entity(failure_days),
        persisted,
        purgeable,
        availability,
        exhausted_deliveries_days: days.iter().map(|d| d.to_string()).collect(),
        moderation_notes,
    })
}

/// `GET /api/v1/admin/instances/:domain`: `Instances#show`.
pub async fn get_admin_instance(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(domain): Path<String>,
) -> AppResult<Json<AdminInstanceDetail>> {
    let domain = domain_param(&domain)?;
    authorize(&state, &auth, false).await?;
    Ok(Json(detail(&state, &domain).await?))
}

/// `POST /api/v1/admin/instances/:domain/clear_delivery_errors`.
pub async fn clear_instance_delivery_errors(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(domain): Path<String>,
) -> AppResult<Json<AdminInstanceDetail>> {
    let domain = domain_param(&domain)?;
    authorize(&state, &auth, true).await?;
    state.delivery_failures.clear_failures(&domain).await?;
    Ok(Json(detail(&state, &domain).await?))
}

/// `POST /api/v1/admin/instances/:domain/restart_delivery`: an unavailable
/// domain is delivered to again, and its mark's removal logged.
pub async fn restart_instance_delivery(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(domain): Path<String>,
) -> AppResult<Json<AdminInstanceDetail>> {
    let domain = domain_param(&domain)?;
    authorize(&state, &auth, true).await?;
    let mark = sqlx::query!(
        "SELECT id FROM unavailable_domains WHERE domain = $1",
        domain
    )
    .fetch_optional(&state.db)
    .await?;
    if let Some(mark) = mark {
        state.delivery_failures.restart(&domain).await?;
        action_log::log(
            &state.db,
            auth.account_id,
            "destroy",
            &Target::unavailable_domain(mark.id, &domain),
        )
        .await?;
    }
    Ok(Json(detail(&state, &domain).await?))
}

/// `POST /api/v1/admin/instances/:domain/stop_delivery`: mark the domain
/// unavailable now, and log it.
pub async fn stop_instance_delivery(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(domain): Path<String>,
) -> AppResult<Json<AdminInstanceDetail>> {
    let domain = domain_param(&domain)?;
    authorize(&state, &auth, true).await?;
    // `UnavailableDomain.create!`, which fails validation on a domain
    // already marked.
    let Some(id) = state.delivery_failures.stop(&domain).await? else {
        return Err(AppError::Unprocessable(
            "Validation failed: Domain has already been taken".into(),
        ));
    };
    action_log::log(
        &state.db,
        auth.account_id,
        "create",
        &Target::unavailable_domain(id, &domain),
    )
    .await?;
    Ok(Json(detail(&state, &domain).await?))
}

/// `PurgeDomainService`: mark the severance events about the domain purged,
/// delete every account from it, and its custom emoji.
/// `Admin::DomainPurgeWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct DomainPurgeWorker {
    pub domain: String,
}

impl crate::jobs::Job for DomainPurgeWorker {
    const KIND: &'static str = "Admin::DomainPurgeWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Pull)
        .lock(crate::jobs::Lock::UntilExecuted(
            std::time::Duration::from_secs(7 * 24 * 3600),
        ));

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        purge_domain(state, &self.domain).await
    }
}

pub async fn purge_domain(state: &AppState, domain: &str) -> anyhow::Result<()> {
    sqlx::query!(
        r#"UPDATE relationship_severance_events SET purged = true, updated_at = now()
           WHERE type IN (0, 1) AND target_name = $1"#,
        domain
    )
    .execute(&state.db)
    .await?;
    let accounts = sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE domain = $1 ORDER BY id",
        domain
    )
    .fetch_all(&state.db)
    .await?;
    for account_id in accounts {
        crate::delete_account::call(
            state,
            account_id,
            crate::delete_account::Options {
                reserve_username: false,
                reserve_email: false,
                skip_side_effects: true,
                ..Default::default()
            },
        )
        .await?;
    }
    sqlx::query!("DELETE FROM custom_emojis WHERE domain = $1", domain)
        .execute(&state.db)
        .await?;
    Ok(())
}

/// `DELETE /api/v1/admin/instances/:domain`: `Admin::DomainPurgeWorker`, in the
/// background, logged as destroying the instance.
pub async fn purge_admin_instance(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(domain): Path<String>,
) -> AppResult<Json<Value>> {
    let domain = domain_param(&domain)?;
    authorize(&state, &auth, true).await?;
    action_log::log(
        &state.db,
        auth.account_id,
        "destroy",
        &Target::instance(&domain),
    )
    .await?;
    // `Admin::DomainPurgeWorker`.
    if crate::feed::sync_fanout() {
        if let Err(error) = purge_domain(&state, &domain).await {
            tracing::warn!(domain, %error, "PurgeDomainService failed");
        }
    } else {
        crate::jobs::push(&state, DomainPurgeWorker { domain }).await;
    }
    Ok(Json(json!({})))
}

#[derive(Debug, Deserialize)]
pub struct NoteForm {
    pub content: Option<String>,
}

/// `POST /api/v1/admin/instances/:domain/moderation_notes`.
pub async fn create_instance_moderation_note(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(domain): Path<String>,
    super::super::extractors::Params(form): super::super::extractors::Params<NoteForm>,
) -> AppResult<Json<AdminInstanceDetail>> {
    let domain = domain_param(&domain)?;
    authorize(&state, &auth, true).await?;
    let content = form.content.unwrap_or_default();
    if content.trim().is_empty() {
        return Err(AppError::Unprocessable(
            "Validation failed: Content can't be blank".into(),
        ));
    }
    if content.chars().count() > NOTE_LIMIT {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: Content is too long (maximum is {NOTE_LIMIT} characters)"
        )));
    }
    sqlx::query!(
        r#"INSERT INTO instance_moderation_notes (account_id, content, domain, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())"#,
        auth.account_id,
        content,
        domain,
    )
    .execute(&state.db)
    .await?;
    Ok(Json(detail(&state, &domain).await?))
}

/// `DELETE /api/v1/admin/instances/:domain/moderation_notes/:id`: its author,
/// or a role that manages federation and outranks the author's.
pub async fn delete_instance_moderation_note(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path((_domain, id)): Path<(String, i64)>,
) -> AppResult<Json<Value>> {
    auth.require_scope("admin:write")?;
    let note = sqlx::query!(
        "SELECT account_id FROM instance_moderation_notes WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let owner = note.account_id == auth.account_id;
    if !owner {
        let acting = role::acting(&state.db, auth.account_id).await?;
        let author = role::of_account(&state.db, note.account_id).await?;
        role::authorize(
            acting.can(&[flag::MANAGE_FEDERATION]) && acting.overrides(author.as_ref()),
        )?;
    }
    sqlx::query!("DELETE FROM instance_moderation_notes WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    Ok(Json(json!({})))
}

// ── The instance dashboard ─────────────────────────────────────────────────

/// The instance measures `Admin::Metrics::Measure` has.
pub(super) const MEASURES: &[&str] = &[
    "instance_accounts",
    "instance_statuses",
    "instance_media_attachments",
    "instance_follows",
    "instance_followers",
    "instance_reports",
];

/// The instance dimensions `Admin::Metrics::Dimension` has.
pub(super) const DIMENSIONS: &[&str] = &["instance_accounts", "instance_languages"];

fn domain_of(params: &HashMap<String, Value>, key: &str) -> String {
    params
        .get(key)
        .and_then(|p| p.get("domain"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// One of the instance measures: an all-time total and its daily series in
/// the range, with no previous total, as `total_in_time_range?` is false.
pub(super) async fn measure(
    state: &AppState,
    key: &str,
    params: &HashMap<String, Value>,
    start: chrono::NaiveDateTime,
    end: chrono::NaiveDateTime,
) -> AppResult<Value> {
    let domain = domain_of(params, key);
    // Each measure's total, and the per-day query, by the same join.
    let (total_sql, per_day_sql) = match key {
        "instance_accounts" => (
            "SELECT count(*) FROM accounts WHERE domain = $1",
            "SELECT count(*) FROM accounts
             WHERE domain = $1 AND date_trunc('day', created_at)::date = axis.period",
        ),
        "instance_statuses" => (
            "SELECT count(*) FROM statuses s JOIN accounts a ON a.id = s.account_id
             WHERE a.domain = $1 AND s.deleted_at IS NULL",
            "SELECT count(*) FROM statuses s JOIN accounts a ON a.id = s.account_id
             WHERE a.domain = $1 AND s.deleted_at IS NULL
               AND date_trunc('day', s.created_at)::date = axis.period",
        ),
        "instance_media_attachments" => (
            "SELECT COALESCE(sum(COALESCE(m.file_file_size, 0) + COALESCE(m.thumbnail_file_size, 0)), 0)::bigint
             FROM media_attachments m JOIN accounts a ON a.id = m.account_id WHERE a.domain = $1",
            "SELECT COALESCE(sum(COALESCE(m.file_file_size, 0) + COALESCE(m.thumbnail_file_size, 0)), 0)::bigint
             FROM media_attachments m JOIN accounts a ON a.id = m.account_id
             WHERE a.domain = $1 AND date_trunc('day', m.created_at)::date = axis.period",
        ),
        "instance_follows" => (
            "SELECT count(*) FROM follows f JOIN accounts a ON a.id = f.target_account_id
             WHERE a.domain = $1",
            "SELECT count(*) FROM follows f JOIN accounts a ON a.id = f.target_account_id
             WHERE a.domain = $1 AND date_trunc('day', f.created_at)::date = axis.period",
        ),
        "instance_followers" => (
            "SELECT count(*) FROM follows f JOIN accounts a ON a.id = f.account_id
             WHERE a.domain = $1",
            "SELECT count(*) FROM follows f JOIN accounts a ON a.id = f.account_id
             WHERE a.domain = $1 AND date_trunc('day', f.created_at)::date = axis.period",
        ),
        _ => (
            "SELECT count(*) FROM reports r JOIN accounts a ON a.id = r.target_account_id
             WHERE a.domain = $1",
            "SELECT count(*) FROM reports r JOIN accounts a ON a.id = r.target_account_id
             WHERE a.domain = $1 AND date_trunc('day', r.created_at)::date = axis.period",
        ),
    };
    let total: i64 = sqlx::query_scalar(total_sql)
        .bind(&domain)
        .fetch_one(&state.db)
        .await?;
    let data: Vec<(chrono::NaiveDate, i64)> = sqlx::query_as(&format!(
        "SELECT axis.period, ({per_day_sql}) AS value
         FROM (SELECT generate_series($2::timestamp, $3::timestamp, '1 day')::date AS period) AS axis
         ORDER BY axis.period"
    ))
    .bind(&domain)
    .bind(start)
    .bind(end)
    .fetch_all(&state.db)
    .await?;
    let mut out = json!({
        "key": key,
        "unit": if key == "instance_media_attachments" { Value::from("bytes") } else { Value::Null },
        "total": total.to_string(),
        "data": data.iter().map(|(date, value)| json!({
            "date": super::super::convert::mastodon_date(date.and_hms_opt(0, 0, 0).unwrap_or_default()),
            "value": value.to_string(),
        })).collect::<Vec<_>>(),
    });
    if key == "instance_media_attachments" {
        out["human_value"] = Value::from(super::human_size(total));
    }
    Ok(out)
}

/// One of the instance dimensions.
pub(super) async fn dimension(
    state: &AppState,
    key: &str,
    params: &HashMap<String, Value>,
    limit: i64,
) -> AppResult<Value> {
    let domain = domain_of(params, key);
    let data: Vec<Value> = if key == "instance_accounts" {
        // The domain's most followed accounts.
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT a.username, count(f.id) AS value FROM accounts a
             LEFT JOIN follows f ON f.target_account_id = a.id
             WHERE a.domain = $1 GROUP BY a.id ORDER BY value DESC LIMIT $2",
        )
        .bind(&domain)
        .bind(limit)
        .fetch_all(&state.db)
        .await?;
        rows.into_iter()
            .map(|(username, value)| {
                json!({"key": username, "human_key": username, "value": value.to_string()})
            })
            .collect()
    } else {
        // The languages of the domain's posts, boosts left out.
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT COALESCE(s.language, 'und') AS language, count(*) AS value
             FROM statuses s JOIN accounts a ON a.id = s.account_id
             WHERE a.domain = $1 AND s.reblog_of_id IS NULL
             GROUP BY COALESCE(s.language, 'und') ORDER BY count(*) DESC LIMIT $2",
        )
        .bind(&domain)
        .bind(limit)
        .fetch_all(&state.db)
        .await?;
        rows.into_iter()
            .map(|(language, value)| {
                json!({
                    "key": language,
                    "human_key": super::locale_name(&language),
                    "value": value.to_string(),
                })
            })
            .collect()
    };
    Ok(json!({"key": key, "data": data}))
}

// ── Exporting and importing domain blocks and allows ──────────────────────

fn csv_file(name: &str, body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/csv".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        body,
    )
        .into_response()
}

/// `GET /api/v1/admin/export_domain_blocks/export`: `DomainBlock
/// .with_limitations` oldest first, in Mastodon's columns. Asks `InstancePolicy
/// #index?`.
pub async fn export_domain_blocks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Response> {
    authorize(&state, &auth, false).await?;
    let rows = sqlx::query!(
        r#"SELECT domain, severity, reject_media, reject_reports, public_comment, obfuscate
           FROM domain_blocks
           WHERE COALESCE(severity, 0) IN (0, 1) OR reject_media OR reject_reports
           ORDER BY id ASC"#
    )
    .fetch_all(&state.db)
    .await?;
    let mut out = csv::Writer::from_writer(vec![]);
    let csv_error = |e: csv::Error| AppError::Internal(e.into());
    out.write_record([
        "#domain",
        "#severity",
        "#reject_media",
        "#reject_reports",
        "#public_comment",
        "#obfuscate",
    ])
    .map_err(csv_error)?;
    for r in rows {
        out.write_record([
            r.domain.as_str(),
            domain_severity::to_str(r.severity),
            if r.reject_media { "true" } else { "false" },
            if r.reject_reports { "true" } else { "false" },
            r.public_comment.as_deref().unwrap_or(""),
            if r.obfuscate { "true" } else { "false" },
        ])
        .map_err(csv_error)?;
    }
    let body = String::from_utf8(
        out.into_inner()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("{e}")))?,
    )
    .unwrap_or_default();
    Ok(csv_file("domain_blocks.csv", body))
}

/// `GET /api/v1/admin/export_domain_allows/export`: `DomainAllow
/// .allowed_domains`, under `#domain`.
pub async fn export_domain_allows(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Response> {
    authorize(&state, &auth, false).await?;
    let domains = sqlx::query_scalar!("SELECT domain FROM domain_allows ORDER BY id ASC")
        .fetch_all(&state.db)
        .await?;
    let mut out = csv::Writer::from_writer(vec![]);
    let csv_error = |e: csv::Error| AppError::Internal(e.into());
    out.write_record(["#domain"]).map_err(csv_error)?;
    for domain in domains {
        out.write_record([domain]).map_err(csv_error)?;
    }
    let body = String::from_utf8(
        out.into_inner()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("{e}")))?,
    )
    .unwrap_or_default();
    Ok(csv_file("domain_allows.csv", body))
}

/// One row of an import, by column.
type CsvRow = HashMap<String, Option<String>>;

/// `Admin::Import`: the uploaded file's rows, under its headers when the first
/// is `#domain`, else each row's first column as `#domain`; refused when
/// missing, malformed or over the processing limit.
fn import_rows(parts: Vec<(String, Part)>) -> AppResult<(String, Vec<CsvRow>)> {
    let file = parts.into_iter().find_map(|(name, part)| match part {
        Part::File {
            data, file_name, ..
        } if name == "data" || name == "admin_import[data]" => Some((data, file_name)),
        _ => None,
    });
    let Some((data, file_name)) = file.filter(|(d, _)| !d.is_empty()) else {
        return Err(AppError::Unprocessable(
            "Validation failed: Data can't be blank".into(),
        ));
    };
    let invalid = |e: csv::Error| {
        AppError::Unprocessable(format!(
            "Validation failed: Data Invalid CSV file. Error: {e}"
        ))
    };
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(data.as_slice());
    let mut records = vec![];
    for record in reader.records() {
        let record = record.map_err(invalid)?;
        // `skip_blanks: true`.
        if record.iter().all(|f| f.is_empty()) {
            continue;
        }
        records.push(record);
        if records.len() > ROWS_PROCESSING_LIMIT + 1 {
            break;
        }
    }
    let headers: Vec<String> = match records.first() {
        Some(first) if first.get(0) == Some("#domain") => {
            let headers = first.iter().map(str::to_owned).collect();
            records.remove(0);
            headers
        }
        _ => vec!["#domain".to_owned()],
    };
    if records.len() > ROWS_PROCESSING_LIMIT {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: Data contains more than {ROWS_PROCESSING_LIMIT} rows"
        )));
    }
    let rows = records
        .iter()
        .map(|record| {
            headers
                .iter()
                .enumerate()
                .map(|(i, h)| (h.clone(), record.get(i).map(str::to_owned)))
                .collect()
        })
        .collect();
    Ok((file_name.unwrap_or_default(), rows))
}

/// `ActiveModel::Type::Boolean.new.cast(field&.downcase)`.
fn cast_boolean(field: Option<&String>) -> Option<bool> {
    let field = field?.trim().to_lowercase();
    if field.is_empty() {
        return None;
    }
    Some(!matches!(field.as_str(), "0" | "f" | "false" | "off"))
}

/// A domain block an import would create, for the moderator to pick from.
#[derive(Debug, Serialize)]
pub struct ImportedDomainBlock {
    pub domain: String,
    pub severity: &'static str,
    pub reject_media: bool,
    pub reject_reports: bool,
    pub private_comment: String,
    pub public_comment: Option<String>,
    pub obfuscate: bool,
}

#[derive(Debug, Serialize)]
pub struct DomainBlockImport {
    pub domain_blocks: Vec<ImportedDomainBlock>,
    /// `instances_from_imported_blocks`: the domains among them that local
    /// accounts follow or are followed from.
    pub warning_domains: Vec<String>,
    /// Why rows were left out, as Mastodon's flash says.
    pub errors: Vec<String>,
}

/// `POST /api/v1/admin/export_domain_blocks/import`: read the file into the
/// blocks it would create, leaving out domains already covered by a block.
/// Nothing is saved: the moderator creates the ones they keep, as Mastodon's
/// confirmation form does.
pub async fn import_domain_blocks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Parts(parts): Parts,
) -> AppResult<Json<DomainBlockImport>> {
    auth.require_scope("admin:write")?;
    super::require_permission(&state, auth.account_id, flag::MANAGE_FEDERATION).await?;
    let (file_name, rows) = import_rows(parts)?;
    // `admin.export_domain_blocks.import.private_comment_template`, dated as
    // `I18n.l` formats a time.
    let private_comment = format!(
        "Imported from {file_name} on {}",
        chrono::Utc::now().format("%b %d, %Y, %H:%M")
    );
    let mut blocks: Vec<ImportedDomainBlock> = vec![];
    let mut errors = vec![];
    for row in rows {
        let Some(domain) = row
            .get("#domain")
            .cloned()
            .flatten()
            .map(|d| d.trim().to_lowercase())
        else {
            continue;
        };
        if crate::federation::moderation::lookup(&state, &domain)
            .await
            .is_some()
        {
            continue;
        }
        let severity_text = row
            .get("#severity")
            .cloned()
            .flatten()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "suspend".into());
        let Some(severity) = domain_severity::parse(&severity_text) else {
            errors.push(format!(
                "One or more domain blocks were skipped because of the following error(s): '{severity_text}' is not a valid severity"
            ));
            continue;
        };
        let Some(normalized) =
            normalize_domain(&domain).filter(|d| d.contains('.') || d == "localhost")
        else {
            errors.push(
                "One or more domain blocks were skipped because of the following error(s): Domain is not a valid domain name"
                    .into(),
            );
            continue;
        };
        if blocks.iter().any(|b| b.domain == normalized) {
            continue;
        }
        blocks.push(ImportedDomainBlock {
            domain: normalized,
            severity: domain_severity::to_str(Some(severity)),
            reject_media: cast_boolean(row.get("#reject_media").and_then(Option::as_ref))
                .unwrap_or(false),
            reject_reports: cast_boolean(row.get("#reject_reports").and_then(Option::as_ref))
                .unwrap_or(false),
            private_comment: private_comment.clone(),
            public_comment: row
                .get("#public_comment")
                .cloned()
                .flatten()
                .map(|c| c.trim().to_owned()),
            obfuscate: cast_boolean(row.get("#obfuscate").and_then(Option::as_ref))
                .unwrap_or(false),
        });
    }
    let domains: Vec<String> = blocks.iter().map(|b| b.domain.clone()).collect();
    // `Instance.with_domain_follows`.
    let warning_domains = sqlx::query_scalar!(
        r#"SELECT DISTINCT a.domain AS "domain!" FROM follows f
           JOIN accounts a ON a.id = f.account_id OR a.id = f.target_account_id
           WHERE a.domain = ANY($1)
           ORDER BY 1"#,
        &domains,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(DomainBlockImport {
        domain_blocks: blocks,
        warning_domains,
        errors,
    }))
}

/// `POST /api/v1/admin/export_domain_allows/import`: allow each domain not
/// already allowed, logging each; answered with the allows made.
pub async fn import_domain_allows(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Parts(parts): Parts,
) -> AppResult<(StatusCode, Json<Vec<String>>)> {
    auth.require_scope("admin:write")?;
    super::require_permission(&state, auth.account_id, flag::MANAGE_FEDERATION).await?;
    let (_, rows) = import_rows(parts)?;
    let mut created = vec![];
    for row in rows {
        let Some(domain) = row
            .get("#domain")
            .cloned()
            .flatten()
            .and_then(|d| normalize_domain(&d))
        else {
            continue;
        };
        let id = sqlx::query_scalar!(
            r#"INSERT INTO domain_allows (domain, created_at, updated_at) VALUES ($1, now(), now())
               ON CONFLICT (domain) DO NOTHING RETURNING id"#,
            domain,
        )
        .fetch_optional(&state.db)
        .await?;
        if let Some(id) = id {
            action_log::log(
                &state.db,
                auth.account_id,
                "create",
                &Target::domain_allow(id, &domain),
            )
            .await?;
            created.push(domain);
        }
    }
    Ok((StatusCode::OK, Json(created)))
}
