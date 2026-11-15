//! Quote management: listing a status's quotes and revoking a quote.
//!
//! Mastodon: app/controllers/api/v1/statuses/quotes_controller.rb
//! Mastodon: app/services/revoke_quote_service.rb

use super::*;

// ── GET /api/v1/statuses/:id/quotes ──────────────────────────────────────

/// `Api::V1::Statuses::QuotesController#index`: the posts quoting this one
/// with an accepted quote, paged by the quotes' ids
/// (`@status.quotes.accepted.paginate_by_max_id`), less those of accounts
/// the viewer blocks, mutes or is blocked by (`not_excluded_by_account`);
/// for anyone but the quoted author, a page is then rid of what
/// `StatusFilter#filtered?` hides from them.
pub async fn get_status_quotes(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    req_headers: axum::http::HeaderMap,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    Query(params): Query<PaginationParams>,
) -> AppResult<impl axum::response::IntoResponse> {
    auth.require_scope("read:statuses")?;
    let viewer_id = auth.account_id;
    let limit: i64 = params.limit_clamped(20, 40);
    // `paginate_by_max_id(limit, params[:max_id], params[:since_id])`:
    // `min_id` is not read.
    let max_id: Option<i64> = params.max_id.as_deref().and_then(|s| s.parse().ok());
    let since_id: Option<i64> = params.since_id.as_deref().and_then(|s| s.parse().ok());

    // `set_status`: the quoted status, as `StatusPolicy#show?` lets the
    // caller see it.
    let (quoted_status, _) = fetch_status_with_account(&state, id).await?;
    check_status_visible(&state, &quoted_status, viewer_id).await?;

    let rows = sqlx::query!(
        r#"SELECT q.id AS "quote_id!", s.id AS "status_id!" FROM quotes q
           JOIN statuses s ON s.id = q.status_id AND s.deleted_at IS NULL
           WHERE q.quoted_status_id = $1 AND q.state = 1
             AND NOT EXISTS (SELECT 1 FROM blocks b
                             WHERE b.account_id = $2 AND b.target_account_id = s.account_id)
             AND NOT EXISTS (SELECT 1 FROM blocks b
                             WHERE b.account_id = s.account_id AND b.target_account_id = $2)
             AND NOT EXISTS (SELECT 1 FROM mutes m
                             WHERE m.account_id = $2 AND m.target_account_id = s.account_id)
             AND ($3::bigint IS NULL OR q.id < $3)
             AND ($4::bigint IS NULL OR q.id > $4)
           ORDER BY q.id DESC
           LIMIT $5"#,
        id,
        viewer_id,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(&state.db)
    .await?;

    // The next page's bounds, taken before the page is filtered.
    let records_continue = rows.len() as i64 == limit;
    let pagination_since_id = rows.first().map(|r| r.quote_id);
    let pagination_max_id = rows.last().map(|r| r.quote_id).filter(|_| records_continue);

    let status_ids: Vec<i64> = rows.iter().map(|r| r.status_id).collect();
    let quotes = if viewer_id == quoted_status.account_id {
        let mut by_id: std::collections::HashMap<i64, DbStatus> = sqlx::query_as!(
            DbStatus,
            "SELECT * FROM statuses WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL",
            &status_ids,
        )
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .map(|s| (s.id, s))
        .collect();
        status_ids
            .iter()
            .filter_map(|id| by_id.remove(id))
            .collect()
    } else {
        super::context::permitted_statuses(&state, &status_ids, Some(viewer_id)).await?
    };

    use crate::api::mastodon::timelines::build_status_list_with_filters;
    let result = build_status_list_with_filters(&state, quotes, Some(viewer_id)).await?;

    // `next_path` when the page came back full, `prev_path` unless nothing
    // is left of it, each with the request's `limit` alone.
    let host = req_headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let proto = req_headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("https");
    let given_limit = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
        .filter(|(key, _)| key == "limit")
        .last()
        .map(|(_, value)| {
            url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
        });
    let link = |key: &str, value: i64| match &given_limit {
        Some(limit) => format!("{proto}://{host}{}?limit={limit}&{key}={value}", uri.path()),
        None => format!("{proto}://{host}{}?{key}={value}", uri.path()),
    };
    let mut links = Vec::new();
    if let Some(max_id) = pagination_max_id {
        links.push(format!(r#"<{}>; rel="next""#, link("max_id", max_id)));
    }
    if let Some(since_id) = pagination_since_id.filter(|_| !result.is_empty()) {
        links.push(format!(r#"<{}>; rel="prev""#, link("since_id", since_id)));
    }
    let mut headers = axum::http::HeaderMap::new();
    if !links.is_empty() {
        if let Ok(val) = links.join(", ").parse() {
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
        serialize_status_for_viewer(&state, &quoting_status, auth.account_id).await?,
    ))
}
