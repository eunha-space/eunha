//! `Admin::Metrics`: the dashboard's measures, dimensions and retention
//! cohorts, at `POST /api/v1/admin/measures`, `/dimensions` and `/retention`.
//!
//! Each measure and dimension is the query Mastodon's class runs, read the
//! way Mastodon reads its parameters. Mastodon keeps each answer in
//! `Rails.cache` for five minutes; eunha computes it every time.

use std::collections::HashMap;

use axum::{extract::Extension, http::HeaderMap, Json};
use chrono::{NaiveDate, NaiveDateTime};
use serde::Deserialize;
use serde_json::{json, Value};

use super::super::extractors::{Params, RubyInt};
use super::{instances, perm, require_permission};
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

// ── The time range ────────────────────────────────────────────────────────

/// The range a measure or dimension covers: `start_at` and `end_at`
/// (`to_datetime`), the start no more than two years before the end, as
/// `BaseMeasure` and `BaseDimension` clamp it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Window {
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
}

impl Window {
    /// Mastodon cannot answer without both ends; eunha takes the last week
    /// up to now for whichever is missing.
    fn parse(start_at: Option<&str>, end_at: Option<&str>) -> Self {
        let now = chrono::Utc::now().naive_utc();
        let start = start_at
            .and_then(parse_admin_date)
            .unwrap_or_else(|| now - chrono::Duration::days(7));
        let end = end_at.and_then(parse_admin_date).unwrap_or(now);
        Self {
            start: start.max(end - chrono::Months::new(24)),
            end,
        }
    }

    /// `time_period`: the dates from the start's to the end's.
    pub fn first(&self) -> NaiveDate {
        self.start.date()
    }

    pub fn last(&self) -> NaiveDate {
        self.end.date()
    }

    /// `length_of_period`, in days.
    fn length(&self) -> i64 {
        (self.last() - self.first()).num_days()
    }

    /// `previous_time_period`: the dates shifted back by
    /// `length_of_period + 1` days.
    fn previous(&self) -> (NaiveDate, NaiveDate) {
        let shift = chrono::Duration::days(self.length() + 1);
        (self.first() - shift, self.last() - shift)
    }

    /// Every date of `time_period`.
    fn dates(&self) -> Vec<NaiveDate> {
        self.first()
            .iter_days()
            .take_while(|d| *d <= self.last())
            .collect()
    }

    /// `earliest_status_id`: the snowflake id of the start's day.
    pub fn earliest_status_id(&self) -> i64 {
        snowflake_id(self.first().and_time(chrono::NaiveTime::MIN))
    }

    /// `latest_status_id`: the snowflake id of the end of the end's day.
    pub fn latest_status_id(&self) -> i64 {
        snowflake_id(self.last().and_hms_opt(23, 59, 59).unwrap_or(self.end))
    }
}

/// `Mastodon::Snowflake.id_at(datetime, with_random: false)`.
fn snowflake_id(at: NaiveDateTime) -> i64 {
    crate::snowflake::id_at(at.and_utc())
}

/// `String#to_datetime`: an ISO 8601 time, or a bare date at midnight UTC.
pub(super) fn parse_admin_date(s: &str) -> Option<NaiveDateTime> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&chrono::Utc).naive_utc());
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt);
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .map(|d| d.and_time(chrono::NaiveTime::MIN))
}

/// `params.require(key)`: the key's own parameters, which must be given.
pub(super) fn required<'a>(
    params: &'a HashMap<String, Value>,
    key: &str,
) -> AppResult<&'a serde_json::Map<String, Value>> {
    params
        .get(key)
        .and_then(Value::as_object)
        .filter(|object| !object.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest(format!("param is missing or the value is empty: {key}"))
        })
}

/// A parameter as Rails gives it: a string, whatever it was sent as.
pub(super) fn string_param(object: &serde_json::Map<String, Value>, name: &str) -> Option<String> {
    match object.get(name)? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

// ── The rendering ─────────────────────────────────────────────────────────

/// `REST::Admin::MeasureSerializer`: `human_value` only for a measure that
/// defines `value_to_human_value`, and `previous_total` only for one whose
/// total is of its time range.
pub(super) fn measure_json(
    key: &str,
    unit: Option<&str>,
    total: i64,
    human_value: Option<String>,
    previous_total: Option<i64>,
    data: Vec<Value>,
) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("key".into(), json!(key));
    out.insert("unit".into(), json!(unit));
    out.insert("total".into(), json!(total.to_string()));
    if let Some(human_value) = human_value {
        out.insert("human_value".into(), json!(human_value));
    }
    if let Some(previous_total) = previous_total {
        out.insert("previous_total".into(), json!(previous_total.to_string()));
    }
    out.insert("data".into(), Value::Array(data));
    Value::Object(out)
}

/// A day of a measure that a query counted (`QueryHelper`): the date as
/// Rails decodes a `date` column, and the value.
pub(super) fn sql_point(date: NaiveDate, value: i64) -> Value {
    json!({ "date": date.format("%Y-%m-%d").to_string(), "value": value.to_string() })
}

/// A day of a measure counted in Redis: `date.to_time(:utc).iso8601`.
fn redis_point(date: NaiveDate, value: i64) -> Value {
    json!({ "date": date.format("%Y-%m-%dT00:00:00Z").to_string(), "value": value.to_string() })
}

/// A parameter of a per-day query.
pub(super) enum Arg {
    Int(i64),
    Text(Option<String>),
}

/// `QueryHelper#generated_series_days`, with `per_day` counted for each:
/// `$1` and `$2` are the start and the end, and `args` are `$3` onwards.
pub(super) async fn per_day(
    state: &AppState,
    window: &Window,
    per_day: &str,
    args: Vec<Arg>,
) -> AppResult<Vec<Value>> {
    let sql = format!(
        "SELECT axis.period, ({per_day})::bigint AS value
         FROM (SELECT generate_series($1::timestamp, $2::timestamp, '1 day')::date AS period) AS axis
         ORDER BY axis.period"
    );
    let mut query = sqlx::query_as::<_, (NaiveDate, i64)>(&sql)
        .bind(window.start)
        .bind(window.end);
    for arg in args {
        query = match arg {
            Arg::Int(n) => query.bind(n),
            Arg::Text(t) => query.bind(t),
        };
    }
    let rows = query.fetch_all(&state.db).await?;
    Ok(rows
        .into_iter()
        .map(|(date, value)| sql_point(date, value))
        .collect())
}

/// `ActionView::Helpers::NumberHelper#number_to_human_size`, in English:
/// bytes below a kilobyte, else three significant digits of the largest
/// unit, insignificant zeros stripped.
pub(super) fn number_to_human_size(bytes: i64) -> String {
    const UNITS: [&str; 8] = ["Bytes", "KB", "MB", "GB", "TB", "PB", "EB", "ZB"];
    if bytes < 1024 {
        return format!("{bytes} {}", if bytes == 1 { "Byte" } else { "Bytes" });
    }
    let number = bytes as f64;
    let exponent = ((number.ln() / 1024f64.ln()) as usize).min(UNITS.len() - 1);
    let human = number / 1024f64.powi(exponent as i32);
    format!("{} {}", round_significant(human, 3), UNITS[exponent])
}

/// `NumberToRoundedConverter` with `significant: true` and
/// `strip_insignificant_zeros: true`: `number` to `precision` significant
/// digits, rounding half up.
fn round_significant(number: f64, precision: i32) -> String {
    let digits = |n: f64| (n.abs().log10() + 1.0).floor() as i32;
    let places = precision - digits(number);
    let scale = 10f64.powi(places);
    // Rounded from the shortest decimal that reads back as `number`, as
    // `BigDecimal(number.to_s)` does, so that 2.675 rounds up.
    let decimal: f64 = format!("{number}").parse().unwrap_or(number);
    let rounded = ((decimal * scale) + 0.5 + 1e-9).floor() / scale;
    let places = (precision - digits(rounded)).max(0) as usize;
    let text = format!("{rounded:.places$}");
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    }
}

// ── POST /api/v1/admin/measures ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct MeasuresRequest {
    #[serde(default, deserialize_with = "super::super::extractors::rails::strings")]
    pub keys: Vec<String>,
    pub start_at: Option<String>,
    pub end_at: Option<String>,
    /// Each key's own parameters, such as `tag_accounts[id]`.
    #[serde(flatten)]
    pub params: HashMap<String, Value>,
}

/// `Api::V1::Admin::MeasuresController#create`: each of `keys` that
/// `Admin::Metrics::Measure::MEASURES` has, the others dropped.
pub async fn get_measures(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(body): Params<MeasuresRequest>,
) -> AppResult<Json<Vec<Value>>> {
    require_permission(&state, auth.account_id, perm::VIEW_DASHBOARD).await?;
    let window = Window::parse(body.start_at.as_deref(), body.end_at.as_deref());

    let mut result = Vec::new();
    for key in &body.keys {
        let measure = match key.as_str() {
            "active_users" => {
                activity_measure(
                    &state,
                    key,
                    crate::activity_tracker::LOGINS,
                    crate::activity_tracker::Kind::Unique,
                    &window,
                )
                .await?
            }
            "interactions" => {
                activity_measure(
                    &state,
                    key,
                    crate::activity_tracker::INTERACTIONS,
                    crate::activity_tracker::Kind::Basic,
                    &window,
                )
                .await?
            }
            "new_users" => {
                // Signed up in the range, and each day's new users by the
                // snowflake ids of their accounts.
                counted_measure(
                    &state,
                    key,
                    &window,
                    "SELECT count(*) FROM users WHERE created_at BETWEEN $1 AND $2",
                    "SELECT count(*) FROM users
                     WHERE users.account_id >= (date_part('epoch', date_trunc('day', axis.period)::date) * 1000)::bigint << 16
                       AND users.account_id < ((date_part('epoch', date_trunc('day', axis.period)::date + ('1 day')::interval)) * 1000)::bigint << 16",
                )
                .await?
            }
            "opened_reports" => {
                counted_measure(
                    &state,
                    key,
                    &window,
                    "SELECT count(*) FROM reports WHERE created_at BETWEEN $1 AND $2",
                    "SELECT count(*) FROM reports
                     WHERE date_trunc('day', reports.created_at)::date = axis.period",
                )
                .await?
            }
            "resolved_reports" => {
                counted_measure(
                    &state,
                    key,
                    &window,
                    "SELECT count(*) FROM reports
                     WHERE action_taken_at IS NOT NULL AND action_taken_at BETWEEN $1 AND $2",
                    "SELECT count(*) FROM reports
                     WHERE date_trunc('day', reports.action_taken_at)::date = axis.period",
                )
                .await?
            }
            "tag_accounts" | "tag_uses" => {
                let tag_id = find_tag(&state, required(&body.params, key)?).await?;
                tag_history_measure(&state, key, tag_id, &window).await?
            }
            "tag_servers" => {
                let tag_id = find_tag(&state, required(&body.params, key)?).await?;
                tag_servers_measure(&state, key, tag_id, &window).await?
            }
            other if instances::MEASURES.contains(&other) => {
                instances::measure(&state, other, required(&body.params, key)?, &window).await?
            }
            _ => continue,
        };
        result.push(measure);
    }
    Ok(Json(result))
}

/// A measure `ActivityTracker` keeps (`ActiveUsersMeasure`,
/// `InteractionsMeasure`): the total over the range, the total over the
/// period before it, and each day's count; neither defines
/// `value_to_human_value`.
async fn activity_measure(
    state: &AppState,
    key: &str,
    prefix: &str,
    kind: crate::activity_tracker::Kind,
    window: &Window,
) -> AppResult<Value> {
    use crate::activity_tracker::{get, sum};

    let (previous_first, previous_last) = window.previous();
    let redis_error = |e: redis::RedisError| AppError::Internal(e.into());
    let total = sum(state, prefix, kind, window.first(), window.last())
        .await
        .map_err(redis_error)?;
    let previous_total = sum(state, prefix, kind, previous_first, previous_last)
        .await
        .map_err(redis_error)?;
    let data = get(state, prefix, kind, window.first(), window.last())
        .await
        .map_err(redis_error)?;
    Ok(measure_json(
        key,
        None,
        total,
        None,
        Some(previous_total),
        data.iter()
            .map(|(date, value)| redis_point(*date, *value))
            .collect(),
    ))
}

/// A measure a query counts (`NewUsersMeasure`, `OpenedReportsMeasure`,
/// `ResolvedReportsMeasure`): `total_sql` over the dates of the range and of
/// the period before it, bound as dates (`where(created_at: time_period)`,
/// which reaches midnight of the last day and no further), and `per_day`
/// for each day.
async fn counted_measure(
    state: &AppState,
    key: &str,
    window: &Window,
    total_sql: &'static str,
    per_day_sql: &str,
) -> AppResult<Value> {
    let total = |first: NaiveDate, last: NaiveDate| async move {
        sqlx::query_scalar::<_, i64>(total_sql)
            .bind(first.and_time(chrono::NaiveTime::MIN))
            .bind(last.and_time(chrono::NaiveTime::MIN))
            .fetch_one(&state.db)
            .await
    };
    let (previous_first, previous_last) = window.previous();
    let current = total(window.first(), window.last()).await?;
    let previous = total(previous_first, previous_last).await?;
    let data = per_day(state, window, per_day_sql, vec![]).await?;
    Ok(measure_json(key, None, current, None, Some(previous), data))
}

/// `Tag.find(params[:id])`.
async fn find_tag(state: &AppState, params: &serde_json::Map<String, Value>) -> AppResult<i64> {
    let id = string_param(params, "id")
        .map(|id| crate::search::ruby_to_i(&id))
        .unwrap_or_default();
    sqlx::query_scalar::<_, i64>("SELECT id FROM tags WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

/// `TagAccountsMeasure` and `TagUsesMeasure`: the tag's `Trends::History`,
/// the distinct accounts or the uses over the range's days, over the
/// period's before, and each day's.
async fn tag_history_measure(
    state: &AppState,
    key: &str,
    tag_id: i64,
    window: &Window,
) -> AppResult<Value> {
    use crate::moderation::history;

    let accounts = key == "tag_accounts";
    let aggregate = |dates: Vec<NaiveDate>| async move {
        if accounts {
            history::accounts_on(state, "tags", tag_id, &dates).await
        } else {
            history::uses_on(state, "tags", tag_id, &dates).await
        }
    };
    let (previous_first, previous_last) = window.previous();
    let previous_dates: Vec<NaiveDate> = previous_first
        .iter_days()
        .take_while(|d| *d <= previous_last)
        .collect();
    let total = aggregate(window.dates()).await;
    let previous_total = aggregate(previous_dates).await;
    let mut data = Vec::new();
    for date in window.dates() {
        let value = aggregate(vec![date]).await;
        data.push(redis_point(date, value));
    }
    Ok(measure_json(
        key,
        None,
        total,
        None,
        Some(previous_total),
        data,
    ))
}

/// `TagServersMeasure`: the domains that posted with the tag between the
/// snowflake ids of the start and the end (local ones not counted, as
/// `count('distinct accounts.domain')` skips none), the same over the range
/// shifted back by its length, and each day's, local as one.
async fn tag_servers_measure(
    state: &AppState,
    key: &str,
    tag_id: i64,
    window: &Window,
) -> AppResult<Value> {
    let count = |from: NaiveDateTime, to: NaiveDateTime| async move {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(DISTINCT accounts.domain) FROM statuses
             INNER JOIN statuses_tags ON statuses_tags.status_id = statuses.id
             INNER JOIN accounts ON accounts.id = statuses.account_id
             WHERE statuses_tags.tag_id = $1 AND statuses.deleted_at IS NULL
               AND statuses.id BETWEEN $2 AND $3",
        )
        .bind(tag_id)
        .bind(snowflake_id(from))
        .bind(snowflake_id(to))
        .fetch_one(&state.db)
        .await
    };
    let length = chrono::Duration::days(window.length());
    let total = count(window.start, window.end).await?;
    let previous_total = count(window.start - length, window.end - length).await?;
    let data = per_day(
        state,
        window,
        "WITH tag_servers AS (
           SELECT DISTINCT accounts.domain FROM statuses
           INNER JOIN statuses_tags ON statuses.id = statuses_tags.status_id
           INNER JOIN accounts ON statuses.account_id = accounts.id
           WHERE statuses_tags.tag_id = $3
             AND statuses.id BETWEEN $4 AND $5
             AND date_trunc('day', statuses.created_at)::date = axis.period
         )
         SELECT COUNT(*) FROM tag_servers",
        vec![
            Arg::Int(tag_id),
            Arg::Int(window.earliest_status_id()),
            Arg::Int(window.latest_status_id()),
        ],
    )
    .await?;
    Ok(measure_json(
        key,
        None,
        total,
        None,
        Some(previous_total),
        data,
    ))
}

// ── POST /api/v1/admin/dimensions ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DimensionsRequest {
    #[serde(default, deserialize_with = "super::super::extractors::rails::strings")]
    pub keys: Vec<String>,
    pub start_at: Option<String>,
    pub end_at: Option<String>,
    pub limit: Option<RubyInt>,
    /// Each key's own parameters, such as `tag_servers[id]`.
    #[serde(flatten)]
    pub params: HashMap<String, Value>,
}

/// `Api::V1::Admin::DimensionsController#create`: each of `keys` that
/// `Admin::Metrics::Dimension::DIMENSIONS` has, the others dropped. The
/// `limit` is `limit&.to_i`: none given, no limit at all.
pub async fn get_dimensions(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    headers: HeaderMap,
    Params(body): Params<DimensionsRequest>,
) -> AppResult<Json<Vec<Value>>> {
    require_permission(&state, auth.account_id, perm::VIEW_DASHBOARD).await?;
    let window = Window::parse(body.start_at.as_deref(), body.end_at.as_deref());
    let limit = body.limit.map(|RubyInt(n)| n);
    let locale = {
        let lang = body.params.get("lang").and_then(Value::as_str);
        let locale =
            super::super::translations::requested_locale(&state, &auth, &headers, lang).await?;
        if locale == "ko" {
            crate::locale::Locale::Ko
        } else {
            crate::locale::Locale::En
        }
    };

    let mut result = Vec::new();
    for key in &body.keys {
        let data = match key.as_str() {
            "languages" => languages_dimension(&state, &window, limit, locale).await?,
            "sources" => sources_dimension(&state, &window, limit, locale).await?,
            "servers" => servers_dimension(&state, &window, limit, None).await?,
            "space_usage" => super::space_usage(&state, locale).await?,
            "software_versions" => super::software_versions(&state).await?,
            "tag_servers" => {
                let tag_id = tag_id(required(&body.params, key)?);
                servers_dimension(&state, &window, limit, Some(tag_id)).await?
            }
            "tag_languages" => {
                let tag_id = tag_id(required(&body.params, key)?);
                tag_languages_dimension(&state, &window, limit, tag_id, locale).await?
            }
            other if instances::DIMENSIONS.contains(&other) => {
                instances::dimension(
                    &state,
                    other,
                    required(&body.params, key)?,
                    &window,
                    limit,
                    locale,
                )
                .await?
            }
            _ => continue,
        };
        result.push(json!({ "key": key, "data": data }));
    }
    Ok(Json(result))
}

/// `params[:id]` of a tag dimension, which is not looked up.
fn tag_id(params: &serde_json::Map<String, Value>) -> i64 {
    string_param(params, "id")
        .map(|id| crate::search::ruby_to_i(&id))
        .unwrap_or_default()
}

/// `LanguagesHelper#standard_locale_name`, `generic.none` in the request's
/// locale for a blank one.
pub(super) fn standard_locale_name(locale: &str, ui: crate::locale::Locale) -> String {
    if locale.trim().is_empty() {
        ui.t("generic.none").to_owned()
    } else {
        crate::languages::standard_locale_name(locale)
    }
}

/// `{ key:, human_key:, value: }`, as each counting dimension gives a row.
pub(super) fn dimension_row(key: &str, human_key: &str, value: i64) -> Value {
    json!({ "key": key, "human_key": human_key, "value": value.to_string() })
}

/// `LanguagesDimension`: the locales of the users who signed in during the
/// range.
async fn languages_dimension(
    state: &AppState,
    window: &Window,
    limit: Option<i64>,
    locale: crate::locale::Locale,
) -> AppResult<Vec<Value>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT locale, count(*) AS value FROM users
         WHERE current_sign_in_at BETWEEN $1 AND $2 AND locale IS NOT NULL
         GROUP BY locale ORDER BY count(*) DESC LIMIT $3",
    )
    .bind(window.start)
    .bind(window.end)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .iter()
        .map(|(key, value)| dimension_row(key, &standard_locale_name(key, locale), *value))
        .collect())
}

/// `SourcesDimension`: the applications the users who signed up during the
/// range signed up through, the website (`admin.dashboard.website`) for
/// those who used none.
async fn sources_dimension(
    state: &AppState,
    window: &Window,
    limit: Option<i64>,
    locale: crate::locale::Locale,
) -> AppResult<Vec<Value>> {
    let rows: Vec<(Option<String>, i64)> = sqlx::query_as(
        "SELECT oauth_applications.name, count(*) AS value FROM users
         LEFT JOIN oauth_applications ON oauth_applications.id = users.created_by_application_id
         WHERE users.created_at BETWEEN $1 AND $2
         GROUP BY oauth_applications.name ORDER BY count(*) DESC LIMIT $3",
    )
    .bind(window.start)
    .bind(window.end)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .iter()
        .map(|(name, value)| match name {
            Some(name) => dimension_row(name, name, *value),
            None => dimension_row("web", locale.t("admin.dashboard.website"), *value),
        })
        .collect())
}

/// `ServersDimension`, and `TagServersDimension` for a tag: the domains of
/// the posts between the snowflake ids of the start's day and the end's,
/// this one's under the local domain.
async fn servers_dimension(
    state: &AppState,
    window: &Window,
    limit: Option<i64>,
    tag_id: Option<i64>,
) -> AppResult<Vec<Value>> {
    let rows: Vec<(Option<String>, i64)> = sqlx::query_as(
        "SELECT accounts.domain, count(*) AS value FROM statuses
         INNER JOIN accounts ON accounts.id = statuses.account_id
         WHERE ($4::bigint IS NULL OR EXISTS (
                 SELECT 1 FROM statuses_tags
                 WHERE statuses_tags.status_id = statuses.id AND statuses_tags.tag_id = $4))
           AND statuses.id BETWEEN $1 AND $2
         GROUP BY accounts.domain ORDER BY count(*) DESC LIMIT $3",
    )
    .bind(window.earliest_status_id())
    .bind(window.latest_status_id())
    .bind(limit)
    .bind(tag_id)
    .fetch_all(&state.db)
    .await?;
    let local = &state.instance.domain;
    Ok(rows
        .iter()
        .map(|(domain, value)| {
            let domain = domain.as_deref().unwrap_or(local);
            dimension_row(domain, domain, *value)
        })
        .collect())
}

/// `TagLanguagesDimension`: the languages of the posts with the tag between
/// the snowflake ids of the start's day and the end's.
async fn tag_languages_dimension(
    state: &AppState,
    window: &Window,
    limit: Option<i64>,
    tag_id: i64,
    locale: crate::locale::Locale,
) -> AppResult<Vec<Value>> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT COALESCE(statuses.language, 'und') AS language, count(*) AS value
         FROM statuses
         INNER JOIN statuses_tags ON statuses_tags.status_id = statuses.id
         WHERE statuses_tags.tag_id = $1 AND statuses.id BETWEEN $2 AND $3
         GROUP BY COALESCE(statuses.language, 'und') ORDER BY count(*) DESC LIMIT $4",
    )
    .bind(tag_id)
    .bind(window.earliest_status_id())
    .bind(window.latest_status_id())
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .iter()
        .map(|(key, value)| dimension_row(key, &standard_locale_name(key, locale), *value))
        .collect())
}

// ── POST /api/v1/admin/retention ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RetentionRequest {
    pub start_at: Option<String>,
    pub end_at: Option<String>,
    pub frequency: Option<String>,
}

/// `Api::V1::Admin::RetentionController#create`: `Admin::Metrics::Retention`
/// of `start_at` to `end_at` (both required, read as dates), by day or by
/// month, the start no more than 31 days or 12 months back. A cohort is the
/// users whose accounts' snowflake ids fall in its period; a later period
/// retains those who signed in since it began.
pub async fn get_retention(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(body): Params<RetentionRequest>,
) -> AppResult<Json<Vec<Value>>> {
    require_permission(&state, auth.account_id, perm::VIEW_DASHBOARD).await?;

    let date = |value: Option<&str>, name: &str| {
        value
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| {
                AppError::BadRequest(format!("param is missing or the value is empty: {name}"))
            })
            .and_then(|v| {
                parse_admin_date(v)
                    .map(|d| d.date())
                    .ok_or_else(|| AppError::BadRequest(format!("invalid date: {name}")))
            })
    };
    let start = date(body.start_at.as_deref(), "start_at")?;
    let end = date(body.end_at.as_deref(), "end_at")?;
    let frequency = match body.frequency.as_deref() {
        Some("month") => "month",
        _ => "day",
    };
    let earliest = if frequency == "day" {
        end - chrono::Duration::days(31)
    } else {
        end - chrono::Months::new(12)
    };
    let start = start.max(earliest);

    let rows: Vec<(chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, String)> =
        sqlx::query_as(
            r#"SELECT axis.cohort_period, axis.retention_period, (
                 WITH new_users AS (
                   SELECT users.id FROM users
                   WHERE users.account_id >= (date_part('epoch', date_trunc($3, axis.cohort_period)::date) * 1000)::bigint << 16
                     AND users.account_id < ((date_part('epoch', date_trunc($3, axis.cohort_period)::date + ('1' || $3)::interval)) * 1000)::bigint << 16
                 ),
                 retained_users AS (
                   SELECT users.id FROM users
                   INNER JOIN new_users ON new_users.id = users.id
                   WHERE date_trunc($3, users.current_sign_in_at) >= axis.retention_period
                 )
                 SELECT ARRAY[count(*), (count(*))::float / (SELECT GREATEST(count(*), 1) FROM new_users)]::text
                 FROM retained_users
               )
               FROM (
                 WITH cohort_periods AS (
                   SELECT generate_series(date_trunc($3, $1::timestamp)::date,
                                          date_trunc($3, $2::timestamp)::date,
                                          ('1 ' || $3)::interval) AS cohort_period
                 ),
                 retention_periods AS (
                   SELECT cohort_period AS retention_period FROM cohort_periods
                 )
                 SELECT * FROM cohort_periods, retention_periods
                 WHERE retention_period >= cohort_period
               ) AS axis"#,
        )
        .bind(start.and_time(chrono::NaiveTime::MIN))
        .bind(end.and_time(chrono::NaiveTime::MIN))
        .bind(frequency)
        .fetch_all(&state.db)
        .await?;

    // The periods are `timestamptz`, which Rails reads as times at their
    // offset, so `iso8601` gives `+00:00`.
    let iso8601 =
        |t: &chrono::DateTime<chrono::Utc>| t.format("%Y-%m-%dT%H:%M:%S+00:00").to_string();
    let mut cohorts: Vec<(chrono::DateTime<chrono::Utc>, Vec<Value>)> = Vec::new();
    for (cohort_period, retention_period, value_and_rate) in rows {
        // `row['retention_value_and_rate'].delete('{}').split(',')`.
        let cleaned = value_and_rate.replace(['{', '}'], "");
        let mut parts = cleaned.split(',');
        let value = parts.next().unwrap_or_default().to_owned();
        let rate: f64 = parts.next().and_then(|r| r.parse().ok()).unwrap_or(0.0);
        let entry = json!({
            "date": iso8601(&retention_period),
            "rate": rate,
            "value": value,
        });
        match cohorts.last_mut() {
            Some((period, data)) if *period == cohort_period => data.push(entry),
            _ => cohorts.push((cohort_period, vec![entry])),
        }
    }
    Ok(Json(
        cohorts
            .into_iter()
            .map(|(period, data)| {
                json!({
                    "period": iso8601(&period),
                    "frequency": frequency,
                    "data": data,
                })
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_sizes_read_as_rails_writes_them() {
        assert_eq!(number_to_human_size(0), "0 Bytes");
        assert_eq!(number_to_human_size(1), "1 Byte");
        assert_eq!(number_to_human_size(1023), "1023 Bytes");
        assert_eq!(number_to_human_size(1024), "1 KB");
        assert_eq!(number_to_human_size(1536), "1.5 KB");
        assert_eq!(number_to_human_size(1_234_567), "1.18 MB");
        assert_eq!(number_to_human_size(123_456_789), "118 MB");
        assert_eq!(number_to_human_size(1_048_575), "1020 KB");
        assert_eq!(number_to_human_size(5 * 1024 * 1024 * 1024), "5 GB");
    }

    #[test]
    fn the_previous_period_is_shifted_by_its_length_and_a_day() {
        let window = Window::parse(Some("2026-04-01"), Some("2026-04-07"));
        assert_eq!(
            window.previous(),
            (
                NaiveDate::from_ymd_opt(2026, 3, 25).unwrap(),
                NaiveDate::from_ymd_opt(2026, 3, 31).unwrap()
            )
        );
        assert_eq!(window.dates().len(), 7);
    }

    #[test]
    fn the_start_is_at_most_two_years_back() {
        let window = Window::parse(Some("2020-01-01"), Some("2026-04-07"));
        assert_eq!(window.first(), NaiveDate::from_ymd_opt(2024, 4, 7).unwrap());
    }
}
