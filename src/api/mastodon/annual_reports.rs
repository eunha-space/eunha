use axum::{
    extract::{Extension, Path},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::{Datelike, TimeZone, Utc};
use serde::Serialize;

use super::{
    accounts::{apply_account_stats, batch_account_emojis, batch_account_roles},
    convert::{account_from_db, status_from_db},
    status_serialize::{
        batch_quote_data, batch_reblog_data, batch_status_cards, batch_status_emojis,
        batch_status_media, batch_status_mentions, batch_status_polls, batch_statuses_tags,
        hydrate_status_stats,
    },
    types::{Account as ApiAccount, Status as ApiStatus},
};
use crate::{
    async_refresh::AsyncRefresh,
    db::models::{Account as DbAccount, Status as DbStatus},
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

const SCHEMA_VERSION: i32 = 1;
const AVERAGE_POSTS_PER_YEAR: i64 = 113;

// ── Response types ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AnnualReport {
    pub year: i32,
    pub data: Option<serde_json::Value>,
    pub schema_version: i32,
    pub share_url: Option<String>,
    pub account_id: String,
}

#[derive(Debug, Serialize)]
pub struct AnnualReportsResponse {
    pub annual_reports: Vec<AnnualReport>,
    pub accounts: Vec<ApiAccount>,
    pub statuses: Vec<ApiStatus>,
}

// ── Data generation ────────────────────────────────────────────────────────

async fn generate_report_data(
    state: &AppState,
    account_id: i64,
    year: i32,
) -> AppResult<serde_json::Value> {
    let start = Utc
        .with_ymd_and_hms(year, 1, 1, 0, 0, 0)
        .unwrap()
        .naive_utc();
    let end = Utc
        .with_ymd_and_hms(year + 1, 1, 1, 0, 0, 0)
        .unwrap()
        .naive_utc();

    // Count different post types for archetype
    let reblog_count = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM statuses
         WHERE account_id = $1 AND deleted_at IS NULL
           AND reblog_of_id IS NOT NULL
           AND created_at >= $2 AND created_at < $3",
        account_id,
        start,
        end,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    let reply_count = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM statuses
         WHERE account_id = $1 AND deleted_at IS NULL
           AND in_reply_to_id IS NOT NULL
           AND in_reply_to_account_id != $1
           AND reblog_of_id IS NULL
           AND created_at >= $2 AND created_at < $3",
        account_id,
        start,
        end,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    let standalone_count = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM statuses
         WHERE account_id = $1 AND deleted_at IS NULL
           AND reblog_of_id IS NULL
           AND (in_reply_to_id IS NULL OR in_reply_to_account_id = $1)
           AND created_at >= $2 AND created_at < $3",
        account_id,
        start,
        end,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    let poll_count = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM statuses s
         JOIN polls p ON p.status_id = s.id
         WHERE s.account_id = $1 AND s.deleted_at IS NULL
           AND s.created_at >= $2 AND s.created_at < $3",
        account_id,
        start,
        end,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    let total = reblog_count + reply_count + standalone_count;

    let archetype = if total < AVERAGE_POSTS_PER_YEAR {
        "lurker"
    } else if reblog_count > standalone_count * 2 {
        "booster"
    } else if poll_count > standalone_count / 10 {
        "pollster"
    } else if reply_count > standalone_count * 2 {
        "replier"
    } else {
        "oracle"
    };

    // Top statuses by reblogs, favourites, replies (public/unlisted originals only)
    let top_by_reblogs: Option<i64> = sqlx::query_scalar!(
        r#"SELECT s.id FROM statuses s
           LEFT JOIN status_stats ss ON ss.status_id = s.id
           WHERE s.account_id = $1 AND s.deleted_at IS NULL
             AND s.reblog_of_id IS NULL
             AND s.visibility IN (0, 1) /* vis::PUBLIC, vis::UNLISTED */
             AND s.created_at >= $2 AND s.created_at < $3
           ORDER BY COALESCE(ss.reblogs_count, 0) DESC LIMIT 1"#,
        account_id,
        start,
        end,
    )
    .fetch_optional(&state.db)
    .await?;

    let top_by_favourites: Option<i64> = sqlx::query_scalar!(
        r#"SELECT s.id FROM statuses s
           LEFT JOIN status_stats ss ON ss.status_id = s.id
           WHERE s.account_id = $1 AND s.deleted_at IS NULL
             AND s.reblog_of_id IS NULL
             AND s.visibility IN (0, 1) /* vis::PUBLIC, vis::UNLISTED */
             AND ($4::bigint IS NULL OR s.id != $4)
             AND s.created_at >= $2 AND s.created_at < $3
           ORDER BY COALESCE(ss.favourites_count, 0) DESC LIMIT 1"#,
        account_id,
        start,
        end,
        top_by_reblogs,
    )
    .fetch_optional(&state.db)
    .await?;

    let top_by_replies: Option<i64> = sqlx::query_scalar!(
        r#"SELECT s.id FROM statuses s
           LEFT JOIN status_stats ss ON ss.status_id = s.id
           WHERE s.account_id = $1 AND s.deleted_at IS NULL
             AND s.reblog_of_id IS NULL
             AND s.visibility IN (0, 1) /* vis::PUBLIC, vis::UNLISTED */
             AND ($4::bigint IS NULL OR s.id != $4)
             AND ($5::bigint IS NULL OR s.id != $5)
             AND s.created_at >= $2 AND s.created_at < $3
           ORDER BY COALESCE(ss.replies_count, 0) DESC LIMIT 1"#,
        account_id,
        start,
        end,
        top_by_reblogs,
        top_by_favourites,
    )
    .fetch_optional(&state.db)
    .await?;

    // Time series: total statuses and new followers for the year
    let statuses_in_year: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM statuses
         WHERE account_id = $1 AND deleted_at IS NULL
           AND created_at >= $2 AND created_at < $3",
        account_id,
        start,
        end,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    let followers_in_year: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM follows
         WHERE target_account_id = $1
           AND created_at >= $2 AND created_at < $3",
        account_id,
        start,
        end,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    // Top hashtag
    let top_hashtag = sqlx::query!(
        "SELECT t.name, COUNT(*) as count
         FROM statuses_tags st
         JOIN tags t ON t.id = st.tag_id
         JOIN statuses s ON s.id = st.status_id
         WHERE s.account_id = $1 AND s.deleted_at IS NULL
           AND s.created_at >= $2 AND s.created_at < $3
         GROUP BY t.name
         ORDER BY count DESC LIMIT 1",
        account_id,
        start,
        end,
    )
    .fetch_optional(&state.db)
    .await?;

    let top_hashtags: Vec<serde_json::Value> = top_hashtag
        .into_iter()
        .map(|r| serde_json::json!({ "name": r.name, "count": r.count.unwrap_or(0) }))
        .collect();

    Ok(serde_json::json!({
        "archetype": archetype,
        "top_statuses": {
            "by_reblogs": top_by_reblogs.map(|id| id.to_string()),
            "by_favourites": top_by_favourites.map(|id| id.to_string()),
            "by_replies": top_by_replies.map(|id| id.to_string()),
        },
        "time_series": [{
            "month": 12,
            "statuses": statuses_in_year,
            "followers": followers_in_year,
        }],
        "top_hashtags": top_hashtags,
    }))
}

async fn is_eligible(state: &AppState, account_id: i64, year: i32) -> AppResult<bool> {
    let start = Utc
        .with_ymd_and_hms(year, 1, 1, 0, 0, 0)
        .unwrap()
        .naive_utc();
    let end = Utc
        .with_ymd_and_hms(year + 1, 1, 1, 0, 0, 0)
        .unwrap()
        .naive_utc();
    let count: i64 = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM statuses
         WHERE account_id = $1 AND deleted_at IS NULL
           AND created_at >= $2 AND created_at < $3",
        account_id,
        start,
        end,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);
    Ok(count > 0)
}

// ── Build the response: fetch referenced accounts + statuses ───────────────

async fn build_response(
    state: &AppState,
    reports: Vec<AnnualReport>,
    account: &DbAccount,
    viewer_id: i64,
) -> AppResult<AnnualReportsResponse> {
    // Collect status IDs referenced in all reports
    let mut top_status_ids: Vec<i64> = Vec::new();
    for report in &reports {
        if let Some(data) = &report.data {
            let top = &data["top_statuses"];
            for key in ["by_reblogs", "by_favourites", "by_replies"] {
                if let Some(id_str) = top[key].as_str() {
                    if let Ok(id) = id_str.parse::<i64>() {
                        if !top_status_ids.contains(&id) {
                            top_status_ids.push(id);
                        }
                    }
                }
            }
        }
    }

    // Fetch referenced statuses
    let api_statuses = if top_status_ids.is_empty() {
        vec![]
    } else {
        let statuses = sqlx::query_as!(
            DbStatus,
            "SELECT * FROM statuses WHERE id = ANY($1) AND deleted_at IS NULL",
            &top_status_ids,
        )
        .fetch_all(&state.db)
        .await?;

        let all_ids: Vec<i64> = statuses.iter().map(|s| s.id).collect();
        let media_map = batch_status_media(state, &all_ids).await?;
        let reblog_map = batch_reblog_data(state, &statuses).await?;
        let quote_map = batch_quote_data(state, &statuses, Some(viewer_id)).await?;
        let reblog_ids: Vec<i64> = reblog_map.values().map(|(rs, _, _)| rs.id).collect();
        let mut enrich_ids = all_ids.clone();
        enrich_ids.extend_from_slice(&reblog_ids);
        let tags_map = batch_statuses_tags(state, &enrich_ids).await?;
        let mentions_map = batch_status_mentions(state, &enrich_ids).await?;
        let all_for_emoji: Vec<DbStatus> = statuses
            .iter()
            .cloned()
            .chain(reblog_map.values().map(|(rs, _, _)| rs.clone()))
            .collect();
        let emojis_map = batch_status_emojis(state, &all_for_emoji).await?;
        let polls_map = batch_status_polls(state, &enrich_ids, Some(viewer_id)).await?;
        let cards_map = batch_status_cards(state, &enrich_ids, Some(viewer_id)).await?;
        let ctxs = super::statuses::batch_viewer_contexts(state, viewer_id, &all_ids).await?;

        let all_accounts_for_emoji: Vec<DbAccount> = {
            let mut v = vec![account.clone()];
            v.extend(reblog_map.values().map(|(_, ra, _)| ra.clone()));
            v
        };
        let account_emojis_map = batch_account_emojis(state, &all_accounts_for_emoji).await;
        let account_roles_map = batch_account_roles(state, &all_accounts_for_emoji).await;

        let mut result = Vec::with_capacity(statuses.len());
        for s in &statuses {
            let media = media_map.get(&s.id).cloned().unwrap_or_default();
            let reblog = reblog_map.get(&s.id).cloned();
            let ctx = ctxs.get(&s.id).cloned();
            let mentions = mentions_map.get(&s.id).cloned().unwrap_or_default();
            let rb_mentions = reblog
                .as_ref()
                .and_then(|(rs, _, _)| mentions_map.get(&rs.id))
                .cloned()
                .unwrap_or_default();
            let mut api = status_from_db(
                &state.urls,
                s,
                account,
                media,
                reblog,
                ctx,
                &mentions,
                &rb_mentions,
            );
            api.account.emojis = account_emojis_map
                .get(&account.id)
                .cloned()
                .unwrap_or_default();
            api.account.roles = account_roles_map
                .get(&account.id)
                .cloned()
                .unwrap_or_default();
            api.tags = tags_map.get(&s.id).cloned().unwrap_or_default();
            api.mentions = mentions;
            api.emojis = emojis_map.get(&s.id).cloned().unwrap_or_default();
            api.poll = polls_map.get(&s.id).cloned();
            api.card = cards_map.get(&s.id).cloned();
            api.quote = quote_map.get(&s.id).cloned();
            if let Some(ref mut rb) = api.reblog {
                let rid: i64 = rb.id.parse().unwrap_or(0);
                let rb_id: i64 = rb.account.id.parse().unwrap_or(0);
                rb.account.emojis = account_emojis_map.get(&rb_id).cloned().unwrap_or_default();
                rb.account.roles = account_roles_map.get(&rb_id).cloned().unwrap_or_default();
                rb.tags = tags_map.get(&rid).cloned().unwrap_or_default();
                rb.mentions = rb_mentions;
                rb.emojis = emojis_map.get(&rid).cloned().unwrap_or_default();
                rb.poll = polls_map.get(&rid).cloned();
                rb.card = cards_map.get(&rid).cloned();
            }
            result.push(api);
        }
        hydrate_status_stats(state, result.iter_mut(), viewer_id).await;
        result
    };

    let account_emojis = batch_account_emojis(state, std::slice::from_ref(account)).await;
    let account_roles = batch_account_roles(state, std::slice::from_ref(account)).await;
    let mut api_account = account_from_db(&state.urls, account);
    api_account.emojis = account_emojis.get(&account.id).cloned().unwrap_or_default();
    api_account.roles = account_roles.get(&account.id).cloned().unwrap_or_default();
    apply_account_stats(state, &mut api_account, account.id).await;
    let api_accounts = vec![api_account];

    Ok(AnnualReportsResponse {
        annual_reports: reports,
        accounts: api_accounts,
        statuses: api_statuses,
    })
}

fn db_row_to_report(
    _id: i64,
    account_id: i64,
    year: i32,
    data: Option<serde_json::Value>,
    schema_version: i32,
) -> AnnualReport {
    AnnualReport {
        year,
        data,
        schema_version,
        share_url: None, // no public share URL in eunha
        account_id: account_id.to_string(),
    }
}

// ── GET /api/v1/annual_reports ─────────────────────────────────────────────

pub async fn list_annual_reports(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<AnnualReportsResponse>> {
    auth.require_scope("read:accounts")?;

    let account = sqlx::query_as!(
        DbAccount,
        "SELECT * FROM accounts WHERE id = $1",
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?;

    let rows = sqlx::query!(
        "SELECT id, account_id, year, data, schema_version
         FROM generated_annual_reports
         WHERE account_id = $1 AND viewed_at IS NULL
         ORDER BY year DESC",
        auth.account_id,
    )
    .fetch_all(&state.db)
    .await?;

    let reports: Vec<AnnualReport> = rows
        .into_iter()
        .map(|r| db_row_to_report(r.id, r.account_id, r.year, Some(r.data), r.schema_version))
        .collect();

    let resp = build_response(&state, reports, &account, auth.account_id).await?;
    Ok(Json(resp))
}

// ── GET /api/v1/annual_reports/{year} ─────────────────────────────────────

pub async fn get_annual_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(year): Path<i32>,
) -> AppResult<Json<AnnualReportsResponse>> {
    auth.require_scope("read:accounts")?;

    let account = sqlx::query_as!(
        DbAccount,
        "SELECT * FROM accounts WHERE id = $1",
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?;

    let row = sqlx::query!(
        "SELECT id, account_id, year, data, schema_version
         FROM generated_annual_reports
         WHERE account_id = $1 AND year = $2",
        auth.account_id,
        year,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let report = db_row_to_report(
        row.id,
        row.account_id,
        row.year,
        Some(row.data),
        row.schema_version,
    );
    let resp = build_response(&state, vec![report], &account, auth.account_id).await?;
    Ok(Json(resp))
}

// ── POST /api/v1/annual_reports/{year}/read ────────────────────────────────

pub async fn read_annual_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(year): Path<i32>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("write:accounts")?;

    let updated = sqlx::query_scalar!(
        "UPDATE generated_annual_reports SET viewed_at = NOW(), updated_at = NOW()
         WHERE account_id = $1 AND year = $2
         RETURNING id",
        auth.account_id,
        year,
    )
    .fetch_optional(&state.db)
    .await?;

    if updated.is_none() {
        return Err(AppError::NotFound);
    }

    Ok((StatusCode::OK, Json(serde_json::json!({}))))
}

// ── POST /api/v1/annual_reports/{year}/generate ────────────────────────────

pub async fn generate_annual_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(year): Path<i32>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("write:accounts")?;

    let current_year = Utc::now().year();
    // Only allow generating for completed years (not the current year)
    if year >= current_year {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "Report can only be generated for completed years"
            })),
        )
            .into_response());
    }

    // If already generated, return immediately
    let existing = sqlx::query_scalar!(
        "SELECT id FROM generated_annual_reports WHERE account_id = $1 AND year = $2 AND data IS NOT NULL",
        auth.account_id, year,
    ).fetch_optional(&state.db).await?;

    if existing.is_some() {
        return Ok((StatusCode::ACCEPTED, Json(serde_json::json!({}))).into_response());
    }

    if !is_eligible(&state, auth.account_id, year).await? {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "Not eligible for this year"
            })),
        )
            .into_response());
    }

    // Generated in the background, as `GenerateAnnualReportWorker` does, with
    // an async refresh the client polls until the report is ready.
    let key = refresh_key(auth.account_id, year);
    let refresh = AsyncRefresh::new(&state, &key).await;
    if refresh.is_running() {
        return Ok(accepted_with_refresh(&state, &refresh));
    }
    let refresh = AsyncRefresh::create(&state, &key, false).await;
    crate::jobs::push(
        &state,
        GenerateAnnualReportWorker {
            account_id: auth.account_id,
            year,
        },
    )
    .await;

    Ok(accepted_with_refresh(&state, &refresh))
}

/// `GenerateAnnualReportWorker`: the report, then the async refresh the
/// client polls marked finished.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct GenerateAnnualReportWorker {
    pub account_id: i64,
    pub year: i32,
}

impl crate::jobs::Job for GenerateAnnualReportWorker {
    const KIND: &'static str = "GenerateAnnualReportWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT;

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        generate_and_store(state, self.account_id, self.year)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        crate::async_refresh::finish(state, &refresh_key(self.account_id, self.year)).await;
        Ok(())
    }
}

/// `AnnualReport#refresh_key`.
fn refresh_key(account_id: i64, year: i32) -> String {
    format!("wrapstodon:{account_id}:{year}")
}

/// `head 202`, with the refresh's header (`retry_seconds: 2`).
fn accepted_with_refresh(state: &AppState, refresh: &AsyncRefresh) -> axum::response::Response {
    let mut response = (StatusCode::ACCEPTED, Json(serde_json::json!({}))).into_response();
    if let Some(value) = refresh
        .header_value(state, 2)
        .and_then(|v| axum::http::HeaderValue::from_str(&v).ok())
    {
        response
            .headers_mut()
            .insert(crate::async_refresh::HEADER, value);
    }
    response
}

async fn generate_and_store(state: &AppState, account_id: i64, year: i32) -> AppResult<()> {
    let data = generate_report_data(state, account_id, year).await?;
    sqlx::query!(
        "INSERT INTO generated_annual_reports (account_id, year, data, schema_version, created_at, updated_at)
         VALUES ($1, $2, $3, $4, now(), now())
         ON CONFLICT (account_id, year) DO UPDATE
         SET data = $3, schema_version = $4, updated_at = NOW()",
        account_id, year, data, SCHEMA_VERSION,
    ).execute(&state.db).await?;
    Ok(())
}

// ── GET /api/v1/annual_reports/{year}/state ────────────────────────────────

pub async fn get_annual_report_state(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(year): Path<i32>,
) -> AppResult<axum::response::Response> {
    auth.require_scope("read:accounts")?;

    let row = sqlx::query!(
        "SELECT data FROM generated_annual_reports WHERE account_id = $1 AND year = $2",
        auth.account_id,
        year,
    )
    .fetch_optional(&state.db)
    .await?;

    let mut refresh_header = None;
    let state_str = if let Some(r) = row {
        if r.data.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
            "available"
        } else {
            "generating"
        }
    } else {
        let refresh = AsyncRefresh::new(&state, &refresh_key(auth.account_id, year)).await;
        let current_year = Utc::now().year();
        if refresh.is_running() {
            refresh_header = refresh.header_value(&state, 2);
            "generating"
        } else if year < current_year && is_eligible(&state, auth.account_id, year).await? {
            "eligible"
        } else {
            "ineligible"
        }
    };

    let mut response = Json(serde_json::json!({ "state": state_str })).into_response();
    if let Some(value) = refresh_header.and_then(|v| axum::http::HeaderValue::from_str(&v).ok()) {
        response
            .headers_mut()
            .insert(crate::async_refresh::HEADER, value);
    }
    Ok(response)
}
