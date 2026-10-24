//! Literal substring post search when Elasticsearch is disabled.
//!
//! Mastodon: app/services/statuses_search_service.rb

use super::elasticsearch::StatusOptions;
use crate::{error::AppResult, state::AppState};

pub async fn search(
    state: &AppState,
    query: &str,
    viewer: i64,
    options: &StatusOptions,
) -> AppResult<Vec<i64>> {
    if let Some(account_id) = options.account_id {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM accounts WHERE id = $1)")
                .bind(account_id)
                .fetch_one(&state.db)
                .await?;
        if !exists {
            return Err(crate::error::AppError::NotFound);
        }
    }
    let pattern = format!("%{}%", super::sanitize_sql_like(query));
    Ok(sqlx::query_scalar(
        "SELECT s.id FROM statuses s JOIN accounts a ON a.id = s.account_id \
         WHERE (coalesce(s.text, '') ILIKE $1 OR coalesce(s.spoiler_text, '') ILIKE $1) AND s.deleted_at IS NULL AND s.reblog_of_id IS NULL \
           AND (s.account_id = $2 \
             OR EXISTS (SELECT 1 FROM mentions m WHERE m.status_id = s.id AND m.account_id = $2 AND NOT m.silent) \
             OR EXISTS (SELECT 1 FROM favourites f WHERE f.status_id = s.id AND f.account_id = $2) \
             OR EXISTS (SELECT 1 FROM bookmarks b WHERE b.status_id = s.id AND b.account_id = $2) \
             OR EXISTS (SELECT 1 FROM statuses r WHERE r.reblog_of_id = s.id AND r.account_id = $2 AND r.deleted_at IS NULL) \
             OR EXISTS (SELECT 1 FROM poll_votes v WHERE v.poll_id = s.poll_id AND v.account_id = $2) OR (s.visibility = 0 AND a.indexable)) \
           AND (s.account_id = $2 OR ( \
             a.suspended_at IS NULL AND a.requested_deletion_at IS NULL \
             AND (CASE \
               WHEN s.visibility IN (3, 4) THEN \
                 EXISTS (SELECT 1 FROM mentions m WHERE m.status_id = s.id AND m.account_id = $2) \
               WHEN s.visibility = 2 THEN \
                 EXISTS (SELECT 1 FROM follows f WHERE f.account_id = $2 AND f.target_account_id = s.account_id) \
                 OR EXISTS (SELECT 1 FROM mentions m WHERE m.status_id = s.id AND m.account_id = $2) \
               ELSE NOT EXISTS (SELECT 1 FROM blocks b WHERE b.account_id = s.account_id AND b.target_account_id = $2) \
             END) \
             AND NOT EXISTS (SELECT 1 FROM blocks b WHERE b.account_id = $2 AND b.target_account_id = s.account_id) \
             AND NOT (a.domain IS NOT NULL AND EXISTS ( \
               SELECT 1 FROM account_domain_blocks d WHERE d.account_id = $2 AND d.domain = a.domain)) \
             AND NOT EXISTS (SELECT 1 FROM mutes mu WHERE mu.account_id = $2 AND mu.target_account_id = s.account_id) \
             AND NOT (a.silenced_at IS NOT NULL AND NOT EXISTS ( \
               SELECT 1 FROM follows f WHERE f.account_id = $2 AND f.target_account_id = s.account_id)) \
           )) AND ($3::bigint IS NULL OR s.account_id = $3) \
           AND ($4::bigint IS NULL OR s.id > $4) \
           AND ($5::bigint IS NULL OR s.id < $5) \
         ORDER BY s.id DESC LIMIT $6 OFFSET $7",
    )
    .bind(pattern).bind(viewer).bind(options.account_id)
    .bind(options.min_id).bind(options.max_id).bind(options.limit).bind(options.offset)
    .fetch_all(&state.db).await?)
}
