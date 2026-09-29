use axum::{
    extract::{Extension, Query},
    Json,
};
use serde::Deserialize;

use super::{
    accounts::{batch_account_emojis, batch_account_roles, batch_accounts_to_api},
    convert::status_from_db,
    status_serialize::{
        batch_reblog_data, batch_status_cards, batch_status_emojis, batch_status_media,
        batch_status_mentions, batch_status_polls, batch_statuses_tags, build_status,
        fetch_reblog_data, fetch_status_media, hydrate_status_stats,
    },
    types::{SearchResults, Status, Tag},
};
use crate::{
    api::mastodon::resolve_url::Resolved,
    error::AppResult,
    middleware::{AuthenticatedUser, ResolvedInstance},
    state::AppState,
};

// ── GET /api/v2/search ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(rename = "type")]
    pub search_type: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub resolve: Option<bool>,
    pub following: Option<bool>,
    pub account_id: Option<String>,
    pub exclude_unreviewed: Option<bool>,
}

pub async fn search(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Query(q): Query<SearchQuery>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<SearchResults>> {
    let limit = q.limit.unwrap_or(20).clamp(1, 40);
    let query = q.q.trim();
    let account_pattern = format!("%{}%", q.q.to_lowercase());
    let search_type = q.search_type.as_deref();
    let viewer_id = auth.as_ref().map(|Extension(a)| a.account_id);

    // Mastodon restricts anonymous search: pagination and remote resolution both
    // require authentication (SearchController#query_pagination_error /
    // #remote_resolve_error).
    if viewer_id.is_none() {
        if q.offset.is_some() {
            return Err(crate::error::AppError::UnauthorizedMsg(
                "Search queries pagination is not supported without authentication".into(),
            ));
        }
        if q.resolve == Some(true) {
            return Err(crate::error::AppError::UnauthorizedMsg(
                "Search queries that resolve remote resources are not supported without authentication".into(),
            ));
        }
    }

    let offset = q.offset.unwrap_or(0).max(0);

    // `SearchService#url_query?`: a query that looks like a URL is resolved
    // rather than searched, and only when the caller asked to resolve — which
    // is why this sits above every other kind of search and returns whatever it
    // finds on its own. `ResolveURLService` fetches the URL, following the
    // `rel="alternate"` link when what answers is a page rather than an object,
    // so a server that serves its objects from a different path than its pages
    // resolves like any other.
    if q.resolve.unwrap_or(false) && (query.starts_with("http://") || query.starts_with("https://"))
    {
        let mut results = SearchResults {
            accounts: vec![],
            statuses: vec![],
            hashtags: vec![],
            collections: vec![],
        };
        // `@offset` is zero unless a type was named, and a resolved resource is
        // dropped once the caller has paged past it.
        let url_offset = if search_type.is_none() { 0 } else { offset };
        if url_offset == 0 {
            match crate::api::mastodon::resolve_url::resolve_url(&state, query, viewer_id).await? {
                Some(Resolved::Status(id))
                    if search_type.is_none() || search_type == Some("statuses") =>
                {
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
                    let media = fetch_status_media(&state, s.id).await?;
                    let reblog = fetch_reblog_data(&state, &s).await?;
                    // Compute the viewer's context so quote_approval / interaction
                    // flags reflect the requester rather than defaulting to unknown.
                    let ctx = if let Some(vid) = viewer_id {
                        super::statuses::batch_viewer_contexts(&state, vid, &[s.id])
                            .await?
                            .remove(&s.id)
                    } else {
                        None
                    };
                    results.statuses =
                        vec![build_status(&state, &s, &account, media, reblog, ctx).await?];
                }
                Some(Resolved::Account(id))
                    if search_type.is_none() || search_type == Some("accounts") =>
                {
                    let account = sqlx::query_as!(
                        crate::db::models::Account,
                        "SELECT * FROM accounts WHERE id = $1",
                        id,
                    )
                    .fetch_one(&state.db)
                    .await?;
                    results.accounts = batch_accounts_to_api(&state, &[account]).await;
                }
                _ => {}
            }
        }
        return Ok(Json(results));
    }

    // Detect @user@domain or user@domain handle patterns for exact acct lookup
    let handle_parts: Option<(String, Option<String>)> = {
        let trimmed = q.q.trim().trim_start_matches('@');
        if trimmed.contains('@') {
            let mut parts = trimmed.splitn(2, '@');
            let user = parts.next().unwrap_or("").to_lowercase();
            let dom = parts.next().map(|d| d.to_lowercase());
            if !user.is_empty() {
                Some((user, dom))
            } else {
                None
            }
        } else {
            None
        }
    };

    let db_accounts: Vec<crate::db::models::Account> = if search_type.is_none()
        || search_type == Some("accounts")
    {
        let following_filter = q.following.unwrap_or(false);

        // If query is a handle (user@domain), do an exact acct lookup first
        if let Some((ref uname, ref domain)) = handle_parts {
            let exact: Vec<crate::db::models::Account> = if let Some(dom) = domain {
                sqlx::query_as!(
                    crate::db::models::Account,
                    r#"SELECT * FROM accounts
                       WHERE suspended_at IS NULL AND requested_deletion_at IS NULL
                         AND lower(username) = $1 AND lower(domain) = $2
                       LIMIT $3"#,
                    uname,
                    dom,
                    limit
                )
                .fetch_all(&state.db)
                .await?
            } else {
                sqlx::query_as!(
                    crate::db::models::Account,
                    r#"SELECT * FROM accounts
                       WHERE suspended_at IS NULL AND requested_deletion_at IS NULL
                         AND lower(username) = $1 AND domain IS NULL
                       LIMIT $2"#,
                    uname,
                    limit
                )
                .fetch_all(&state.db)
                .await?
            };
            if !exact.is_empty() {
                exact
            } else {
                // resolve=true with a full user@domain: fetch via WebFinger
                if q.resolve.unwrap_or(false) {
                    if let Some(dom) = domain {
                        if let Ok(actor_url) =
                            crate::federation::webfinger::resolve(&state.fetcher, uname, dom).await
                        {
                            if let Ok(account_id) =
                                crate::api::ap::inbox::resolve_or_fetch_remote_account(
                                    &state, &actor_url,
                                )
                                .await
                            {
                                if let Ok(Some(account)) = sqlx::query_as!(
                                    crate::db::models::Account,
                                    "SELECT * FROM accounts WHERE id = $1",
                                    account_id,
                                )
                                .fetch_optional(&state.db)
                                .await
                                {
                                    let api_accounts =
                                        batch_accounts_to_api(&state, &[account]).await;
                                    return Ok(Json(SearchResults {
                                        accounts: api_accounts,
                                        statuses: vec![],
                                        hashtags: vec![],
                                        collections: vec![],
                                    }));
                                }
                            }
                        }
                    }
                }
                if following_filter {
                    let vid = viewer_id.ok_or(crate::error::AppError::Unauthorized)?;
                    sqlx::query_as!(
                        crate::db::models::Account,
                        r#"SELECT a.* FROM accounts a
                           JOIN follows f ON f.target_account_id = a.id
                           LEFT JOIN account_stats ast ON ast.account_id = a.id
                           WHERE a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                             AND a.moved_to_account_id IS NULL
                             AND f.account_id = $3
                             AND (lower(a.username) LIKE $1 OR lower(a.display_name) LIKE $1)
                           ORDER BY COALESCE(ast.followers_count, 0) DESC LIMIT $2 OFFSET $4"#,
                        account_pattern,
                        limit,
                        vid,
                        offset
                    )
                    .fetch_all(&state.db)
                    .await?
                } else {
                    sqlx::query_as!(
                        crate::db::models::Account,
                        r#"SELECT a.* FROM accounts a
                           LEFT JOIN account_stats ast ON ast.account_id = a.id
                           WHERE a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                             AND a.moved_to_account_id IS NULL
                             AND (lower(a.username) LIKE $1 OR lower(a.display_name) LIKE $1)
                           ORDER BY COALESCE(ast.followers_count, 0) DESC LIMIT $2 OFFSET $3"#,
                        account_pattern,
                        limit,
                        offset
                    )
                    .fetch_all(&state.db)
                    .await?
                }
            }
        } else if following_filter {
            let vid = viewer_id.ok_or(crate::error::AppError::Unauthorized)?;
            sqlx::query_as!(
                crate::db::models::Account,
                r#"SELECT a.* FROM accounts a
                   JOIN follows f ON f.target_account_id = a.id
                   LEFT JOIN account_stats ast ON ast.account_id = a.id
                   WHERE a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                     AND a.moved_to_account_id IS NULL
                     AND f.account_id = $3
                     AND (lower(a.username) LIKE $1 OR lower(a.display_name) LIKE $1)
                   ORDER BY COALESCE(ast.followers_count, 0) DESC LIMIT $2 OFFSET $4"#,
                account_pattern,
                limit,
                vid,
                offset
            )
            .fetch_all(&state.db)
            .await?
        } else {
            sqlx::query_as!(
                crate::db::models::Account,
                r#"SELECT a.* FROM accounts a
                   LEFT JOIN account_stats ast ON ast.account_id = a.id
                   WHERE a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                     AND a.moved_to_account_id IS NULL
                     AND (lower(a.username) LIKE $1 OR lower(a.display_name) LIKE $1)
                   ORDER BY COALESCE(ast.followers_count, 0) DESC LIMIT $2 OFFSET $3"#,
                account_pattern,
                limit,
                offset
            )
            .fetch_all(&state.db)
            .await?
        }
    } else {
        vec![]
    };
    let accounts = batch_accounts_to_api(&state, &db_accounts).await;

    let statuses = if (search_type.is_none() || search_type == Some("statuses")) && auth.is_some() {
        let fts_query = q.q.trim().to_string();
        let filter_account_id: Option<i64> = q.account_id.as_deref().and_then(|s| s.parse().ok());
        let rows = sqlx::query_as!(
            crate::db::models::Status,
            r#"SELECT s.* FROM statuses s
               JOIN accounts a ON a.id = s.account_id
               WHERE s.deleted_at IS NULL
                 AND (s.visibility IN (0, 1) OR s.account_id = $4)
                 AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                 AND (a.domain IS NULL OR NOT EXISTS (
                     SELECT 1 FROM domain_blocks db WHERE db.domain = a.domain
                 ))
                 AND ($3::bigint IS NULL OR s.account_id = $3)
                 AND ($4::bigint IS NULL OR NOT EXISTS (
                     SELECT 1 FROM blocks b
                     WHERE (b.account_id = $4 AND b.target_account_id = s.account_id)
                        OR (b.account_id = s.account_id AND b.target_account_id = $4)
                 ))
                 AND ($4::bigint IS NULL OR NOT EXISTS (
                     SELECT 1 FROM mutes mu
                     WHERE mu.account_id = $4 AND mu.target_account_id = s.account_id
                       AND (mu.expires_at IS NULL OR mu.expires_at > now())
                 ) OR EXISTS (
                     -- Mute exemption: a post that mentions me.
                     SELECT 1 FROM mentions mn
                     WHERE mn.status_id = s.id AND mn.account_id = $4 AND NOT mn.silent
                 ) OR EXISTS (
                     -- Mute exemption: a quote of a post of mine.
                     SELECT 1 FROM quotes q
                     WHERE q.status_id = s.id AND q.quoted_account_id = $4
                 ))
                 AND to_tsvector('simple', coalesce(s.text, ''))
                     @@ websearch_to_tsquery('simple', $1)
               ORDER BY s.id DESC LIMIT $2 OFFSET $5"#,
            fts_query,
            limit,
            filter_account_id,
            viewer_id,
            offset
        )
        .fetch_all(&state.db)
        .await?;

        let all_ids: Vec<i64> = rows.iter().map(|s| s.id).collect();
        let media_map = batch_status_media(&state, &all_ids).await?;
        let reblog_map = batch_reblog_data(&state, &rows).await?;
        let reblog_ids: Vec<i64> = reblog_map.values().map(|(rs, _, _)| rs.id).collect();
        let mut enrich_ids = all_ids.clone();
        enrich_ids.extend_from_slice(&reblog_ids);
        let tags_map = batch_statuses_tags(&state, &enrich_ids).await?;
        let mentions_map = batch_status_mentions(&state, &enrich_ids).await?;
        let all_statuses_for_emoji: Vec<crate::db::models::Status> = rows
            .iter()
            .cloned()
            .chain(reblog_map.values().map(|(rs, _, _)| rs.clone()))
            .collect();
        let emojis_map = batch_status_emojis(&state, &all_statuses_for_emoji).await?;
        let polls_map = batch_status_polls(&state, &enrich_ids, viewer_id).await?;
        let cards_map = batch_status_cards(&state, &enrich_ids).await?;
        let ctxs = if let Some(vid) = viewer_id {
            super::statuses::batch_viewer_contexts(&state, vid, &all_ids).await?
        } else {
            std::collections::HashMap::new()
        };
        let account_ids: Vec<i64> = rows
            .iter()
            .map(|s| s.account_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let accounts: Vec<crate::db::models::Account> = sqlx::query_as!(
            crate::db::models::Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &account_ids,
        )
        .fetch_all(&state.db)
        .await?;
        let account_map: std::collections::HashMap<i64, crate::db::models::Account> =
            accounts.into_iter().map(|a| (a.id, a)).collect();
        let all_accounts_for_emoji: Vec<crate::db::models::Account> = {
            let mut seen = std::collections::HashSet::new();
            account_map
                .values()
                .chain(reblog_map.values().map(|(_, ra, _)| ra))
                .filter(|a| seen.insert(a.id))
                .cloned()
                .collect()
        };
        let account_emojis_map = batch_account_emojis(&state, &all_accounts_for_emoji).await;
        let account_roles_map = batch_account_roles(&state, &all_accounts_for_emoji).await;

        let mut result: Vec<Status> = Vec::with_capacity(rows.len());
        for s in &rows {
            let Some(account) = account_map.get(&s.account_id) else {
                continue;
            };
            let media = media_map.get(&s.id).cloned().unwrap_or_default();
            let reblog = reblog_map.get(&s.id).cloned();
            let mentions = mentions_map.get(&s.id).cloned().unwrap_or_default();
            let rb_mentions = reblog
                .as_ref()
                .and_then(|(rs, _, _)| mentions_map.get(&rs.id))
                .cloned()
                .unwrap_or_default();
            let ctx = ctxs.get(&s.id).cloned();
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
        hydrate_status_stats(&state, result.iter_mut()).await;
        result
    } else {
        vec![]
    };

    let hashtags = if search_type.is_none() || search_type == Some("hashtags") {
        // Strip leading # and use prefix match (Mastodon's matches_name scope)
        let tag_term = q.q.trim().trim_start_matches('#').to_lowercase();
        let tag_prefix = format!("{}%", tag_term);
        let exclude_unreviewed = q.exclude_unreviewed.unwrap_or(false);
        sqlx::query!(
            r#"SELECT id, name FROM tags
               WHERE lower(name) LIKE $1
                 AND (listable IS NULL OR listable = true)
                 AND (NOT $4::boolean OR reviewed_at IS NOT NULL OR lower(name) = $5)
               ORDER BY LENGTH(name) ASC, name ASC
               LIMIT $2 OFFSET $3"#,
            tag_prefix,
            limit,
            offset,
            exclude_unreviewed,
            tag_term
        )
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .map(|r| Tag {
            id: r.id.to_string(),
            name: r.name.clone(),
            url: format!("https://{}/tags/{}", instance.domain, r.name),
            history: vec![],
            following: None,
            featuring: None,
        })
        .collect()
    } else {
        vec![]
    };

    Ok(Json(SearchResults {
        accounts,
        statuses,
        hashtags,
        collections: vec![],
    }))
}
