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

    // `set_status`: the quoted status, as `StatusPolicy#show?` lets the
    // caller see it.
    let (quoted_status, _) = fetch_status_with_account(&state, id).await?;
    check_status_visible(&state, &quoted_status, auth.account_id).await?;

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

/// `Api::V1::Statuses::QuotesController#revoke`: the quoted author takes back
/// a quote of their post (`RevokeQuoteService`).
pub async fn revoke_quote(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path((quoted_status_id, quoting_status_id)): Path<(i64, i64)>,
) -> AppResult<impl axum::response::IntoResponse> {
    auth.require_scope("write:statuses")?;

    // `set_status`: the quoted status, as `StatusPolicy#show?` lets the
    // caller see it.
    let (quoted_status, _) = fetch_status_with_account(&state, quoted_status_id).await?;
    check_status_visible(&state, &quoted_status, auth.account_id).await?;

    // `set_quote`: `@status.quotes.find_by!(status_id:)`, in whatever state.
    // A deleted quoting status's quote went with it.
    let quote = sqlx::query_scalar!(
        r#"SELECT q.id FROM quotes q JOIN statuses s ON s.id = q.status_id
           WHERE q.quoted_status_id = $1 AND q.status_id = $2 AND s.deleted_at IS NULL"#,
        quoted_status_id,
        quoting_status_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let quote = crate::quotes::find(&state.db, quote)
        .await?
        .ok_or(AppError::NotFound)?;

    // `QuotePolicy#revoke?`
    if quote.quoted_account_id.is_none() || quote.quoted_account_id != Some(auth.account_id) {
        return Err(AppError::Forbidden);
    }

    crate::quotes::revoke(&state, &quote, false).await?;

    let quoting_status = sqlx::query_as!(
        DbStatus,
        "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        quoting_status_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(
        serialize_status(&state, &quoting_status, Some(auth.account_id)).await?,
    ))
}
