//! Account search: fuzzy (`q`) and advanced (name/prefix) local search,
//! plus WebFinger-based remote resolution for exact `user@host` queries.

use super::*;

// Mastodon's relevance BOOST: reputation (followers per following) + follower
// volume + recency of the last status, averaged. Reads from `account_stats s`.
fn account_boost_expr() -> String {
    let reputation = "(greatest(0, coalesce(s.followers_count, 0)) / (greatest(0, coalesce(s.following_count, 0)) + 1.0))";
    let followers = "log(greatest(0, coalesce(s.followers_count, 0)) + 2)";
    let time_distance = "(case when s.last_status_at is null then 0 else exp(-1.0 * ((greatest(0, abs(extract(DAY FROM age(s.last_status_at))) - 30.0)^2) / (2.0 * ((-1.0 * 30^2) / (2.0 * ln(0.3)))))) end)";
    format!("(({reputation} + {followers} + {time_distance}) / 3.0)")
}

// Mastodon's `generate_query_for_search`: strip tsquery metacharacters, then
// wrap the terms as a prefix query (`' terms ':*`).
fn account_tsquery(terms: &str) -> String {
    let sanitized: String = terms
        .chars()
        .map(|c| match c {
            '\'' | '?' | '\\' | ':' | '\u{2018}' | '\u{2019}' => ' ',
            other => other,
        })
        .collect();
    format!("' {sanitized} ':*")
}

// Unauthenticated ranking (Mastodon's BASIC_SEARCH_SQL): weighted prefix match
// ordered by relevance BOOST.
async fn simple_account_search(
    state: &AppState,
    tsquery: &str,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<Account>> {
    let ranks = ACCOUNT_TEXT_SEARCH_RANKS;
    let boost = account_boost_expr();
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
        .bind(tsquery)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await?)
}

// Authenticated ranking (Mastodon's ADVANCED_SEARCH_*). `following` restricts
// to the viewer's follows; otherwise accounts in the viewer's follow graph are
// boosted to the top.
async fn advanced_account_search(
    state: &AppState,
    tsquery: &str,
    viewer_id: i64,
    following: bool,
    limit: i64,
    offset: i64,
) -> AppResult<Vec<Account>> {
    let ranks = ACCOUNT_TEXT_SEARCH_RANKS;
    let boost = account_boost_expr();
    let rank_expr = format!("{boost} * ts_rank_cd({ranks}, to_tsquery('simple', $1), 32)");
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
             ORDER BY ((count(f.id) + 1) * {rank_expr}) DESC \
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
             ORDER BY count(f.id) DESC, ({rank_expr}) DESC \
             LIMIT $3 OFFSET $4"
        )
    };
    Ok(sqlx::query_as::<_, Account>(&sql)
        .bind(tsquery)
        .bind(viewer_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&state.db)
        .await?)
}

async fn find_local_account(state: &AppState, username: &str) -> AppResult<Option<Account>> {
    Ok(sqlx::query_as::<_, Account>(
        "SELECT * FROM accounts WHERE lower(username) = lower($1) AND domain IS NULL AND suspended_at IS NULL AND requested_deletion_at IS NULL LIMIT 1",
    )
    .bind(username)
    .fetch_optional(&state.db)
    .await?)
}

async fn find_remote_account(
    state: &AppState,
    username: &str,
    domain: &str,
) -> AppResult<Option<Account>> {
    Ok(sqlx::query_as::<_, Account>(
        "SELECT * FROM accounts WHERE lower(username) = lower($1) AND lower(domain) = lower($2) AND suspended_at IS NULL AND requested_deletion_at IS NULL LIMIT 1",
    )
    .bind(username)
    .bind(domain)
    .fetch_optional(&state.db)
    .await?)
}

async fn account_is_following(
    state: &AppState,
    account_id: i64,
    target_id: i64,
) -> AppResult<bool> {
    Ok(
        sqlx::query("SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2")
            .bind(account_id)
            .bind(target_id)
            .fetch_optional(&state.db)
            .await?
            .is_some(),
    )
}

// Mastodon's exact-match resolution for a full `user@domain`: return the local
// row if known, otherwise WebFinger the remote actor and fetch it in.
async fn resolve_remote_exact(
    state: &AppState,
    username: &str,
    domain: &str,
) -> AppResult<Option<Account>> {
    let username = username.to_lowercase();
    let domain = domain.to_lowercase();
    if let Some(existing) = find_remote_account(state, &username, &domain).await? {
        return Ok(Some(existing));
    }

    let Ok(uri) = crate::federation::webfinger::resolve(&state.fetcher, &username, &domain).await
    else {
        return Ok(None);
    };
    let Ok(account_id) = crate::api::ap::inbox::resolve_or_fetch_remote_account(state, &uri).await
    else {
        return Ok(None);
    };
    Ok(
        sqlx::query_as::<_, Account>("SELECT * FROM accounts WHERE id = $1")
            .bind(account_id)
            .fetch_optional(&state.db)
            .await?,
    )
}

pub async fn search_accounts(
    state: AppState,
    Query(q): Query<AccountSearchQuery>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Vec<ApiAccount>>> {
    let limit = q.limit.unwrap_or(40).clamp(1, 80);
    let offset = q.offset.unwrap_or(0).max(0);
    // Mastodon strips a leading '@' from the query, so "@alice" matches "alice".
    let query = q.q.trim().trim_start_matches('@').to_string();
    if query.is_empty() || limit < 1 {
        return Ok(Json(vec![]));
    }

    let resolve = q.resolve.unwrap_or(false);
    let following = q.following.unwrap_or(false);
    let viewer_id = auth.as_ref().map(|Extension(a)| a.account_id);

    let (username_part, domain_part) = match query.split_once('@') {
        Some((u, d)) => (u.to_string(), Some(d.to_string())),
        None => (query.clone(), None),
    };
    let domain_is_local = domain_part
        .as_ref()
        .map(|d| d.eq_ignore_ascii_case(&state.instance.domain))
        .unwrap_or(true);

    let mut results: Vec<Account> = Vec::new();

    // Exact match, first page only, for a complete "user@domain" handle
    // (Mastodon's `username_complete?`). A local domain resolves locally; a
    // remote one is fetched via WebFinger when `resolve` is set.
    if offset == 0 && query.contains('@') {
        let exact = if domain_is_local {
            find_local_account(&state, &username_part).await?
        } else if let Some(domain) = domain_part.as_deref() {
            if resolve {
                resolve_remote_exact(&state, &username_part, domain).await?
            } else {
                find_remote_account(&state, &username_part, domain).await?
            }
        } else {
            None
        };
        if let Some(acc) = exact {
            // Drop the exact match if a `following` filter excludes it.
            let keep = !following
                || match viewer_id {
                    Some(vid) => account_is_following(&state, vid, acc.id).await?,
                    None => true,
                };
            if keep {
                results.push(acc);
            }
        }
    }

    // Non-exact ranked results. Unauthenticated searches require a minimum
    // query length (Mastodon's MIN_QUERY_LENGTH gate).
    let min_len_ok = viewer_id.is_some() || query.chars().count() >= MIN_ACCOUNT_QUERY_LENGTH;
    let remaining = limit - results.len() as i64;
    if min_len_ok && remaining > 0 {
        // A local (or bare) handle searches on the username alone; a remote one
        // keeps the full "user@domain".
        let terms = if domain_is_local {
            &username_part
        } else {
            &query
        };
        let tsquery = account_tsquery(terms);
        let ranked = match viewer_id {
            Some(vid) => {
                advanced_account_search(&state, &tsquery, vid, following, remaining, offset).await?
            }
            None => simple_account_search(&state, &tsquery, remaining, offset).await?,
        };
        for acc in ranked {
            if !results.iter().any(|a| a.id == acc.id) {
                results.push(acc);
            }
        }
    }

    Ok(Json(batch_accounts_to_api(&state, &results).await))
}
