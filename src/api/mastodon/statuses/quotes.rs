//! Quote management: listing a status's quotes and revoking a quote.

use super::*;

// ── GET /api/v1/statuses/:id/quotes ──────────────────────────────────────

pub async fn get_status_quotes(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    axum::extract::RawQuery(raw_query): axum::extract::RawQuery,
    req_headers: axum::http::HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    Query(params): Query<PaginationParams>,
) -> AppResult<impl axum::response::IntoResponse> {
    auth.require_scope("read:statuses")?;
    let viewer_id = Some(auth.account_id);
    let limit: i64 = params.limit_clamped(20, 40);
    let max_id: Option<i64> = params.max_id.as_deref().and_then(|s| s.parse().ok());
    let since_id: Option<i64> = params.since_id.as_deref().and_then(|s| s.parse().ok());
    let min_id: Option<i64> = params.min_id.as_deref().and_then(|s| s.parse().ok());

    // Verify the quoted status exists
    let _ = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    // Only return accepted quotes; private quoting statuses are hidden from non-owners
    let quoted_owner: Option<i64> =
        sqlx::query_scalar!("SELECT account_id FROM statuses WHERE id = $1", id,)
            .fetch_optional(&state.db)
            .await?;
    let viewer_is_owner = viewer_id.is_some() && viewer_id == quoted_owner;

    let quotes = sqlx::query_as!(
        DbStatus,
        r#"SELECT s.* FROM statuses s
           JOIN quotes q ON q.status_id = s.id AND q.quoted_status_id = $1
           WHERE s.deleted_at IS NULL
             AND q.state = 1
             AND (s.visibility IN (0, 1) OR (s.visibility = 2 AND $6::bool))
             AND ($2::bigint IS NULL OR q.id < $2)
             AND ($3::bigint IS NULL OR q.id > $3)
             AND ($4::bigint IS NULL OR q.id > $4)
           ORDER BY q.id DESC
           LIMIT $5"#,
        id,
        max_id,
        since_id,
        min_id,
        limit,
        viewer_is_owner,
    )
    .fetch_all(&state.db)
    .await?;

    use crate::api::mastodon::timelines::build_status_list_with_filters;
    let result = build_status_list_with_filters(&state, quotes, viewer_id).await?;

    let link = result.first().zip(result.last()).map(|(newest, oldest)| {
        let extra = crate::api::mastodon::non_pagination_query(raw_query.as_deref());
        crate::api::mastodon::link_header(&req_headers, uri.path(), &extra, &newest.id, &oldest.id)
    });
    let mut headers = axum::http::HeaderMap::new();
    if let Some(v) = link {
        if let Ok(val) = v.parse() {
            headers.insert(axum::http::header::LINK, val);
        }
    }
    Ok((headers, Json(result)))
}

// ── POST /api/v1/statuses/:status_id/quotes/:id/revoke ────────────────────

pub async fn revoke_quote(
    state: AppState,
    Extension(ResolvedInstance(_instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path((quoted_status_id, quoting_status_id)): Path<(i64, i64)>,
) -> AppResult<impl axum::response::IntoResponse> {
    auth.require_scope("write:statuses")?;

    // Find the quote record; the caller must be the quoted status's author
    let quote = sqlx::query!(
        r#"SELECT q.id, q.status_id, q.quoted_status_id, q.quoted_account_id, q.state
           FROM quotes q
           WHERE q.quoted_status_id = $1 AND q.status_id = $2 AND q.state != 3"#,
        quoted_status_id,
        quoting_status_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    if quote.quoted_account_id != Some(auth.account_id) {
        return Err(AppError::Forbidden);
    }

    sqlx::query!("UPDATE quotes SET state = 3 WHERE id = $1", quote.id,)
        .execute(&state.db)
        .await?;

    // Return the quoting status
    let quoting_status = sqlx::query_as!(
        DbStatus,
        "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        quoting_status_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let account = fetch_account(&state, quoting_status.account_id).await?;
    let media = fetch_status_media(&state, quoting_status.id).await?;
    let reblog = fetch_reblog_data(&state, &quoting_status).await?;
    let ctx = build_viewer_context(&state, auth.account_id, quoting_status.id)
        .await
        .ok();
    let api_status = build_status(&state, &quoting_status, &account, media, reblog, ctx).await?;
    Ok(Json(api_status))
}
