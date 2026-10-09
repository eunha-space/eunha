use axum::{
    extract::{Extension, Path},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::{Datelike, TimeZone, Utc};
use serde::Serialize;

use super::{
    accounts::{batch_account_emojis, batch_account_roles},
    convert::status_from_db,
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

/// `AnnualReport::SCHEMA`.
const SCHEMA_VERSION: i32 = 2;

/// `AnnualReport.current_campaign`: the year whose reports are on offer,
/// while the `wrapstodon` setting is on and it is 10 to 31 December (UTC).
pub(crate) async fn current_campaign(state: &AppState) -> Option<i32> {
    if !crate::settings::boolean(state, "wrapstodon").await {
        return None;
    }
    campaign_at(Utc::now())
}

/// The date half of [`current_campaign`].
fn campaign_at(now: chrono::DateTime<Utc>) -> Option<i32> {
    (now.month() == 12 && (10..=31).contains(&now.day())).then(|| now.year())
}

#[cfg(test)]
mod campaign_tests {
    use chrono::TimeZone;

    #[test]
    fn runs_from_the_tenth_of_december_to_the_end_of_the_year() {
        let at =
            |m, d| super::campaign_at(chrono::Utc.with_ymd_and_hms(2026, m, d, 12, 0, 0).unwrap());
        assert_eq!(at(12, 9), None);
        assert_eq!(at(12, 10), Some(2026));
        assert_eq!(at(12, 31), Some(2026));
        assert_eq!(at(11, 20), None);
        assert_eq!(at(1, 5), None);
    }
}
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

/// `AnnualReport#data`: each of `AnnualReport::SOURCES` in turn, over
/// `report_statuses`, the account's posts (deleted ones aside) whose ids fall
/// in [`year_as_snowflake_range`].
async fn generate_report_data(
    state: &AppState,
    account_id: i64,
    year: i32,
) -> AppResult<serde_json::Value> {
    let (first, last) = year_as_snowflake_range(year);

    // `AnnualReport::Archetype`, and `TimeSeries`' count of posts.
    let counts = sqlx::query!(
        r#"SELECT
             COUNT(*) FILTER (WHERE reblog_of_id IS NOT NULL) AS "reblogs!",
             COUNT(*) FILTER (WHERE in_reply_to_id IS NOT NULL
                                AND in_reply_to_account_id <> $1) AS "replies!",
             COUNT(*) FILTER (WHERE (reply = FALSE OR in_reply_to_account_id = account_id)
                                AND reblog_of_id IS NULL) AS "standalone!",
             COUNT(*) FILTER (WHERE poll_id IS NOT NULL) AS "polls!",
             COUNT(*) AS "statuses!"
           FROM statuses
           WHERE account_id = $1 AND deleted_at IS NULL AND id BETWEEN $2 AND $3"#,
        account_id,
        first,
        last,
    )
    .fetch_one(&state.db)
    .await?;
    let archetype = archetype(
        counts.reblogs,
        counts.replies,
        counts.standalone,
        counts.polls,
    );

    // `AnnualReport::TopStatuses`: the most boosted public or unlisted post,
    // among those with stats; Mastodon leaves the other two empty.
    let by_reblogs = sqlx::query_scalar!(
        r#"SELECT s.id FROM statuses s
           JOIN status_stats ss ON ss.status_id = s.id
           WHERE s.account_id = $1 AND s.deleted_at IS NULL AND s.id BETWEEN $2 AND $3
             AND s.visibility IN (0, 1) /* vis::PUBLIC, vis::UNLISTED */
           ORDER BY ss.reblogs_count DESC LIMIT 1"#,
        account_id,
        first,
        last,
    )
    .fetch_optional(&state.db)
    .await?;

    // `AnnualReport::TimeSeries`: the follows of the account made in the
    // year, by `DATE_PART`.
    let followers = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "count!" FROM follows
           WHERE target_account_id = $1 AND DATE_PART('year', created_at) = $2"#,
        account_id,
        f64::from(year),
    )
    .fetch_one(&state.db)
    .await?;

    // `AnnualReport::TopHashtags`: the hashtag used most, by its display
    // name, when it was used more than once.
    let top_hashtags: Vec<serde_json::Value> = sqlx::query!(
        r#"SELECT COALESCE(t.display_name, t.name) AS "name!", COUNT(*) AS "count!"
           FROM tags t
           JOIN statuses_tags st ON st.tag_id = t.id
           JOIN statuses s ON s.id = st.status_id
           WHERE s.deleted_at IS NULL
             AND s.id IN (SELECT id FROM statuses
                          WHERE account_id = $1 AND deleted_at IS NULL
                            AND id BETWEEN $2 AND $3)
           GROUP BY COALESCE(t.display_name, t.name)
           HAVING COUNT(*) > 1
           ORDER BY COUNT(*) DESC LIMIT 1"#,
        account_id,
        first,
        last,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| serde_json::json!({ "name": r.name, "count": r.count }))
    .collect();

    Ok(serde_json::json!({
        "archetype": archetype,
        "top_statuses": {
            "by_reblogs": by_reblogs.map(|id| id.to_string()),
            "by_favourites": null,
            "by_replies": null,
        },
        "time_series": [{
            "month": 12,
            "statuses": counts.statuses,
            "followers": followers,
        }],
        "top_hashtags": top_hashtags,
    }))
}

/// `AnnualReport::Archetype#archetype`.
fn archetype(reblogs: i64, replies: i64, standalone: i64, polls: i64) -> &'static str {
    if standalone + replies + reblogs < AVERAGE_POSTS_PER_YEAR {
        "lurker"
    } else if reblogs > standalone * 2 {
        "booster"
    } else if (polls as f64) > (standalone as f64) * 0.1 {
        // `standalone_count * 0.1`, a float.
        "pollster"
    } else if replies > standalone * 2 {
        "replier"
    } else {
        "oracle"
    }
}

#[cfg(test)]
mod archetype_tests {
    use super::archetype;

    #[test]
    fn is_mastodons() {
        assert_eq!(archetype(10, 10, 10, 10), "lurker");
        assert_eq!(archetype(100, 0, 40, 0), "booster");
        // 5 polls against 45 standalone posts: more than 4.5.
        assert_eq!(archetype(0, 70, 45, 5), "pollster");
        assert_eq!(archetype(0, 70, 50, 5), "oracle");
        assert_eq!(archetype(0, 200, 50, 0), "replier");
    }
}

/// `AnnualReport#eligible?`: every source is, which comes to
/// `TopStatuses` (a public or unlisted post of the year) and `TopHashtags`
/// (a post of the year with a hashtag). The year is `year_as_snowflake_range`,
/// which Mastodon draws with up to a second of randomness at each end; eunha
/// takes the whole seconds.
async fn is_eligible(state: &AppState, account_id: i64, year: i32) -> AppResult<bool> {
    let (first, last) = year_as_snowflake_range(year);
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM statuses
             WHERE account_id = $1 AND deleted_at IS NULL AND id BETWEEN $2 AND $3
               AND visibility IN (0, 1)
           ) AND EXISTS (
             SELECT 1 FROM statuses
             JOIN statuses_tags ON statuses_tags.status_id = statuses.id
             WHERE statuses.account_id = $1 AND statuses.deleted_at IS NULL
               AND statuses.id BETWEEN $2 AND $3
           ) AS "eligible!""#,
        account_id,
        first,
        last,
    )
    .fetch_one(&state.db)
    .await?)
}

/// `AnnualReport::Source#year_as_snowflake_range`: from the start of the
/// year to the end of its last second.
fn year_as_snowflake_range(year: i32) -> (i64, i64) {
    let at = |y, m, d, h, min, sec| {
        Utc.with_ymd_and_hms(y, m, d, h, min, sec)
            .single()
            .map_or(0, crate::snowflake::id_at)
    };
    (at(year, 1, 1, 0, 0, 0), at(year, 12, 31, 23, 59, 59))
}

/// `params[:id]&.to_i`, as a year.
fn year_param(raw: &str) -> i32 {
    crate::search::ruby_to_i(raw).clamp(i32::MIN as i64, i32::MAX as i64) as i32
}

// ── Build the response: fetch referenced accounts + statuses ───────────────

async fn build_response(
    state: &AppState,
    reports: Vec<AnnualReport>,
    account: &DbAccount,
    viewer_id: Option<i64>,
) -> AppResult<AnnualReportsResponse> {
    // `AnnualReportsPresenter#statuses`: every report's `status_ids`.
    let mut top_status_ids: Vec<i64> = Vec::new();
    for report in &reports {
        for id in report.data.as_ref().map(status_ids).unwrap_or_default() {
            if !top_status_ids.contains(&id) {
                top_status_ids.push(id);
            }
        }
    }

    // Fetch referenced statuses
    let api_statuses = if top_status_ids.is_empty() {
        vec![]
    } else {
        let statuses = sqlx::query_as!(
            DbStatus,
            // `Status.where(id:)`, under `default_scope { recent.kept }`.
            "SELECT * FROM statuses WHERE id = ANY($1) AND deleted_at IS NULL ORDER BY id DESC",
            &top_status_ids,
        )
        .fetch_all(&state.db)
        .await?;

        let all_ids: Vec<i64> = statuses.iter().map(|s| s.id).collect();
        let media_map = batch_status_media(state, &all_ids).await?;
        let reblog_map = batch_reblog_data(state, &statuses).await?;
        let quote_map = batch_quote_data(state, &statuses, viewer_id).await?;
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
        let polls_map = batch_status_polls(state, &enrich_ids, viewer_id).await?;
        let cards_map = batch_status_cards(state, &enrich_ids, viewer_id).await?;
        // Serialized for no one (`scope: nil`) on the shared page.
        let ctxs = match viewer_id {
            Some(viewer_id) => {
                super::statuses::batch_viewer_contexts(state, viewer_id, &all_ids).await?
            }
            None => Default::default(),
        };

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
            api.account.set_roles(
                account_roles_map
                    .get(&account.id)
                    .cloned()
                    .unwrap_or_default(),
            );
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
                rb.account
                    .set_roles(account_roles_map.get(&rb_id).cloned().unwrap_or_default());
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

    // `AnnualReportsPresenter#accounts`: every report's `account_ids`.
    let mut account_ids: Vec<i64> = Vec::new();
    for report in &reports {
        let owner = report.account_id.parse().unwrap_or(account.id);
        for id in report_account_ids(report.schema_version, owner, report.data.as_ref()) {
            if !account_ids.contains(&id) {
                account_ids.push(id);
            }
        }
    }
    let accounts = if account_ids.is_empty() {
        vec![]
    } else {
        sqlx::query_as::<_, DbAccount>("SELECT * FROM accounts WHERE id = ANY($1) ORDER BY id")
            .bind(&account_ids)
            .fetch_all(&state.db)
            .await?
    };
    let api_accounts = super::accounts::batch_accounts_to_api(state, &accounts).await;

    Ok(AnnualReportsResponse {
        annual_reports: reports,
        accounts: api_accounts,
        statuses: api_statuses,
    })
}

/// `GeneratedAnnualReport#status_ids`: the values of `top_statuses`.
fn status_ids(data: &serde_json::Value) -> Vec<i64> {
    data["top_statuses"]
        .as_object()
        .into_iter()
        .flat_map(|top| top.values())
        .filter_map(json_id)
        .collect()
}

/// `GeneratedAnnualReport#account_ids`: for schema 1, the accounts most
/// boosted and most interacted with; for schema 2, the report's own.
fn report_account_ids(
    schema_version: i32,
    account_id: i64,
    data: Option<&serde_json::Value>,
) -> Vec<i64> {
    match schema_version {
        1 => [
            "most_reblogged_accounts",
            "commonly_interacted_with_accounts",
        ]
        .iter()
        .flat_map(|key| {
            data.and_then(|d| d[*key].as_array())
                .into_iter()
                .flatten()
                .filter_map(|entry| json_id(&entry["account_id"]))
        })
        .collect(),
        2 => vec![account_id],
        _ => vec![],
    }
}

/// An id as the report's JSON holds it, a string or a number.
fn json_id(value: &serde_json::Value) -> Option<i64> {
    value
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| value.as_i64())
}

#[cfg(test)]
mod presenter_tests {
    use serde_json::json;

    #[test]
    fn ids_are_the_presenters() {
        let v2 = json!({ "top_statuses": { "by_reblogs": "5", "by_favourites": null } });
        assert_eq!(super::status_ids(&v2), [5]);
        assert_eq!(super::report_account_ids(2, 9, Some(&v2)), [9]);
        let v1 = json!({
            "top_statuses": { "by_reblogs": "5", "by_replies": "7" },
            "most_reblogged_accounts": [{ "account_id": "3", "count": 2 }],
            "commonly_interacted_with_accounts": [{ "account_id": 4, "count": 1 }],
        });
        assert_eq!(super::status_ids(&v1), [5, 7]);
        assert_eq!(super::report_account_ids(1, 9, Some(&v1)), [3, 4]);
    }
}

fn db_row_to_report(
    state: &AppState,
    account: &DbAccount,
    year: i32,
    data: Option<serde_json::Value>,
    schema_version: i32,
    share_key: Option<&str>,
) -> AnnualReport {
    AnnualReport {
        year,
        data,
        schema_version,
        share_url: share_url(&state.urls.local_domain, &account.username, year, share_key),
        account_id: account.id.to_string(),
    }
}

/// `REST::AnnualReportSerializer#share_url`: `public_wrapstodon_url`, the
/// shared page, once the report has a share key.
fn share_url(
    local_domain: &str,
    username: &str,
    year: i32,
    share_key: Option<&str>,
) -> Option<String> {
    share_key
        .filter(|key| !key.trim().is_empty())
        .map(|key| format!("https://{local_domain}/@{username}/wrapstodon/{year}/{key}"))
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
        "SELECT year, data, schema_version, share_key
         FROM generated_annual_reports
         WHERE account_id = $1 AND viewed_at IS NULL
         ORDER BY year DESC",
        auth.account_id,
    )
    .fetch_all(&state.db)
    .await?;

    let reports: Vec<AnnualReport> = rows
        .into_iter()
        .map(|r| {
            db_row_to_report(
                &state,
                &account,
                r.year,
                Some(r.data),
                r.schema_version,
                r.share_key.as_deref(),
            )
        })
        .collect();

    let resp = build_response(&state, reports, &account, Some(auth.account_id)).await?;
    Ok(Json(resp))
}

// ── GET /api/v1/annual_reports/{year} ─────────────────────────────────────

pub async fn get_annual_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(year): Path<String>,
) -> AppResult<Json<AnnualReportsResponse>> {
    auth.require_scope("read:accounts")?;
    let year = year_param(&year);

    let account = sqlx::query_as!(
        DbAccount,
        "SELECT * FROM accounts WHERE id = $1",
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?;

    let row = sqlx::query!(
        "SELECT year, data, schema_version, share_key
         FROM generated_annual_reports
         WHERE account_id = $1 AND year = $2",
        auth.account_id,
        year,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let report = db_row_to_report(
        &state,
        &account,
        row.year,
        Some(row.data),
        row.schema_version,
        row.share_key.as_deref(),
    );
    let resp = build_response(&state, vec![report], &account, Some(auth.account_id)).await?;
    Ok(Json(resp))
}

// ── POST /api/v1/annual_reports/{year}/read ────────────────────────────────

pub async fn read_annual_report(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(year): Path<String>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("write:accounts")?;
    let year = year_param(&year);

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
    Path(year): Path<String>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("write:accounts")?;
    let year = year_param(&year);

    // `render_empty` unless it is this year's campaign, and once the report
    // exists.
    let render_empty = || Ok((StatusCode::OK, Json(serde_json::json!({}))).into_response());
    if current_campaign(&state).await != Some(year) {
        return render_empty();
    }
    let existing = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM generated_annual_reports
                          WHERE account_id = $1 AND year = $2) AS "e!""#,
        auth.account_id,
        year,
    )
    .fetch_one(&state.db)
    .await?;
    if existing {
        return render_empty();
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
    let mut response = StatusCode::ACCEPTED.into_response();
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

/// `AnnualReport#generate`: nothing once the report exists, else the report
/// with a share key; a deleted account has none made.
async fn generate_and_store(state: &AppState, account_id: i64, year: i32) -> AppResult<()> {
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM generated_annual_reports
                          WHERE account_id = $1 AND year = $2) AS "e!""#,
        account_id,
        year,
    )
    .fetch_one(&state.db)
    .await?;
    let account_exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM accounts WHERE id = $1) AS "e!""#,
        account_id,
    )
    .fetch_one(&state.db)
    .await?;
    if exists || !account_exists {
        return Ok(());
    }
    let data = generate_report_data(state, account_id, year).await?;
    // `SecureRandom.hex(8)`.
    let share_key = hex::encode(rand::random::<[u8; 8]>());
    // `rescue ActiveRecord::RecordNotUnique`: another worker made it first.
    sqlx::query!(
        "INSERT INTO generated_annual_reports
           (account_id, year, data, schema_version, share_key, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, now(), now())
         ON CONFLICT (account_id, year) DO NOTHING",
        account_id,
        year,
        data,
        SCHEMA_VERSION,
        share_key,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

// ── GET /api/v1/annual_reports/{year}/state ────────────────────────────────

pub async fn get_annual_report_state(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(year): Path<String>,
) -> AppResult<axum::response::Response> {
    auth.require_scope("read:accounts")?;
    let year = year_param(&year);

    // `AnnualReport#state`.
    let exists = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM generated_annual_reports
                          WHERE account_id = $1 AND year = $2) AS "e!""#,
        auth.account_id,
        year,
    )
    .fetch_one(&state.db)
    .await?;
    let mut refresh_header = None;
    let state_str = if exists {
        "available"
    } else {
        let refresh = AsyncRefresh::new(&state, &refresh_key(auth.account_id, year)).await;
        if refresh.is_running() {
            refresh_header = refresh.header_value(&state, 2);
            "generating"
        } else if current_campaign(&state).await == Some(year)
            && is_eligible(&state, auth.account_id, year).await?
        {
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

// ── GET /@{account_username}/wrapstodon/{year}/{share_key} ────────────────

/// What `WrapstodonController#show` finds for a shared report.
pub enum Shared {
    /// `AccountOwnedConcern#authenticate_user!`: in limited federation mode
    /// the page is for signed-in users only.
    SignInRequired,
    /// No such local account or report, or an account still pending,
    /// unconfirmed, or without a user.
    NotFound,
    /// `permanent_unavailability_response`: suspended or deleted with no
    /// deletion request left to undo it.
    Gone,
    /// `temporary_suspension_response`.
    Suspended,
    Found(Box<SharedReport>),
}

pub struct SharedReport {
    pub account: DbAccount,
    pub year: i32,
    /// `render_wrapstodon_share_data`: `REST::AnnualReportsSerializer` with
    /// no viewer, plus `me` and the local `domain`.
    pub payload: serde_json::Value,
}

/// `WrapstodonController`: the report of a local account's `year` whose
/// share key is `share_key`, shown to anyone who has the link.
pub async fn shared(
    state: &AppState,
    viewer: Option<&AuthenticatedUser>,
    username: &str,
    year: &str,
    share_key: &str,
) -> AppResult<Shared> {
    let signed_in = viewer.filter(|v| v.user_id.is_some());
    if state.instance.limited_federation_mode
        && !signed_in.is_some_and(|v| v.standing == crate::middleware::Standing::Functional)
    {
        return Ok(Shared::SignInRequired);
    }

    // `AccountOwnedConcern`: `Account.find_local!`, then approval,
    // suspension and confirmation, in that order.
    let Some(account) = crate::search::accounts::find_remote(state, username, None).await? else {
        return Ok(Shared::NotFound);
    };
    let user = sqlx::query!(
        "SELECT approved, confirmed_at IS NOT NULL AS \"confirmed!\"
         FROM users WHERE account_id = $1",
        account.id,
    )
    .fetch_optional(&state.db)
    .await?;
    if user.as_ref().is_some_and(|u| !u.approved) {
        return Ok(Shared::NotFound);
    }
    if account.is_unavailable() {
        let reversible = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM account_deletion_requests WHERE account_id = $1) AS "e!""#,
            account.id,
        )
        .fetch_one(&state.db)
        .await?;
        if !reversible {
            return Ok(Shared::Gone);
        }
        return Ok(Shared::Suspended);
    }
    if !user.is_some_and(|u| u.confirmed) {
        return Ok(Shared::NotFound);
    }

    // `GeneratedAnnualReport.find_by!(account:, year:, share_key:)`.
    let year = year_param(year);
    let Some(row) = sqlx::query!(
        "SELECT year, data, schema_version, share_key
         FROM generated_annual_reports
         WHERE account_id = $1 AND year = $2 AND share_key = $3",
        account.id,
        year,
        share_key,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(Shared::NotFound);
    };

    let report = db_row_to_report(
        state,
        &account,
        row.year,
        Some(row.data),
        row.schema_version,
        row.share_key.as_deref(),
    );
    let response = build_response(state, vec![report], &account, None).await?;
    let mut payload = serde_json::to_value(response)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serializing the report: {e}")))?;
    if let Some(viewer) = signed_in {
        payload["me"] = viewer.account_id.to_string().into();
    }
    payload["domain"] = idna::domain_to_unicode(&state.urls.local_domain).0.into();
    Ok(Shared::Found(Box::new(SharedReport {
        account,
        year: row.year,
        payload,
    })))
}

#[cfg(test)]
mod share_url_tests {
    #[test]
    fn is_the_shared_page_once_there_is_a_share_key() {
        assert_eq!(
            super::share_url("example.com", "alice", 2025, Some("0123456789abcdef")).as_deref(),
            Some("https://example.com/@alice/wrapstodon/2025/0123456789abcdef"),
        );
        assert_eq!(super::share_url("example.com", "alice", 2025, None), None);
        assert_eq!(
            super::share_url("example.com", "alice", 2025, Some("")),
            None
        );
    }
}
