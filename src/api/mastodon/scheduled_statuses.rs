use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};
use axum::{
    extract::{Path, Query},
    http::{HeaderMap, Uri},
    response::{IntoResponse, Json},
    Extension,
};
use serde::{Deserialize, Serialize};

pub type ScheduledStatusResponse = ScheduledStatus;

#[derive(Debug, Serialize)]
pub struct ScheduledStatus {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduled_at: Option<String>,
    pub params: serde_json::Value,
    pub media_attachments: Vec<super::types::MediaAttachment>,
}

/// `REST::ScheduledStatusSerializer#params`: the stored params, the quoted
/// post's id as a string, and the quote policy by the name of the first of
/// `InteractionPolicy::POLICY_FLAGS` its automatic bits have, `nobody`
/// when none.
fn serialize_params(params: Option<serde_json::Value>) -> serde_json::Value {
    let Some(serde_json::Value::Object(mut params)) = params else {
        return params.unwrap_or(serde_json::Value::Null);
    };
    let quoted_status_id = match params.get("quoted_status_id") {
        Some(serde_json::Value::Number(id)) => serde_json::Value::String(id.to_string()),
        Some(serde_json::Value::String(id)) => serde_json::Value::String(id.clone()),
        _ => serde_json::Value::Null,
    };
    const POLICY_FLAGS: [(&str, i64); 5] = [
        ("unsupported_policy", 1 << 0),
        ("public", 1 << 1),
        ("followers", 1 << 2),
        ("following", 1 << 3),
        ("disabled", 1 << 4),
    ];
    let bits = params
        .get("quote_approval_policy")
        .and_then(serde_json::Value::as_i64);
    let policy = POLICY_FLAGS
        .iter()
        .find(|(_, flag)| bits.is_some_and(|bits| bits & (flag << 16) != 0))
        .map_or("nobody", |(name, _)| name);
    params.insert("quoted_status_id".into(), quoted_status_id);
    params.insert("quote_approval_policy".into(), policy.into());
    serde_json::Value::Object(params)
}

async fn fetch_scheduled_media(
    state: &AppState,
    scheduled_status_id: i64,
) -> Vec<super::types::MediaAttachment> {
    let rows = sqlx::query_as!(
        crate::db::models::MediaAttachment,
        "SELECT * FROM media_attachments WHERE scheduled_status_id = $1 ORDER BY id",
        scheduled_status_id,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    rows.iter()
        .map(|m| super::convert::media_from_db(&state.urls, m))
        .collect()
}

pub async fn list_scheduled_statuses(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(pagination): Query<super::types::PaginationParams>,
    uri: Uri,
    req_headers: HeaderMap,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:statuses")?;
    let limit = pagination.limit_clamped(20, 40);
    let max_id = pagination
        .max_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let since_id = pagination
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let min_id = pagination
        .min_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());

    let rows = sqlx::query!(
        r#"SELECT id, scheduled_at, params
           FROM scheduled_statuses
           WHERE account_id = $1
             AND ($2::bigint IS NULL OR id < $2)
             AND ($3::bigint IS NULL OR id > $3)
             AND ($5::bigint IS NULL OR id > $5)
           ORDER BY CASE WHEN $5::bigint IS NULL THEN -id ELSE id END
           LIMIT $4"#,
        auth.account_id,
        max_id,
        since_id,
        limit,
        min_id,
    )
    .fetch_all(&state.db)
    .await?;
    let rows = super::timelines::newest_first(min_id, rows);

    let first_id = rows.first().map(|r| r.id.to_string());
    let last_id = rows.last().map(|r| r.id.to_string());

    let mut statuses = Vec::with_capacity(rows.len());
    for r in rows {
        let media_attachments = fetch_scheduled_media(&state, r.id).await;
        statuses.push(ScheduledStatus {
            id: r.id.to_string(),
            scheduled_at: r.scheduled_at.map(super::convert::mastodon_date),
            params: serialize_params(r.params),
            media_attachments,
        });
    }

    let bounds = first_id.zip(last_id);
    let resp_headers = super::link_headers(
        &req_headers,
        &uri,
        bounds.as_ref().map(|(n, o)| (n.as_str(), o.as_str())),
    );

    Ok((resp_headers, Json(statuses)))
}

// ── GET /api/v1/scheduled_statuses/:id ────────────────────────────────────

pub async fn get_scheduled_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<ScheduledStatus>> {
    auth.require_scope("read:statuses")?;
    load(&state, auth.account_id, id)
        .await?
        .map(Json)
        .ok_or(AppError::NotFound)
}

/// `account.scheduled_statuses.find(id)`, as `REST::ScheduledStatusSerializer`
/// shows it.
pub(crate) async fn load(
    state: &AppState,
    account_id: i64,
    id: i64,
) -> AppResult<Option<ScheduledStatus>> {
    let Some(row) = sqlx::query!(
        "SELECT id, scheduled_at, params FROM scheduled_statuses WHERE id = $1 AND account_id = $2",
        id,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(None);
    };

    let media_attachments = fetch_scheduled_media(state, row.id).await;
    Ok(Some(ScheduledStatus {
        id: row.id.to_string(),
        scheduled_at: row.scheduled_at.map(super::convert::mastodon_date),
        params: serialize_params(row.params),
        media_attachments,
    }))
}

// ── PUT /api/v1/scheduled_statuses/:id ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UpdateScheduledStatusForm {
    #[serde(default, deserialize_with = "super::extractors::rails::opt_string")]
    pub scheduled_at: Option<String>,
}

pub async fn update_scheduled_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    super::extractors::Params(form): super::extractors::Params<UpdateScheduledStatusForm>,
) -> AppResult<Json<ScheduledStatus>> {
    auth.require_scope("write:statuses")?;
    let scheduled_at = form
        .scheduled_at
        .as_deref()
        .map(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .map(|dt| dt.with_timezone(&chrono::Utc).naive_utc())
        })
        .transpose()
        .map_err(|_| AppError::Unprocessable("Invalid scheduled_at format".into()))?;

    // Mastodon re-runs ScheduledStatus#validate_future_date on update: the new
    // time must still be at least MINIMUM_OFFSET (5 min) in the future.
    if let Some(at) = scheduled_at {
        if at <= chrono::Utc::now().naive_utc() + chrono::Duration::minutes(5) {
            return Err(AppError::Unprocessable(
                "Validation failed: Scheduled date must be in the future".into(),
            ));
        }
    }

    let row = sqlx::query!(
        r#"UPDATE scheduled_statuses SET scheduled_at = $1
           WHERE id = $2 AND account_id = $3
           RETURNING id, scheduled_at, params"#,
        scheduled_at,
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    state.queues.scheduled_statuses.notify_one();

    let media_attachments = fetch_scheduled_media(&state, row.id).await;
    Ok(Json(ScheduledStatus {
        id: row.id.to_string(),
        scheduled_at: row.scheduled_at.map(super::convert::mastodon_date),
        params: serialize_params(row.params),
        media_attachments,
    }))
}

// ── DELETE /api/v1/scheduled_statuses/:id ─────────────────────────────────

pub async fn delete_scheduled_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:statuses")?;
    let deleted = sqlx::query_scalar!(
        "DELETE FROM scheduled_statuses WHERE id = $1 AND account_id = $2 RETURNING id",
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?;

    if deleted.is_none() {
        return Err(AppError::NotFound);
    }

    Ok(Json(serde_json::json!({})))
}
