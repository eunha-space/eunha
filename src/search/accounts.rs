//! Mastodon's `AccountSearchService`: an exact match for a complete
//! `user@domain`, then ranked results — from Elasticsearch when the instance
//! has it, from PostgreSQL's text search (`Account.search_for` and
//! `Account.advanced_search_for`) otherwise, or when Elasticsearch fails.

use std::sync::LazyLock;

use crate::db::models::Account;
use crate::error::AppResult;
use crate::state::AppState;

/// `MIN_QUERY_LENGTH`: an anonymous search shorter than this gets the exact
/// match or nothing.
pub const MIN_QUERY_LENGTH: usize = 3;

/// The weighted document Mastodon ranks against: display name (A), username
/// (B), domain (C). `Account::Search::TEXT_SEARCH_RANKS`.
const TEXT_SEARCH_RANKS: &str = "(setweight(to_tsvector('simple', accounts.display_name), 'A') || setweight(to_tsvector('simple', accounts.username), 'B') || setweight(to_tsvector('simple', coalesce(accounts.domain, '')), 'C'))";

/// `"@#{query}".match?(MENTION_ONLY_RE)`, the `@` already taken off: a
/// username, optionally followed by `@` and a domain.
static MENTION_ONLY: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"(?i)^[a-z0-9_]+(?:[.-]+[a-z0-9_]+)*(?:@\w+(?:[.-]+\w+)*)?$").unwrap()
});

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub limit: i64,
    pub offset: i64,
    pub resolve: bool,
    pub following: bool,
    /// `use_searchable_text`: `/api/v2/search` also matches the text of a
    /// discoverable account's bio. Only Elasticsearch reads it.
    pub use_searchable_text: bool,
}

/// `AccountSearchService#call`.
pub async fn search(
    state: &AppState,
    query: &str,
    viewer: Option<i64>,
    options: &Options,
) -> AppResult<Vec<Account>> {
    // `query&.strip&.gsub(/\A@/, '')`: one leading `@`.
    let query = query.trim();
    let query = query.strip_prefix('@').unwrap_or(query);
    if query.is_empty() || options.limit < 1 {
        return Ok(vec![]);
    }

    // `query.split('@')`, which in Ruby drops trailing empty parts.
    let mut parts: Vec<&str> = query.split('@').collect();
    while parts.last() == Some(&"") {
        parts.pop();
    }
    let username = parts.first().copied().unwrap_or("").to_string();
    // `query_domain`: the last part, when there is more than one.
    let domain = (parts.len() > 1).then(|| parts[parts.len() - 1].to_string());
    let domain_is_local = domain
        .as_deref()
        .is_none_or(|d| crate::search::is_local_domain(state, d));

    let mut results: Vec<Account> = Vec::new();

    // `exact_match`: the first page of a complete handle.
    if options.offset == 0 && query.contains('@') && MENTION_ONLY.is_match(query) {
        let found = if options.resolve {
            resolve(state, &username, domain.as_deref(), domain_is_local).await?
        } else if domain_is_local {
            find_remote(state, &username, None).await?
        } else {
            find_remote(state, &username, domain.as_deref()).await?
        };
        if let Some(account) = found {
            let keep = match viewer {
                Some(viewer) if options.following => following(state, viewer, account.id).await?,
                _ => true,
            };
            if keep {
                results.push(account);
            }
        }
    }

    // `limit_for_non_exact_results`.
    let remaining = if viewer.is_none() && query.chars().count() < MIN_QUERY_LENGTH {
        0
    } else {
        options.limit - results.len() as i64
    };
    if remaining > 0 {
        // `terms_for_query`: a local handle searches on the username alone.
        let terms = if domain_is_local { &username } else { query };
        let from_elasticsearch =
            crate::search::elasticsearch::accounts(state, terms, viewer, options, remaining).await;
        let ranked = match (from_elasticsearch, viewer) {
            (Some(found), _) => found,
            (None, Some(viewer)) => {
                advanced_search_for(
                    state,
                    terms,
                    viewer,
                    options.following,
                    remaining,
                    options.offset,
                )
                .await?
            }
            (None, None) => search_for(state, terms, remaining, options.offset).await?,
        };
        for account in ranked {
            if !results.iter().any(|a| a.id == account.id) {
                results.push(account);
            }
        }
    }
    Ok(results)
}

/// `ResolveAccountService#call` for the exact match: the local account for a
/// local handle, a known remote one, or one WebFinger finds.
async fn resolve(
    state: &AppState,
    username: &str,
    domain: Option<&str>,
    domain_is_local: bool,
) -> AppResult<Option<Account>> {
    let domain = match domain {
        Some(domain) if !domain_is_local => domain,
        _ => return find_remote(state, username, None).await,
    };
    if crate::federation::moderation::domain_not_allowed(state, domain).await {
        return Ok(None);
    }
    if let Some(known) = find_remote(state, username, Some(domain)).await? {
        return Ok(Some(known));
    }
    let Ok(uri) = crate::federation::webfinger::resolve(&state.fetcher, username, domain).await
    else {
        return Ok(None);
    };
    let Ok(id) = crate::api::ap::inbox::resolve_or_fetch_remote_account(state, &uri).await else {
        return Ok(None);
    };
    Ok(
        sqlx::query_as::<_, Account>("SELECT * FROM accounts WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?,
    )
}

/// `Account.find_remote` (and `find_local`, with no domain): by username and
/// domain, case-insensitively, whatever state the account is in — a
/// suspended account is still the one the handle names.
pub async fn find_remote(
    state: &AppState,
    username: &str,
    domain: Option<&str>,
) -> AppResult<Option<Account>> {
    if domain == Some("handle.invalid") {
        return Ok(None);
    }
    Ok(sqlx::query_as::<_, Account>(
        "SELECT * FROM accounts \
         WHERE lower(username) = lower($1) \
           AND (CASE WHEN $2::text IS NULL THEN domain IS NULL ELSE lower(domain) = lower($2) END) \
         ORDER BY id ASC LIMIT 1",
    )
    .bind(username)
    .bind(domain)
    .fetch_optional(&state.db)
    .await?)
}

async fn following(state: &AppState, account_id: i64, target_id: i64) -> AppResult<bool> {
    Ok(
        sqlx::query("SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2")
            .bind(account_id)
            .bind(target_id)
            .fetch_optional(&state.db)
            .await?
            .is_some(),
    )
}

/// `generate_query_for_search`: the characters `to_tsquery` would choke on
/// become spaces, and the terms a prefix query.
pub fn tsquery(terms: &str) -> String {
    let sanitized: String = terms
        .chars()
        .map(|c| match c {
            '\'' | '?' | '\\' | ':' | '\u{2018}' | '\u{2019}' => ' ',
            other => other,
        })
        .collect();
    format!("' {sanitized} ':*")
}

/// `Account::Search::BOOST`: reputation, follower volume and how recently the
/// account posted, averaged.
fn boost() -> String {
    let reputation = "(greatest(0, coalesce(s.followers_count, 0)) / (greatest(0, coalesce(s.following_count, 0)) + 1.0))";
    let followers = "log(greatest(0, coalesce(s.followers_count, 0)) + 2)";
    let time_distance = "(case when s.last_status_at is null then 0 else exp(-1.0 * ((greatest(0, abs(extract(DAY FROM age(s.last_status_at))) - 30.0)^2) / (2.0 * ((-1.0 * 30^2) / (2.0 * ln(0.3)))))) end)";
    format!("(({reputation} + {followers} + {time_distance}) / 3.0)")
}

/// `Account.search_for`: `BASIC_SEARCH_SQL`.
pub async fn search_for(
    state: &AppState,
    terms: &str,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<Account>> {
    let ranks = TEXT_SEARCH_RANKS;
    let boost = boost();
    let sql = format!(
        "SELECT accounts.* FROM accounts \
         LEFT JOIN users ON accounts.id = users.account_id \
         LEFT JOIN account_stats AS s ON accounts.id = s.account_id \
         WHERE to_tsquery('simple', $1) @@ {ranks} \
           AND accounts.suspended_at IS NULL AND accounts.requested_deletion_at IS NULL \
           AND accounts.moved_to_account_id IS NULL \
           AND (accounts.domain IS NOT NULL OR (users.approved = TRUE AND users.confirmed_at IS NOT NULL)) \
         ORDER BY ({boost} * ts_rank_cd({ranks}, to_tsquery('simple', $1), 32)) DESC \
         LIMIT $2 OFFSET $3"
    );
    Ok(sqlx::query_as::<_, Account>(&sql)
        .bind(tsquery(terms))
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await?)
}

/// `Account.advanced_search_for`: `ADVANCED_SEARCH_WITH_FOLLOWING` limits the
/// results to who the viewer follows; `ADVANCED_SEARCH_WITHOUT_FOLLOWING`
/// puts the accounts either side of a follow with the viewer first.
pub async fn advanced_search_for(
    state: &AppState,
    terms: &str,
    viewer: i64,
    following: bool,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<Account>> {
    let ranks = TEXT_SEARCH_RANKS;
    let boost = boost();
    let rank = format!("{boost} * ts_rank_cd({ranks}, to_tsquery('simple', $1), 32)");
    let sql = if following {
        format!(
            "WITH first_degree AS (\
                 SELECT target_account_id FROM follows WHERE account_id = $2 \
                 UNION ALL SELECT $2\
             ) \
             SELECT accounts.* FROM accounts \
             LEFT OUTER JOIN follows AS f ON (accounts.id = f.account_id AND f.target_account_id = $2) \
             LEFT JOIN account_stats AS s ON accounts.id = s.account_id \
             WHERE accounts.id IN (SELECT * FROM first_degree) \
               AND to_tsquery('simple', $1) @@ {ranks} \
               AND accounts.suspended_at IS NULL AND accounts.requested_deletion_at IS NULL \
               AND accounts.moved_to_account_id IS NULL \
             GROUP BY accounts.id, s.id \
             ORDER BY ((count(f.id) + 1) * {rank}) DESC \
             LIMIT $3 OFFSET $4"
        )
    } else {
        format!(
            "SELECT accounts.* FROM accounts \
             LEFT OUTER JOIN follows AS f ON \
               (accounts.id = f.account_id AND f.target_account_id = $2) OR (accounts.id = f.target_account_id AND f.account_id = $2) \
             LEFT JOIN users ON accounts.id = users.account_id \
             LEFT JOIN account_stats AS s ON accounts.id = s.account_id \
             WHERE to_tsquery('simple', $1) @@ {ranks} \
               AND accounts.suspended_at IS NULL AND accounts.requested_deletion_at IS NULL \
               AND accounts.moved_to_account_id IS NULL \
               AND (accounts.domain IS NOT NULL OR (users.approved = TRUE AND users.confirmed_at IS NOT NULL)) \
             GROUP BY accounts.id, s.id \
             ORDER BY count(f.id) DESC, ({rank}) DESC \
             LIMIT $3 OFFSET $4"
        )
    };
    Ok(sqlx::query_as::<_, Account>(&sql)
        .bind(tsquery(terms))
        .bind(viewer)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_handles_are_recognised() {
        assert!(MENTION_ONLY.is_match("alice@example.com"));
        assert!(MENTION_ONLY.is_match("a.b-c_d@sub.example.com"));
        assert!(MENTION_ONLY.is_match("alice"));
        assert!(!MENTION_ONLY.is_match("alice@"));
        assert!(!MENTION_ONLY.is_match("al ice@example.com"));
        assert!(!MENTION_ONLY.is_match("alice@example.com@x"));
    }

    #[test]
    fn tsquery_matches_generate_query_for_search() {
        assert_eq!(tsquery("alice"), "' alice ':*");
        assert_eq!(tsquery("o'brien: x?"), "' o brien  x  ':*");
    }
}
