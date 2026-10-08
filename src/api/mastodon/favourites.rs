use axum::{
    extract::{Extension, Query},
    http::{HeaderMap, Uri},
    response::IntoResponse,
    Json,
};

use super::{
    accounts::{batch_account_emojis, batch_account_roles},
    convert::status_from_db,
    status_serialize::{
        batch_reblog_data, batch_status_cards, batch_status_emojis, batch_status_media,
        batch_status_mentions, batch_status_polls, batch_statuses_tags, hydrate_status_stats,
    },
    types::PaginationParams,
};
use crate::{error::AppResult, middleware::AuthenticatedUser, state::AppState};

// ── GET /api/v1/favourites ────────────────────────────────────────────────

pub async fn get_favourites(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Query(q): Query<PaginationParams>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:favourites")?;
    let limit = q.limit_clamped(20, 40);
    let max_id = q.max_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let since_id = q.since_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let min_id = q.min_id.as_deref().and_then(|s| s.parse::<i64>().ok());

    // Fetch (sort_id, status_id) pairs ordered by favourite sort_id so that
    // pagination reflects the time the favourite was created, not the status age.
    struct FRow {
        sort_id: i64,
        status_id: i64,
    }
    let frows: Vec<FRow> = if min_id.is_some() {
        sqlx::query_as!(
            FRow,
            r#"SELECT f.id AS sort_id, f.status_id FROM favourites f
               JOIN statuses s ON s.id = f.status_id
               WHERE f.account_id = $1
                 AND s.deleted_at IS NULL
                 AND ($2::bigint IS NULL OR f.id > $2)
               ORDER BY f.id ASC LIMIT $3"#,
            auth.account_id,
            min_id,
            limit
        )
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as!(
            FRow,
            r#"SELECT f.id AS sort_id, f.status_id FROM favourites f
               JOIN statuses s ON s.id = f.status_id
               WHERE f.account_id = $1
                 AND s.deleted_at IS NULL
                 AND ($2::bigint IS NULL OR f.id < $2)
                 AND ($3::bigint IS NULL OR f.id > $3)
               ORDER BY f.id DESC LIMIT $4"#,
            auth.account_id,
            max_id,
            since_id,
            limit
        )
        .fetch_all(&state.db)
        .await?
    };
    // A `min_id` page reads newest first too.
    let frows = crate::api::mastodon::timelines::newest_first(min_id, frows);

    let sort_ids: Vec<i64> = frows.iter().map(|r| r.sort_id).collect();
    let status_id_order: Vec<i64> = frows.iter().map(|r| r.status_id).collect();

    // Fetch the actual status rows, then re-sort into favourite order.
    let mut status_map_by_id: std::collections::HashMap<i64, crate::db::models::Status> = {
        if status_id_order.is_empty() {
            std::collections::HashMap::new()
        } else {
            sqlx::query_as!(
                crate::db::models::Status,
                "SELECT * FROM statuses WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL",
                &status_id_order,
            )
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|s| (s.id, s))
            .collect()
        }
    };

    // Rebuild in favourite sort order (drop any status deleted between queries).
    let statuses: Vec<crate::db::models::Status> = status_id_order
        .iter()
        .filter_map(|id| status_map_by_id.remove(id))
        .collect();

    if statuses.is_empty() {
        return Ok((HeaderMap::new(), Json(vec![])));
    }

    let all_ids: Vec<i64> = statuses.iter().map(|s| s.id).collect();
    let media_map = batch_status_media(&state, &all_ids).await?;
    let reblog_map = batch_reblog_data(&state, &statuses).await?;
    let reblog_ids: Vec<i64> = reblog_map.values().map(|(rs, _, _)| rs.id).collect();
    let mut enrich_ids = all_ids.clone();
    enrich_ids.extend_from_slice(&reblog_ids);
    let tags_map = batch_statuses_tags(&state, &enrich_ids).await?;
    let mentions_map = batch_status_mentions(&state, &enrich_ids).await?;
    let all_statuses_for_emoji: Vec<crate::db::models::Status> = statuses
        .iter()
        .cloned()
        .chain(reblog_map.values().map(|(rs, _, _)| rs.clone()))
        .collect();
    let emojis_map = batch_status_emojis(&state, &all_statuses_for_emoji).await?;
    let polls_map = batch_status_polls(&state, &enrich_ids, Some(auth.account_id)).await?;
    let cards_map = batch_status_cards(&state, &enrich_ids, Some(auth.account_id)).await?;
    let ctxs = super::statuses::batch_viewer_contexts(&state, auth.account_id, &all_ids).await?;

    let accounts: Vec<crate::db::models::Account> = {
        let account_ids: Vec<i64> = statuses
            .iter()
            .map(|s| s.account_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        sqlx::query_as!(
            crate::db::models::Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &account_ids,
        )
        .fetch_all(&state.db)
        .await?
    };
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

    let mut result = Vec::with_capacity(statuses.len());
    for s in &statuses {
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
        let mut ctx = ctxs
            .get(&s.id)
            .cloned()
            .unwrap_or(super::convert::StatusViewerContext {
                account_id: auth.account_id,
                follows_author: false,
                author_follows: false,
                favourited: true,
                reblogged: false,
                muted: false,
                bookmarked: false,
                pinned: false,
            });
        ctx.favourited = true;
        let mut api = status_from_db(
            &state.urls,
            s,
            account,
            media,
            reblog,
            Some(ctx),
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
    hydrate_status_stats(&state, result.iter_mut(), auth.account_id).await;

    // Link header cursors use sort_ids (favourite creation order), not status IDs.
    let bounds = sort_ids
        .first()
        .zip(sort_ids.last())
        .map(|(n, o)| (n.to_string(), o.to_string()));
    let resp_headers = super::link_headers(
        &req_headers,
        &uri,
        bounds.as_ref().map(|(n, o)| (n.as_str(), o.as_str())),
    );
    Ok((resp_headers, Json(result)))
}
