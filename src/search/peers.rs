//! `Api::V1::Peers::SearchController#set_domains`: known domains starting
//! with what was typed, from the instances index when Elasticsearch is on and
//! from the `instances` rows otherwise. A search server that fails is a 500,
//! as nothing upstream rescues it.

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// `LIMIT`.
pub const LIMIT: i64 = 10;

/// `TagManager#normalize_domain`: stripped, without a trailing slash,
/// lowercased and in its ASCII form. `None` where Addressable would raise.
pub fn normalize_domain(domain: &str) -> Option<String> {
    let domain = domain.trim();
    let domain = domain.strip_suffix('/').unwrap_or(domain).to_lowercase();
    if domain.is_empty() {
        return Some(String::new());
    }
    url::Url::parse(&format!("https://{domain}/"))
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
}

/// The domains for `q`; `None` for a blank one, which the endpoint renders as
/// `null`.
pub async fn search(state: &AppState, q: Option<&str>) -> AppResult<Option<Vec<String>>> {
    let Some(q) = q.filter(|q| !q.trim().is_empty()) else {
        return Ok(None);
    };
    let Some(domain) = normalize_domain(q) else {
        return Ok(Some(vec![]));
    };
    // Nothing rescues a search server error here, so it is a 500.
    if let Some(found) = crate::search::elasticsearch::peers(state, &domain).await {
        return found
            .map(Some)
            .map_err(|e| AppError::Unrescued(format!("{e:#}")));
    }
    // `Instance.searchable.domain_starts_with(domain)`. The `instances`
    // materialized view is the union of the domains accounts are on and those
    // a block or allow names; this reads the same union, so that a view nobody
    // has refreshed cannot hide a domain.
    let pattern = format!("{}%", crate::search::sanitize_sql_like(&domain));
    Ok(Some(
        sqlx::query_scalar::<_, String>(
            "SELECT domain FROM ( \
               SELECT DISTINCT domain FROM accounts WHERE domain IS NOT NULL \
               UNION SELECT domain FROM domain_allows \
             ) AS instances \
             WHERE domain NOT IN (SELECT domain FROM domain_blocks) \
               AND domain LIKE $1 \
             LIMIT $2",
        )
        .bind(pattern)
        .bind(LIMIT)
        .fetch_all(&state.db)
        .await?,
    ))
}
