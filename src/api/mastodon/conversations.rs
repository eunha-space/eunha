//! Direct conversations: listing, reading and removing them, and keeping
//! them up to date as posts arrive.
//!
//! Mastodon: app/controllers/api/v1/conversations_controller.rb
//! Mastodon: app/serializers/rest/conversation_serializer.rb
//! Mastodon: app/models/account_conversation.rb
//! Mastodon: app/workers/push_conversation_worker.rb

use axum::{
    extract::{Extension, Path, Query},
    http::{HeaderMap, Uri},
    response::IntoResponse,
    Json,
};

use super::{
    accounts::batch_accounts_to_api,
    timelines::build_status_list_with_filters,
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
        return Ok((HeaderMap::new(), Json(Vec::<Conversation>::new())).into_response());
    }

    let result = render_conversations(&state, auth.account_id, &rows).await?;

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
    let mut resp = (resp_headers, Json(result)).into_response();
    resp.extensions_mut()
        .insert(super::feature_approval::Hydrated);
    Ok(resp)
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
    push_to_streaming(&state, id).await;
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
    push_to_streaming(&state, id).await;
    Ok(Json(conv))
}

// ── Shared helper ─────────────────────────────────────────────────────────

/// `AccountConversation.add_status(recipient, status)`: the status into the
/// recipient's conversation with everyone else it involves (its author and
/// those it mentions, not silently), unread unless the recipient wrote it,
/// and streamed. A status already in it changes nothing.
pub(crate) async fn add_status(state: &AppState, recipient_id: i64, status_id: i64) {
    if let Err(error) = try_add_status(state, recipient_id, status_id).await {
        tracing::warn!(%error, status_id, "could not add a status to a conversation");
    }
}

async fn try_add_status(state: &AppState, recipient_id: i64, status_id: i64) -> sqlx::Result<()> {
    let Some(status) = sqlx::query!(
        "SELECT account_id, conversation_id FROM statuses WHERE id = $1",
        status_id
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let Some(conversation_id) = status.conversation_id else {
        return Ok(());
    };
    // `participants_from_status`.
    let mut participants: Vec<i64> = sqlx::query_scalar!(
        "SELECT account_id FROM mentions WHERE status_id = $1 AND NOT silent",
        status_id
    )
    .fetch_all(&state.db)
    .await?;
    participants.push(status.account_id);
    participants.sort_unstable();
    participants.dedup();
    participants.retain(|&id| id != recipient_id);
    let row = sqlx::query_scalar!(
        r#"INSERT INTO account_conversations
             (account_id, conversation_id, participant_account_ids, status_ids, last_status_id, unread)
           VALUES ($1, $2, $3, ARRAY[$4::bigint], $4, $5)
           ON CONFLICT (account_id, conversation_id, participant_account_ids) DO UPDATE
             SET status_ids = (SELECT array_agg(id ORDER BY id)
                               FROM unnest(array_append(account_conversations.status_ids, $4)) AS id),
                 last_status_id = GREATEST(account_conversations.last_status_id, $4),
                 unread = EXCLUDED.unread,
                 lock_version = account_conversations.lock_version + 1
             WHERE NOT ($4 = ANY(account_conversations.status_ids))
           RETURNING id"#,
        recipient_id,
        conversation_id,
        &participants,
        status_id,
        status.account_id != recipient_id,
    )
    .fetch_optional(&state.db)
    .await?;
    // `after_commit :push_to_streaming_api`.
    if let Some(row) = row {
        push_to_streaming(state, row).await;
    }
    Ok(())
}

/// `AccountConversation#push_to_streaming_api` (`after_commit`) and
/// `PushConversationWorker`: the conversation as rendered for its account, to
/// the account's `direct` stream, when somebody streams it.
pub(crate) async fn push_to_streaming(state: &AppState, row_id: i64) {
    let Ok(Some(row)) = sqlx::query!(
        "SELECT account_id, unread, participant_account_ids, last_status_id FROM account_conversations WHERE id = $1",
        row_id
    )
    .fetch_optional(&state.db)
    .await
    else {
        return;
    };
    let channel = format!("timeline:direct:{}", row.account_id);
    if !state.streaming.is_subscribed(&channel).await {
        return;
    }
    let Ok(conversation) = build_conversation_response(
        state,
        row.account_id,
        row_id,
        row.unread,
        row.participant_account_ids,
        row.last_status_id,
    )
    .await
    else {
        return;
    };
    if let Ok(payload) = serde_json::to_value(&conversation) {
        state
            .streaming
            .publish(
                &channel,
                serde_json::json!({"event": "conversation", "payload": payload}),
            )
            .await;
    }
}

async fn build_conversation_response(
    state: &AppState,
    viewer_account_id: i64,
    row_id: i64,
    unread: bool,
    participant_account_ids: Vec<i64>,
    last_status_id: Option<i64>,
) -> AppResult<Conversation> {
    let row = ConvRow {
        id: row_id,
        unread,
        participant_account_ids,
        last_status_id,
    };
    render_conversations(state, viewer_account_id, std::slice::from_ref(&row))
        .await?
        .pop()
        .ok_or(AppError::NotFound)
}

/// `REST::ConversationSerializer` for each of `rows`, as `viewer` reads them:
/// the participants as `REST::AccountSerializer` writes any account, and the
/// last status as `REST::StatusSerializer` writes any status, with the
/// viewer's filter matches (`StatusRelationshipsPresenter`), its quote and
/// its application — the same rendering the timelines use. Every account in
/// it has `feature_approval.current_user` set for the viewer.
async fn render_conversations(
    state: &AppState,
    viewer: i64,
    rows: &[ConvRow],
) -> AppResult<Vec<Conversation>> {
    use std::collections::{HashMap, HashSet};

    let participant_ids: Vec<i64> = rows
        .iter()
        .flat_map(|r| r.participant_account_ids.iter().copied())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let participants: HashMap<i64, super::types::Account> = if participant_ids.is_empty() {
        HashMap::new()
    } else {
        let accounts = sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &participant_ids,
        )
        .fetch_all(&state.db)
        .await?;
        let ids: Vec<i64> = accounts.iter().map(|a| a.id).collect();
        ids.into_iter()
            .zip(batch_accounts_to_api(state, &accounts).await)
            .collect()
    };

    let last_status_ids: Vec<i64> = rows.iter().filter_map(|r| r.last_status_id).collect();
    let last_statuses: HashMap<i64, super::types::Status> = if last_status_ids.is_empty() {
        HashMap::new()
    } else {
        let statuses = sqlx::query_as!(
            crate::db::models::Status,
            "SELECT * FROM statuses WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL",
            &last_status_ids,
        )
        .fetch_all(&state.db)
        .await?;
        build_status_list_with_filters(state, statuses, Some(viewer))
            .await?
            .into_iter()
            .filter_map(|s| Some((s.id.parse().ok()?, s)))
            .collect()
    };

    let mut result: Vec<Conversation> = rows
        .iter()
        .map(|row| Conversation {
            id: row.id.to_string(),
            unread: row.unread,
            accounts: row
                .participant_account_ids
                .iter()
                .filter_map(|id| participants.get(id).cloned())
                .collect(),
            // `get`, not `remove`: two rows of one conversation may name the
            // same last status.
            last_status: row
                .last_status_id
                .and_then(|sid| last_statuses.get(&sid))
                .cloned(),
        })
        .collect();

    let mut accounts = Vec::new();
    let mut statuses = Vec::new();
    for conversation in result.iter_mut() {
        accounts.extend(conversation.accounts.iter_mut());
        statuses.extend(conversation.last_status.as_mut());
    }
    super::feature_approval::hydrate_entities(state, viewer, accounts, statuses).await;
    Ok(result)
}
