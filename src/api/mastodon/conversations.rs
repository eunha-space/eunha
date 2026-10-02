use axum::{
    extract::{Extension, Path, Query},
    http::{HeaderMap, Uri},
    response::IntoResponse,
    Json,
};

use super::{
    accounts::{batch_account_emojis, batch_account_roles, batch_account_stats, fetch_account},
    convert::{account_from_db, status_from_db},
    status_serialize::{
        batch_reblog_data, batch_status_cards, batch_status_emojis, batch_status_media,
        batch_status_mentions, batch_status_polls, batch_statuses_tags, build_status,
        fetch_status_media, hydrate_status_stats,
    },
    statuses::{batch_viewer_contexts, build_viewer_context},
    types::{Conversation, PaginationParams},
};
use crate::{
    db::models::Account,
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

// Mastodon's conversation id is the `account_conversations` row's own id, not
// the `conversation_id` it points at. The two differ whenever a thread's
// participant set changes: the table is unique on
// (account_id, conversation_id, participant_account_ids), so one account can
// hold several rows for one conversation. Serving `conversation_id` handed the
// client duplicate ids, and a client that keys rows by id — as every one does —
// renders and deletes the wrong ones.
struct ConvRow {
    id: i64,
    unread: bool,
    participant_account_ids: Vec<i64>,
    last_status_id: Option<i64>,
}

// ── GET /api/v1/conversations ─────────────────────────────────────────────

pub async fn get_conversations(
    state: AppState,
    Query(pagination): Query<PaginationParams>,
    uri: Uri,
    req_headers: HeaderMap,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:statuses")?;
    let limit = pagination.limit_clamped(20, 40);
    let max_id = pagination
        .max_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let since_id = pagination
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let min_id = pagination
        .min_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());

    // Mastodon paginates conversations by last_status_id
    let rows: Vec<ConvRow> = if min_id.is_some() {
        sqlx::query_as!(
            ConvRow,
            r#"SELECT ac.id, ac.unread, ac.participant_account_ids, ac.last_status_id
               FROM account_conversations ac
               WHERE ac.account_id = $1
                 AND ($2::bigint IS NULL OR ac.last_status_id > $2)
               ORDER BY ac.last_status_id ASC NULLS LAST
               LIMIT $3"#,
            auth.account_id,
            min_id,
            limit,
        )
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as!(
            ConvRow,
            r#"SELECT ac.id, ac.unread, ac.participant_account_ids, ac.last_status_id
               FROM account_conversations ac
               WHERE ac.account_id = $1
                 AND ($2::bigint IS NULL OR ac.last_status_id < $2)
                 AND ($4::bigint IS NULL OR ac.last_status_id > $4)
               ORDER BY ac.last_status_id DESC NULLS LAST
               LIMIT $3"#,
            auth.account_id,
            max_id,
            limit,
            since_id,
        )
        .fetch_all(&state.db)
        .await?
    };
    // A `min_id` page reads newest first too.
    let rows = crate::api::mastodon::timelines::newest_first(min_id, rows);

    if rows.is_empty() {
        return Ok((HeaderMap::new(), Json(vec![])));
    }

    // Collect all unique participant account IDs across all conversations
    let all_participant_ids: Vec<i64> = rows
        .iter()
        .flat_map(|r| r.participant_account_ids.iter().copied())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let participant_accounts: Vec<Account> = if !all_participant_ids.is_empty() {
        sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &all_participant_ids,
        )
        .fetch_all(&state.db)
        .await?
    } else {
        vec![]
    };
    let participant_acct_map: std::collections::HashMap<i64, Account> = participant_accounts
        .into_iter()
        .map(|a| (a.id, a))
        .collect();
    let participant_emojis_map = {
        let accs: Vec<Account> = participant_acct_map.values().cloned().collect();
        batch_account_emojis(&state, &accs).await
    };

    // Fetch last statuses by ID (already known from last_status_id)
    let last_status_ids: Vec<i64> = rows.iter().filter_map(|r| r.last_status_id).collect();
    let last_statuses: Vec<crate::db::models::Status> = if !last_status_ids.is_empty() {
        sqlx::query_as!(
            crate::db::models::Status,
            "SELECT * FROM statuses WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL",
            &last_status_ids,
        )
        .fetch_all(&state.db)
        .await?
    } else {
        vec![]
    };

    // Batch enrich last statuses
    let status_ids: Vec<i64> = last_statuses.iter().map(|s| s.id).collect();
    let mut enriched_map: std::collections::HashMap<i64, super::types::Status> =
        std::collections::HashMap::new();

    if !status_ids.is_empty() {
        let media_map = batch_status_media(&state, &status_ids).await?;
        let reblog_map = batch_reblog_data(&state, &last_statuses).await?;
        let reblog_ids: Vec<i64> = reblog_map.values().map(|(rs, _, _)| rs.id).collect();
        let mut enrich_ids = status_ids.clone();
        enrich_ids.extend_from_slice(&reblog_ids);
        let tags_map = batch_statuses_tags(&state, &enrich_ids).await?;
        let mentions_map = batch_status_mentions(&state, &enrich_ids).await?;
        let all_for_emoji: Vec<crate::db::models::Status> = last_statuses
            .iter()
            .cloned()
            .chain(reblog_map.values().map(|(rs, _, _)| rs.clone()))
            .collect();
        let emojis_map = batch_status_emojis(&state, &all_for_emoji).await?;
        let polls_map = batch_status_polls(&state, &enrich_ids, Some(auth.account_id)).await?;
        let cards_map = batch_status_cards(&state, &enrich_ids, Some(auth.account_id)).await?;
        let ctxs = batch_viewer_contexts(&state, auth.account_id, &status_ids).await?;

        let status_account_ids: Vec<i64> = last_statuses
            .iter()
            .map(|s| s.account_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let status_accounts: Vec<Account> = sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &status_account_ids,
        )
        .fetch_all(&state.db)
        .await?;
        let status_account_map: std::collections::HashMap<i64, Account> =
            status_accounts.into_iter().map(|a| (a.id, a)).collect();
        let all_stat_accounts_for_emoji: Vec<Account> = {
            let mut seen = std::collections::HashSet::new();
            status_account_map
                .values()
                .chain(reblog_map.values().map(|(_, ra, _)| ra))
                .filter(|a| seen.insert(a.id))
                .cloned()
                .collect()
        };
        let status_account_emojis_map =
            batch_account_emojis(&state, &all_stat_accounts_for_emoji).await;
        let status_account_roles_map =
            batch_account_roles(&state, &all_stat_accounts_for_emoji).await;

        for s in &last_statuses {
            let Some(account) = status_account_map.get(&s.account_id) else {
                continue;
            };
            let media = media_map.get(&s.id).cloned().unwrap_or_default();
            let reblog = reblog_map.get(&s.id).cloned();
            let ctx = ctxs.get(&s.id).cloned();
            let mentions = mentions_map.get(&s.id).cloned().unwrap_or_default();
            let rb_mentions = reblog
                .as_ref()
                .and_then(|(rs, _, _)| mentions_map.get(&rs.id))
                .cloned()
                .unwrap_or_default();
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
            api.account.emojis = status_account_emojis_map
                .get(&account.id)
                .cloned()
                .unwrap_or_default();
            api.account.roles = status_account_roles_map
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
                rb.account.emojis = status_account_emojis_map
                    .get(&rb_id)
                    .cloned()
                    .unwrap_or_default();
                rb.account.roles = status_account_roles_map
                    .get(&rb_id)
                    .cloned()
                    .unwrap_or_default();
                rb.tags = tags_map.get(&rid).cloned().unwrap_or_default();
                rb.mentions = rb_mentions;
                rb.emojis = emojis_map.get(&rid).cloned().unwrap_or_default();
                rb.poll = polls_map.get(&rid).cloned();
                rb.card = cards_map.get(&rid).cloned();
            }
            enriched_map.insert(s.id, api);
        }
        hydrate_status_stats(&state, enriched_map.values_mut()).await;
    }

    let participant_roles_map = batch_account_roles(
        &state,
        &participant_acct_map.values().cloned().collect::<Vec<_>>(),
    )
    .await;
    let participant_stats_map = batch_account_stats(
        &state,
        &participant_acct_map.keys().copied().collect::<Vec<_>>(),
    )
    .await;
    let mut result = Vec::with_capacity(rows.len());
    for row in &rows {
        result.push(Conversation {
            id: row.id.to_string(),
            unread: row.unread,
            accounts: row
                .participant_account_ids
                .iter()
                .filter_map(|id| participant_acct_map.get(id))
                .map(|a| {
                    let mut api_acct = account_from_db(&state.urls, a);
                    api_acct.emojis = participant_emojis_map
                        .get(&a.id)
                        .cloned()
                        .unwrap_or_default();
                    api_acct.roles = participant_roles_map
                        .get(&a.id)
                        .cloned()
                        .unwrap_or_default();
                    if let Some(&(statuses, following, followers)) =
                        participant_stats_map.get(&a.id)
                    {
                        api_acct.statuses_count = statuses;
                        api_acct.following_count = following;
                        api_acct.followers_count = followers;
                    }
                    api_acct
                })
                .collect(),
            // `get`, not `remove`: two rows of one conversation may name the
            // same last status.
            last_status: row
                .last_status_id
                .and_then(|sid| enriched_map.get(&sid))
                .cloned(),
        });
    }

    // Link header cursors use last_status_id (matching Mastodon's pagination_max_id/since_id)
    let newest_cursor = rows
        .first()
        .and_then(|r| r.last_status_id)
        .map(|id| id.to_string());
    let oldest_cursor = rows
        .last()
        .and_then(|r| r.last_status_id)
        .map(|id| id.to_string());
    let bounds = newest_cursor.zip(oldest_cursor);
    let resp_headers = super::link_headers(
        &req_headers,
        &uri,
        bounds.as_ref().map(|(n, o)| (n.as_str(), o.as_str())),
    );
    Ok((resp_headers, Json(result)))
}

// ── DELETE /api/v1/conversations/:id ─────────────────────────────────────

pub async fn delete_conversation(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:conversations")?;
    let deleted = sqlx::query!(
        "DELETE FROM account_conversations WHERE id = $1 AND account_id = $2 RETURNING id",
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?;

    if deleted.is_none() {
        return Err(AppError::NotFound);
    }

    Ok(Json(serde_json::json!({})))
}

// ── POST /api/v1/conversations/:id/unread ────────────────────────────────

pub async fn mark_conversation_unread(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Conversation>> {
    auth.require_scope("write:conversations")?;

    struct Updated {
        participant_account_ids: Vec<i64>,
        last_status_id: Option<i64>,
    }
    let updated = sqlx::query_as!(
        Updated,
        "UPDATE account_conversations SET unread = true WHERE id = $1 AND account_id = $2 RETURNING participant_account_ids, last_status_id",
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?;

    let Some(row) = updated else {
        return Err(AppError::NotFound);
    };

    let conv = build_conversation_response(
        &state,
        auth.account_id,
        id,
        true,
        row.participant_account_ids,
        row.last_status_id,
    )
    .await?;
    Ok(Json(conv))
}

// ── POST /api/v1/conversations/:id/read ──────────────────────────────────

pub async fn mark_conversation_read(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Conversation>> {
    auth.require_scope("write:conversations")?;

    struct Updated {
        participant_account_ids: Vec<i64>,
        last_status_id: Option<i64>,
    }
    let updated = sqlx::query_as!(
        Updated,
        "UPDATE account_conversations SET unread = false WHERE id = $1 AND account_id = $2 RETURNING participant_account_ids, last_status_id",
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?;

    let Some(row) = updated else {
        return Err(AppError::NotFound);
    };

    let conv = build_conversation_response(
        &state,
        auth.account_id,
        id,
        false,
        row.participant_account_ids,
        row.last_status_id,
    )
    .await?;
    Ok(Json(conv))
}

// ── Shared helper ─────────────────────────────────────────────────────────

async fn build_conversation_response(
    state: &AppState,
    viewer_account_id: i64,
    row_id: i64,
    unread: bool,
    participant_account_ids: Vec<i64>,
    last_status_id: Option<i64>,
) -> AppResult<Conversation> {
    let participants: Vec<Account> = if !participant_account_ids.is_empty() {
        sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &participant_account_ids,
        )
        .fetch_all(&state.db)
        .await?
    } else {
        vec![]
    };

    let last_status = if let Some(sid) = last_status_id {
        let s = sqlx::query_as!(
            crate::db::models::Status,
            "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
            sid,
        )
        .fetch_optional(&state.db)
        .await?;

        if let Some(s) = s {
            let saccount = fetch_account(state, s.account_id).await?;
            let media = fetch_status_media(state, s.id).await?;
            let reblog = super::status_serialize::fetch_reblog_data(state, &s).await?;
            let ctx = build_viewer_context(state, viewer_account_id, s.id).await?;
            Some(build_status(state, &s, &saccount, media, reblog, Some(ctx)).await?)
        } else {
            None
        }
    } else {
        None
    };

    let participant_emojis_map = batch_account_emojis(state, &participants).await;
    let participant_roles_map = batch_account_roles(state, &participants).await;
    Ok(Conversation {
        id: row_id.to_string(),
        unread,
        accounts: participants
            .iter()
            .map(|a| {
                let mut api = account_from_db(&state.urls, a);
                api.emojis = participant_emojis_map
                    .get(&a.id)
                    .cloned()
                    .unwrap_or_default();
                api.roles = participant_roles_map
                    .get(&a.id)
                    .cloned()
                    .unwrap_or_default();
                api
            })
            .collect(),
        last_status,
    })
}
