//! `GET /api/v2/search`: `Api::V2::SearchController` and `SearchService`.

use axum::{
    extract::{Extension, Query},
    Json,
};
use serde::Deserialize;

use super::{
    accounts::batch_accounts_to_api,
    extractors::FlexBool,
    status_serialize::{build_status, fetch_reblog_data, fetch_status_media},
    types::{SearchResults, Tag},
};
use crate::{
    api::mastodon::resolve_url::Resolved,
    error::{AppError, AppResult},
    middleware::{AuthenticatedUser, ResolvedInstance},
    search::ruby_to_i,
    state::AppState,
};

/// `RESULTS_LIMIT`.
const RESULTS_LIMIT: i64 = 20;

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: Option<String>,
    #[serde(rename = "type")]
    pub search_type: Option<String>,
    pub limit: Option<String>,
    pub offset: Option<String>,
    pub resolve: Option<FlexBool>,
    pub following: Option<FlexBool>,
    pub account_id: Option<String>,
    pub exclude_unreviewed: Option<FlexBool>,
    pub min_id: Option<String>,
    pub max_id: Option<String>,
}

fn truthy(value: Option<FlexBool>) -> bool {
    value.is_some_and(|FlexBool(b)| b)
}

fn present(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.trim().is_empty())
}

pub async fn search(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Query(q): Query<SearchQuery>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<SearchResults>> {
    // `authorize_if_got_token! :read, :'read:search'`.
    if let Some(Extension(ref auth)) = auth {
        auth.require_scope("read:search")?;
    }
    // `validate_search_params!`: `params.require(:q)`.
    let Some(raw_query) = present(q.q.as_deref()) else {
        return Err(AppError::BadRequest(
            "param is missing or the value is empty: q".into(),
        ));
    };
    let viewer_id = auth.as_ref().map(|Extension(a)| a.account_id);
    let resolve = truthy(q.resolve);

    // Pagination and remote resolution both need a signed-in user
    // (`query_pagination_error`, `remote_resolve_error`).
    if viewer_id.is_none() {
        if present(q.offset.as_deref()).is_some() {
            return Err(AppError::UnauthorizedMsg(
                "Search queries pagination is not supported without authentication".into(),
            ));
        }
        if resolve {
            return Err(AppError::UnauthorizedMsg(
                "Search queries that resolve remote resources are not supported without authentication".into(),
            ));
        }
    }

    // `require_valid_pagination_options!`.
    let limit_param = q.limit.as_deref().map(ruby_to_i);
    let offset_param = q.offset.as_deref().map(ruby_to_i);
    if limit_param.is_some_and(|l| l < 0) || offset_param.is_some_and(|o| o < 0) {
        return Err(AppError::BadRequest(
            "Pagination values for `offset` and `limit` must be positive".into(),
        ));
    }
    // `limit_param(RESULTS_LIMIT)`.
    let limit = limit_param.map_or(RESULTS_LIMIT, |l| l.abs().min(RESULTS_LIMIT * 2));

    // `SearchService#call`.
    let query = crate::search::normalize_query(raw_query);
    let search_type = present(q.search_type.as_deref());
    // A page past the first is only for a search of one type.
    let offset = if search_type.is_none() {
        0
    } else {
        offset_param.unwrap_or(0)
    };
    let mut results = SearchResults {
        accounts: vec![],
        statuses: vec![],
        hashtags: vec![],
        collections: vec![],
    };
    if query.is_empty() || limit == 0 {
        return Ok(Json(results));
    }

    // `url_query?`: a URL is resolved rather than searched, and only when the
    // caller asked to resolve. `ResolveURLService` fetches the URL, following
    // the `rel="alternate"` link when what answers is a page rather than an
    // object, so a server that serves its objects from a different path than
    // its pages resolves like any other.
    if resolve && (query.starts_with("http://") || query.starts_with("https://")) {
        if offset == 0 {
            resolve_url_into(&state, &query, viewer_id, search_type, &mut results).await?;
        }
        return Ok(Json(results));
    }

    let wants = |kind: &str| search_type.is_none_or(|t| t == kind);

    if wants("accounts") {
        let found = crate::search::accounts::search(
            &state,
            &query,
            viewer_id,
            &crate::search::accounts::Options {
                limit,
                offset,
                resolve,
                following: truthy(q.following),
                use_searchable_text: true,
            },
        )
        .await?;
        results.accounts = batch_accounts_to_api(&state, &found).await;
    }

    // `status_searchable?`: posts are searched only with Elasticsearch, and
    // only for a signed-in user. Without it, a post is found by its URL alone.

    if wants("hashtags") {
        let found = crate::search::tags::search(
            &state,
            &query,
            &crate::search::tags::Options {
                limit,
                offset,
                exclude_unreviewed: truthy(q.exclude_unreviewed),
            },
        )
        .await?;
        results.hashtags = render_tags(&state, &instance.domain, &found, viewer_id).await?;
    }

    Ok(Json(results))
}

/// `url_resource_results`: what the URL names, when it is of the type asked
/// for.
async fn resolve_url_into(
    state: &AppState,
    query: &str,
    viewer_id: Option<i64>,
    search_type: Option<&str>,
    results: &mut SearchResults,
) -> AppResult<()> {
    match crate::api::mastodon::resolve_url::resolve_url(state, query, viewer_id).await? {
        Some(Resolved::Status(id)) if search_type.is_none_or(|t| t == "statuses") => {
            let s = sqlx::query_as!(
                crate::db::models::Status,
                "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
                id,
            )
            .fetch_one(&state.db)
            .await?;
            let account = sqlx::query_as!(
                crate::db::models::Account,
                "SELECT * FROM accounts WHERE id = $1",
                s.account_id
            )
            .fetch_one(&state.db)
            .await?;
            let media = fetch_status_media(state, s.id).await?;
            let reblog = fetch_reblog_data(state, &s).await?;
            // The viewer's context, so that quote_approval and the interaction
            // flags describe the requester rather than defaulting to unknown.
            let ctx = if let Some(vid) = viewer_id {
                super::statuses::batch_viewer_contexts(state, vid, &[s.id])
                    .await?
                    .remove(&s.id)
            } else {
                None
            };
            results.statuses = vec![build_status(state, &s, &account, media, reblog, ctx).await?];
        }
        Some(Resolved::Account(id)) if search_type.is_none_or(|t| t == "accounts") => {
            let account = sqlx::query_as!(
                crate::db::models::Account,
                "SELECT * FROM accounts WHERE id = $1",
                id,
            )
            .fetch_one(&state.db)
            .await?;
            results.accounts = batch_accounts_to_api(state, &[account]).await;
        }
        _ => {}
    }
    Ok(())
}

/// `REST::TagSerializer` for each hashtag found: its history, and for a
/// signed-in user whether they follow and feature it.
async fn render_tags(
    state: &AppState,
    domain: &str,
    found: &[crate::search::tags::FoundTag],
    viewer_id: Option<i64>,
) -> AppResult<Vec<Tag>> {
    let ids: Vec<i64> = found.iter().map(|t| t.id).collect();
    let histories = super::tags::fetch_tags_histories(state, &ids).await;
    let (followed, featured): (Vec<i64>, Vec<i64>) = match viewer_id {
        Some(viewer) => (
            sqlx::query_scalar!(
                "SELECT tag_id FROM tag_follows WHERE account_id = $1 AND tag_id = ANY($2)",
                viewer,
                &ids,
            )
            .fetch_all(&state.db)
            .await?,
            sqlx::query_scalar!(
                "SELECT tag_id FROM featured_tags WHERE account_id = $1 AND tag_id = ANY($2)",
                viewer,
                &ids,
            )
            .fetch_all(&state.db)
            .await?,
        ),
        None => (vec![], vec![]),
    };
    Ok(found
        .iter()
        .map(|t| Tag {
            id: t.id.to_string(),
            name: t.display_name.clone(),
            url: format!("https://{domain}/tags/{}", t.name),
            history: histories.get(&t.id).cloned().unwrap_or_default(),
            following: viewer_id.map(|_| followed.contains(&t.id)),
            featuring: viewer_id.map(|_| featured.contains(&t.id)),
        })
        .collect())
}
