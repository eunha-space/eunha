use axum::{
    extract::{Extension, Path, Query, RawQuery},
    http::{HeaderMap, Uri},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use super::{
    accounts::{
        apply_account_stats, batch_account_emojis, batch_account_roles, batch_account_stats,
        batch_accounts_to_api, fetch_account_emojis,
    },
    convert::{account_from_db, status_from_db},
    status_serialize::{
        batch_reblog_data, batch_status_cards, batch_status_emojis, batch_status_media,
        batch_status_mentions, batch_status_polls, batch_statuses_tags, build_status,
        fetch_reblog_data, fetch_status_media, hydrate_status_stats,
    },
    types::{
        Notification, NotificationGroup, NotificationGroupsResponse, NotificationPagination,
        NotificationPolicy, NotificationPolicySummary, NotificationPolicyV1, NotificationRequest,
        PaginationParams, PartialAccount,
    },
};
use crate::{
    db::models::{Account, Notification as DbNotification},
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

async fn fetch_reports_map(
    state: &AppState,
    report_ids: &[i64],
) -> AppResult<std::collections::HashMap<i64, super::types::Report>> {
    let mut map = std::collections::HashMap::new();
    if report_ids.is_empty() {
        return Ok(map);
    }
    let rows = sqlx::query!(
        r#"SELECT r.id, r.comment, COALESCE(r.forwarded, false) AS "forwarded!",
                  CASE r.category WHEN 1000 THEN 'spam' WHEN 1500 THEN 'legal' WHEN 2000 THEN 'violation' ELSE 'other' END AS "category!",
                  r.action_taken_at, r.created_at, r.status_ids, r.rule_ids,
                  r.target_account_id,
                  ARRAY(SELECT cr.collection_id FROM collection_reports cr
                        WHERE cr.report_id = r.id ORDER BY cr.collection_id) AS "collection_ids!"
           FROM reports r
           WHERE r.id = ANY($1::bigint[])"#,
        report_ids,
    )
    .fetch_all(&state.db)
    .await?;

    let target_ids: Vec<i64> = rows
        .iter()
        .map(|r| r.target_account_id)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let target_accounts: std::collections::HashMap<i64, Account> = if !target_ids.is_empty() {
        sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &target_ids
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|a| (a.id, a))
        .collect()
    } else {
        std::collections::HashMap::new()
    };
    let ta_vec: Vec<Account> = target_accounts.values().cloned().collect();
    let ta_emojis_map = batch_account_emojis(state, &ta_vec).await;
    let ta_stats_map =
        batch_account_stats(state, &target_accounts.keys().copied().collect::<Vec<_>>()).await;
    for r in rows {
        let Some(ta) = target_accounts.get(&r.target_account_id) else {
            continue;
        };
        let mut ta_api = account_from_db(&state.urls, ta);
        ta_api.emojis = ta_emojis_map.get(&ta.id).cloned().unwrap_or_default();
        if let Some(&(statuses_c, following, followers)) = ta_stats_map.get(&ta.id) {
            ta_api.statuses_count = statuses_c;
            ta_api.following_count = following;
            ta_api.followers_count = followers;
        }
        map.insert(
            r.id,
            super::types::Report {
                id: r.id.to_string(),
                action_taken: r.action_taken_at.is_some(),
                action_taken_at: r.action_taken_at.map(super::convert::mastodon_date),
                category: r.category,
                comment: r.comment,
                forwarded: r.forwarded,
                created_at: super::convert::mastodon_date(r.created_at),
                status_ids: r.status_ids.iter().map(|i| i.to_string()).collect(),
                rule_ids: r
                    .rule_ids
                    .unwrap_or_default()
                    .iter()
                    .map(|i| i.to_string())
                    .collect(),
                collection_ids: r.collection_ids.iter().map(|i| i.to_string()).collect(),
                target_account: ta_api,
            },
        );
    }
    Ok(map)
}

/// `REST::ReportSerializer` of one report.
pub async fn report_entity(
    state: &AppState,
    report_id: i64,
) -> AppResult<Option<super::types::Report>> {
    Ok(fetch_reports_map(state, &[report_id])
        .await?
        .remove(&report_id))
}

/// The `moderation_warning` a notification carries: `REST::AccountWarningSerializer`
/// of its `AccountWarning` activity.
async fn moderation_warning_of(state: &AppState, n: &DbNotification) -> Option<serde_json::Value> {
    if n.r#type.as_deref() != Some("moderation_warning")
        || n.activity_type.as_deref() != Some("AccountWarning")
    {
        return None;
    }
    crate::moderation::warning::serialize(state, n.activity_id?).await
}

/// The `event` a `severed_relationships` notification carries:
/// `REST::AccountRelationshipSeveranceEventSerializer`.
async fn severance_event_of(state: &AppState, n: &DbNotification) -> Option<serde_json::Value> {
    if n.activity_type.as_deref() != Some("AccountRelationshipSeveranceEvent") {
        return None;
    }
    crate::moderation::severance::serialize(state, n.activity_id?).await
}

/// `belongs_to :target_collection, key: :collection, if: :collection_type?`:
/// for `added_to_collection` the collection the item is in, for
/// `collection_update` the collection, as the recipient sees it; `null` when
/// it is gone, and absent for any other type.
async fn collection_of(state: &AppState, n: &DbNotification) -> Option<serde_json::Value> {
    let collection_id = match (n.r#type.as_deref(), n.activity_type.as_deref()) {
        (Some("added_to_collection"), Some("CollectionItem")) => sqlx::query_scalar!(
            "SELECT collection_id FROM collection_items WHERE id = $1",
            n.activity_id?,
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten(),
        (Some("collection_update"), Some("Collection")) => n.activity_id,
        (Some("added_to_collection" | "collection_update"), _) => None,
        _ => return None,
    };
    let rendered = match collection_id {
        Some(id) => super::collections::render(state, id, Some(n.account_id))
            .await
            .ok()
            .flatten(),
        None => None,
    };
    Some(rendered.unwrap_or(serde_json::Value::Null))
}

/// One notification as `GET /api/v1/notifications/:id` renders it, as the
/// streaming API sends it.
pub async fn render_notification(state: &AppState, notification_id: i64) -> Option<String> {
    let n = sqlx::query_as!(
        DbNotification,
        "SELECT * FROM notifications WHERE id = $1",
        notification_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()?;
    let notification = build_notification(state, &n, None).await.ok()?;
    serde_json::to_string(&notification).ok()
}

/// Resolve status_id for a batch of notifications from their activity columns.
/// Returns a map of notification_id → status_id.
async fn batch_notification_status_ids(
    state: &AppState,
    notification_ids: &[i64],
) -> std::collections::HashMap<i64, i64> {
    if notification_ids.is_empty() {
        return std::collections::HashMap::new();
    }
    sqlx::query!(
        r#"SELECT n.id,
               CASE n.activity_type
                   WHEN 'Status'    THEN n.activity_id
                   WHEN 'Mention'   THEN m.status_id
                   WHEN 'Favourite' THEN f.status_id
                   WHEN 'Poll'      THEN p.status_id
                   WHEN 'Quote'     THEN q.status_id
                   ELSE NULL
               END AS "status_id: i64"
           FROM notifications n
           LEFT JOIN mentions   m ON m.id = n.activity_id AND n.activity_type = 'Mention'
           LEFT JOIN favourites f ON f.id = n.activity_id AND n.activity_type = 'Favourite'
           LEFT JOIN polls      p ON p.id = n.activity_id AND n.activity_type = 'Poll'
           LEFT JOIN quotes     q ON q.id = n.activity_id AND n.activity_type = 'Quote'
           WHERE n.id = ANY($1::bigint[])
             AND n.activity_id IS NOT NULL"#,
        notification_ids,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .filter_map(|r| r.status_id.map(|sid| (r.id, sid)))
    .collect()
}

// ── GET /api/v1/notifications ─────────────────────────────────────────────

pub async fn get_notifications(
    state: AppState,
    Query(pagination): Query<PaginationParams>,
    RawQuery(qs): RawQuery,
    uri: Uri,
    req_headers: HeaderMap,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:notifications")?;
    let limit = pagination.limit_clamped(40, 80);
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

    let (types, exclude_types, account_id, include_filtered) = parse_notif_filters(qs.as_deref());
    let supported_types = query_values(qs.as_deref(), "supported_types");
    // Mastodon excludes filtered notifications by default; include all when include_filtered=true
    // or when filtering by account_id.
    let exclude_filtered = !include_filtered && account_id.is_none();

    let notifications: Vec<DbNotification> = if min_id.is_some() {
        sqlx::query_as(
            r#"SELECT n.* FROM notifications n
               JOIN accounts a ON a.id = n.from_account_id AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
               WHERE n.account_id = $1
                 AND ($2::bigint IS NULL OR n.id > $2)
                 AND ($5::text[] IS NULL OR n.type = ANY($5))
                 AND ($6::text[] IS NULL OR NOT (n.type = ANY($6)))
                 AND ($7::bigint IS NULL OR n.from_account_id = $7)
                 AND (NOT $8::boolean OR NOT n.filtered)
               ORDER BY n.id ASC
               LIMIT $4"#,
        )
        .bind(auth.account_id)
        .bind(min_id)
        .bind(Option::<i64>::None)
        .bind(limit)
        .bind(types)
        .bind(exclude_types)
        .bind(account_id)
        .bind(exclude_filtered)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as(
            r#"SELECT n.* FROM notifications n
               JOIN accounts a ON a.id = n.from_account_id AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
               WHERE n.account_id = $1
                 AND ($2::bigint IS NULL OR n.id < $2)
                 AND ($3::bigint IS NULL OR n.id > $3)
                 AND ($5::text[] IS NULL OR n.type = ANY($5))
                 AND ($6::text[] IS NULL OR NOT (n.type = ANY($6)))
                 AND ($7::bigint IS NULL OR n.from_account_id = $7)
                 AND (NOT $8::boolean OR NOT n.filtered)
               ORDER BY n.id DESC
               LIMIT $4"#,
        )
        .bind(auth.account_id)
        .bind(max_id)
        .bind(since_id)
        .bind(limit)
        .bind(types)
        .bind(exclude_types)
        .bind(account_id)
        .bind(exclude_filtered)
        .fetch_all(&state.db)
        .await?
    };
    // A `min_id` page reads newest first too.
    let notifications = crate::api::mastodon::timelines::newest_first(min_id, notifications);

    if notifications.is_empty() {
        return Ok((HeaderMap::new(), Json(vec![])));
    }

    let from_account_ids: Vec<i64> = notifications
        .iter()
        .map(|n| n.from_account_id)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let from_accounts_vec: Vec<Account> = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
        &from_account_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let from_account_map: std::collections::HashMap<i64, Account> =
        from_accounts_vec.into_iter().map(|a| (a.id, a)).collect();
    let (from_account_emojis_map, from_account_roles_map) = {
        let accs: Vec<Account> = from_account_map.values().cloned().collect();
        (
            batch_account_emojis(&state, &accs).await,
            batch_account_roles(&state, &accs).await,
        )
    };
    let from_account_stats_map = batch_account_stats(
        &state,
        &from_account_map.keys().copied().collect::<Vec<_>>(),
    )
    .await;

    let notif_ids_v1: Vec<i64> = notifications.iter().map(|n| n.id).collect();
    let notif_status_map_v1 = batch_notification_status_ids(&state, &notif_ids_v1).await;
    let notif_status_ids: Vec<i64> = notif_status_map_v1
        .values()
        .copied()
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let status_api_map: std::collections::HashMap<i64, super::types::Status> = if !notif_status_ids
        .is_empty()
    {
        let statuses: Vec<crate::db::models::Status> = sqlx::query_as!(
            crate::db::models::Status,
            "SELECT * FROM statuses WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL",
            &notif_status_ids,
        )
        .fetch_all(&state.db)
        .await?;

        let stat_account_ids: Vec<i64> = statuses
            .iter()
            .map(|s| s.account_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let stat_accounts: Vec<Account> = sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &stat_account_ids,
        )
        .fetch_all(&state.db)
        .await?;
        let stat_account_map: std::collections::HashMap<i64, Account> =
            stat_accounts.into_iter().map(|a| (a.id, a)).collect();

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
        let viewer_ctxs =
            super::statuses::batch_viewer_contexts(&state, auth.account_id, &all_ids).await?;
        let notif_filter_map =
            super::timelines::compute_filter_results(&state.db, auth.account_id, &statuses).await;
        let all_accounts_for_emoji: Vec<Account> = {
            let mut seen = std::collections::HashSet::new();
            stat_account_map
                .values()
                .chain(reblog_map.values().map(|(_, ra, _)| ra))
                .filter(|a| seen.insert(a.id))
                .cloned()
                .collect()
        };
        let stat_account_emojis_map = batch_account_emojis(&state, &all_accounts_for_emoji).await;
        let stat_account_roles_map = batch_account_roles(&state, &all_accounts_for_emoji).await;

        let mut map = std::collections::HashMap::new();
        for s in &statuses {
            let Some(account) = stat_account_map.get(&s.account_id) else {
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
            let ctx = viewer_ctxs.get(&s.id).cloned();
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
            api.account.emojis = stat_account_emojis_map
                .get(&account.id)
                .cloned()
                .unwrap_or_default();
            api.account.roles = stat_account_roles_map
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
                rb.account.emojis = stat_account_emojis_map
                    .get(&rb_id)
                    .cloned()
                    .unwrap_or_default();
                rb.account.roles = stat_account_roles_map
                    .get(&rb_id)
                    .cloned()
                    .unwrap_or_default();
                rb.tags = tags_map.get(&rid).cloned().unwrap_or_default();
                rb.mentions = rb_mentions;
                rb.emojis = emojis_map.get(&rid).cloned().unwrap_or_default();
                rb.poll = polls_map.get(&rid).cloned();
                rb.card = cards_map.get(&rid).cloned();
            }
            if let Some(filter_json) = notif_filter_map.get(&s.id) {
                if let Some(arr) = filter_json.as_array() {
                    if !arr.is_empty() {
                        api.filtered = Some(arr.clone());
                    }
                }
            }
            map.insert(s.id, api);
        }
        hydrate_status_stats(&state, map.values_mut(), auth.account_id).await;
        map
    } else {
        std::collections::HashMap::new()
    };

    // Batch-fetch reports for admin.report notifications (via activity_id/activity_type polymorphic association)
    let report_ids: Vec<i64> = notifications
        .iter()
        .filter_map(|n| {
            if n.r#type.as_deref() == Some("admin.report")
                && n.activity_type.as_deref() == Some("Report")
            {
                n.activity_id
            } else {
                None
            }
        })
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let report_map = fetch_reports_map(&state, &report_ids).await?;

    let mut result = Vec::with_capacity(notifications.len());
    for n in &notifications {
        let Some(account) = from_account_map.get(&n.from_account_id) else {
            continue;
        };
        // A status-bearing notification whose target status is gone (deleted or
        // otherwise unavailable) would be an orphan: Mastodon cascade-deletes
        // such notifications when the status is removed, so they never appear.
        // Mirror that by skipping it here instead of emitting a statusless one.
        if let Some(sid) = notif_status_map_v1.get(&n.id) {
            if !status_api_map.contains_key(sid) {
                continue;
            }
        }
        let status = notif_status_map_v1
            .get(&n.id)
            .and_then(|sid| status_api_map.get(sid))
            .cloned();
        let report_id = if n.activity_type.as_deref() == Some("Report") {
            n.activity_id
        } else {
            None
        };
        let report = report_id.and_then(|rid| report_map.get(&rid)).cloned();
        let mut notif_account = account_from_db(&state.urls, account);
        notif_account.emojis = from_account_emojis_map
            .get(&account.id)
            .cloned()
            .unwrap_or_default();
        notif_account.roles = from_account_roles_map
            .get(&account.id)
            .cloned()
            .unwrap_or_default();
        if let Some(&(statuses_c, following, followers)) = from_account_stats_map.get(&account.id) {
            notif_account.statuses_count = statuses_c;
            notif_account.following_count = following;
            notif_account.followers_count = followers;
        }
        let notification_type = n.r#type.clone().unwrap_or_default();
        let event = severance_event_of(&state, n).await;
        let moderation_warning = moderation_warning_of(&state, n).await;
        let collection = collection_of(&state, n).await;
        let fallback = notification_fallback(
            &state,
            &notification_type,
            supported_types.as_deref(),
            &[&notif_account],
            report.as_ref(),
            event.as_ref(),
            moderation_warning.as_ref(),
            collection.as_ref(),
        );
        result.push(Notification {
            id: n.id.to_string(),
            notification_type,
            created_at: super::convert::mastodon_date(n.created_at),
            // Mastodon serializes the notification's own group key here too, so
            // a client can tell which group a single notification belongs to.
            group_key: n
                .group_key
                .clone()
                .unwrap_or_else(|| format!("ungrouped-{}", n.id)),
            account: notif_account,
            status,
            report,
            filtered: if n.filtered { Some(true) } else { None },
            event,
            moderation_warning,
            fallback,
            collection,
        });
    }

    // Base pagination on the raw page boundaries, not the filtered `result`:
    // a page made up entirely of orphaned (skipped) notifications must still
    // advance the cursor, otherwise the client would stop paginating early.
    let bounds = notifications
        .first()
        .zip(notifications.last())
        .map(|(n, o)| (n.id.to_string(), o.id.to_string()));
    let resp_headers = super::link_headers(
        &req_headers,
        &uri,
        bounds.as_ref().map(|(n, o)| (n.as_str(), o.as_str())),
    );
    crate::email_subscriptions::fill(&state, result.iter_mut().map(|n| &mut n.account)).await;
    Ok((resp_headers, Json(result)))
}

// ── GET /api/v1/notifications/:id ─────────────────────────────────────────

pub async fn get_notification(
    state: AppState,
    Path(id): Path<i64>,
    RawQuery(qs): RawQuery,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Notification>> {
    auth.require_scope("read:notifications")?;
    // `without_suspended.find`
    let n = sqlx::query_as!(
        DbNotification,
        "SELECT n.* FROM notifications n
         JOIN accounts a ON a.id = n.from_account_id
           AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
         WHERE n.id = $1 AND n.account_id = $2",
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let supported_types = query_values(qs.as_deref(), "supported_types");
    build_notification(&state, &n, supported_types.as_deref())
        .await
        .map(Json)
}

// ── POST /api/v1/notifications/clear ──────────────────────────────────────

pub async fn clear_notifications(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:notifications")?;
    sqlx::query!(
        "DELETE FROM notifications WHERE account_id = $1",
        auth.account_id
    )
    .execute(&state.db)
    .await?;
    Ok(Json(serde_json::json!({})))
}

// ── POST /api/v1/notifications/:id/dismiss ────────────────────────────────

pub async fn dismiss_notification(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:notifications")?;
    let deleted = sqlx::query!(
        "DELETE FROM notifications WHERE id = $1 AND account_id = $2",
        id,
        auth.account_id,
    )
    .execute(&state.db)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(Json(serde_json::json!({})))
}

// ── GET /api/v2/notifications ─────────────────────────────────────────────

/// Number of sample accounts surfaced per notification group
/// (Mastodon `NotificationGroup::SAMPLE_ACCOUNTS_SIZE`).
const SAMPLE_ACCOUNTS_SIZE: usize = 8;

/// Mastodon's `MAXIMUM_GROUP_SPAN_HOURS`: how far a single group may reach.
pub const MAXIMUM_GROUP_SPAN_HOURS: i64 = 12;

/// The part of a group key that identifies *what* is being grouped, before the
/// time bucket. `None` for a type Mastodon does not group.
///
/// Mastodon groups four types and no others — `GROUPABLE_NOTIFICATION_TYPES` —
/// and keys a favourite or boost by the status it concerns, so two people
/// favouriting different posts never share a group.
pub fn group_type_prefix(notif_type: &str, target_status_id: Option<i64>) -> Option<String> {
    match notif_type {
        "favourite" | "reblog" => target_status_id.map(|sid| format!("{notif_type}-{sid}")),
        "follow" | "admin.sign_up" => Some(notif_type.to_string()),
        _ => None,
    }
}

/// Fetch the notifications that belong to a group key for an account, newest
/// first. Used by the per-group endpoints so keys returned by the list endpoint
/// resolve back to their members.
async fn notifications_for_group_key(
    state: &AppState,
    account_id: i64,
    group_key: &str,
) -> AppResult<Vec<DbNotification>> {
    if let Some(id_str) = group_key.strip_prefix("ungrouped-") {
        let id: i64 = id_str.parse().map_err(|_| AppError::NotFound)?;
        return Ok(
            sqlx::query_as("SELECT * FROM notifications WHERE id = $1 AND account_id = $2")
                .bind(id)
                .bind(account_id)
                .fetch_all(&state.db)
                .await?,
        );
    }
    // `by_group_key`: a stored key identifies its members directly.
    let stored: Vec<DbNotification> = sqlx::query_as(
        "SELECT * FROM notifications
         WHERE account_id = $1 AND group_key = $2 ORDER BY id DESC",
    )
    .bind(account_id)
    .bind(group_key)
    .fetch_all(&state.db)
    .await?;
    Ok(stored)
}

/// `Notification::TYPES`, each with whether it is `baseline`: a type every
/// client is taken to understand, so that it is never given a `fallback`.
const NOTIFICATION_TYPES: &[(&str, bool)] = &[
    ("mention", true),
    ("status", true),
    ("reblog", true),
    ("follow", true),
    ("follow_request", true),
    ("favourite", true),
    ("poll", true),
    ("update", true),
    ("severed_relationships", false),
    ("moderation_warning", false),
    ("annual_report", true),
    ("admin.sign_up", false),
    ("admin.report", false),
    ("quote", true),
    ("quoted_update", true),
    ("added_to_collection", false),
    ("collection_update", false),
];

/// `Notification::GROUPABLE_NOTIFICATION_TYPES`.
const GROUPABLE_NOTIFICATION_TYPES: &[&str] = &["favourite", "reblog", "follow", "admin.sign_up"];

/// Every value given for `name[]`, or `name`, in the query string; `None`
/// when there is none.
fn query_values(qs: Option<&str>, name: &str) -> Option<Vec<String>> {
    let bracket = format!("{name}[]");
    let values: Vec<String> = url::form_urlencoded::parse(qs.unwrap_or("").as_bytes())
        .filter(|(k, _)| k == name || *k == bracket)
        .map(|(_, v)| v.into_owned())
        .collect();
    (!values.is_empty()).then_some(values)
}

/// What `Notification.browserable` keeps of an account's notifications.
struct Browserable {
    /// The types asked for, or `None` when that is every type.
    types: Option<Vec<String>>,
    from_account_id: Option<i64>,
    /// Whether filtered notifications are kept: when they are asked for, or
    /// when the notifications from one account are.
    include_filtered: bool,
}

impl Browserable {
    fn new(
        types: Option<Vec<String>>,
        exclude_types: Option<Vec<String>>,
        from_account_id: Option<i64>,
        include_filtered: bool,
    ) -> Self {
        // `types.map(&:to_sym) & TYPES`, less `exclude_types`.
        let requested: Vec<String> = NOTIFICATION_TYPES
            .iter()
            .map(|(t, _)| *t)
            .filter(|t| types.as_ref().is_none_or(|ts| ts.iter().any(|x| x == t)))
            .filter(|t| {
                !exclude_types
                    .as_ref()
                    .is_some_and(|ex| ex.iter().any(|x| x == t))
            })
            .map(str::to_owned)
            .collect();
        Self {
            types: (requested.len() != NOTIFICATION_TYPES.len()).then_some(requested),
            from_account_id,
            include_filtered: include_filtered || from_account_id.is_some(),
        }
    }
}

/// `paginate_groups`' `grouped_types`: the groupable types among those
/// given, sorted, or `None` when none were given — every groupable type.
fn normalize_grouped_types(grouped_types: Option<&[String]>) -> Option<Vec<String>> {
    grouped_types.map(|given| {
        let mut types: Vec<String> = GROUPABLE_NOTIFICATION_TYPES
            .iter()
            .filter(|t| given.iter().any(|g| g == *t))
            .map(|t| (*t).to_owned())
            .collect();
        types.sort();
        types
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PageOrder {
    Desc,
    Asc,
}

/// `Notification.paginate_groups` over `browserable` without suspended
/// senders: up to `limit` notifications, each the first met in `order` of
/// a group not met before, with `before_id` and `after_id` as exclusive
/// bounds. A notification without a group key, or of a type not in
/// `grouped_types`, is a group of its own.
#[allow(clippy::too_many_arguments)]
async fn paginate_groups(
    state: &AppState,
    account_id: i64,
    filter: &Browserable,
    limit: i64,
    order: PageOrder,
    before_id: Option<i64>,
    after_id: Option<i64>,
    grouped_types: Option<&[String]>,
) -> AppResult<Vec<DbNotification>> {
    let (dir, cmp) = match order {
        PageOrder::Desc => ("DESC", "<"),
        PageOrder::Asc => ("ASC", ">"),
    };
    let group_key = "COALESCE(CASE WHEN $8::text[] IS NULL OR n.type = ANY($8::text[]) \
                     THEN n.group_key END, 'ungrouped-' || n.id)::text";
    let from = "notifications n JOIN accounts a ON a.id = n.from_account_id \
                AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL";
    let conditions = "n.account_id = $1
          AND ($3::text[] IS NULL OR n.type = ANY($3::text[]))
          AND ($4::boolean OR NOT n.filtered)
          AND ($5::bigint IS NULL OR n.from_account_id = $5)
          AND ($6::bigint IS NULL OR n.id < $6)
          AND ($7::bigint IS NULL OR n.id > $7)";
    let sql = format!(
        "WITH RECURSIVE grouped_notifications AS (
           (SELECT n.*, ARRAY[{group_key}] AS groups
            FROM {from} WHERE {conditions}
            ORDER BY n.id {dir} LIMIT 1)
           UNION ALL
           (SELECT n.*, array_append(wt.groups, {group_key}) AS groups
            FROM (SELECT g.id, g.groups FROM grouped_notifications g
                  WHERE array_length(g.groups, 1) < $2) wt,
            LATERAL (SELECT n.* FROM {from}
                     WHERE {conditions} AND n.id {cmp} wt.id
                       AND NOT ({group_key} = ANY(wt.groups))
                     ORDER BY n.id {dir} LIMIT 1) n)
         )
         SELECT * FROM grouped_notifications ORDER BY id {dir} LIMIT $2"
    );
    Ok(sqlx::query_as(&sql)
        .bind(account_id)
        .bind(limit)
        .bind(filter.types.as_deref())
        .bind(filter.include_filtered)
        .bind(filter.from_account_id)
        .bind(before_id)
        .bind(after_id)
        .bind(grouped_types)
        .fetch_all(&state.db)
        .await?)
}

/// The ids a page of groups was read from: `begin..end`, or `begin...end`
/// when `exclusive`; either end may be open.
struct PageRange {
    begin: Option<i64>,
    end: Option<i64>,
    exclusive: bool,
}

/// What `NotificationGroup.load_groups_data` reads of one group.
struct GroupData {
    most_recent_id: Option<i64>,
    sample_account_ids: Vec<i64>,
    count: i64,
    page_min_id: Option<i64>,
    latest_page_notification_at: Option<chrono::NaiveDateTime>,
}

/// `NotificationGroup.load_groups_data`: for each group, its newest
/// notification, up to `SAMPLE_ACCOUNTS_SIZE` senders newest first, and how
/// many notifications it has, none past the page's end; and, for a page, the
/// oldest of its notifications from the page's beginning on and when its
/// newest arrived.
async fn load_groups_data(
    state: &AppState,
    account_id: i64,
    group_keys: &[String],
    range: Option<&PageRange>,
) -> AppResult<std::collections::HashMap<String, GroupData>> {
    if group_keys.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let upper = "($5::bigint IS NULL OR id < $5 OR (NOT $6::boolean AND id = $5))";
    let of_group = "account_id = $1 AND group_key = groups.group_key";
    let sql = format!(
        "SELECT groups.group_key,
           (SELECT id FROM notifications WHERE {of_group} AND {upper}
            ORDER BY id DESC LIMIT 1),
           array(SELECT from_account_id FROM notifications WHERE {of_group} AND {upper}
                 ORDER BY id DESC LIMIT $2),
           (SELECT count(*) FROM notifications WHERE {of_group} AND {upper}),
           (SELECT id FROM notifications WHERE {of_group} AND id >= $4
            ORDER BY id ASC LIMIT 1),
           (SELECT created_at FROM notifications WHERE {of_group} AND {upper}
            ORDER BY id DESC LIMIT 1)
         FROM unnest($3::text[]) AS groups(group_key)"
    );
    #[allow(clippy::type_complexity)]
    let rows: Vec<(
        String,
        Option<i64>,
        Vec<i64>,
        i64,
        Option<i64>,
        Option<chrono::NaiveDateTime>,
    )> = sqlx::query_as(&sql)
        .bind(account_id)
        .bind(SAMPLE_ACCOUNTS_SIZE as i64)
        .bind(group_keys)
        .bind(range.and_then(|r| r.begin).unwrap_or(0))
        .bind(range.and_then(|r| r.end))
        .bind(range.is_some_and(|r| r.exclusive))
        .fetch_all(&state.db)
        .await?;
    Ok(rows
        .into_iter()
        .map(
            |(key, most_recent_id, sample_account_ids, count, min_id, latest)| {
                (
                    key,
                    GroupData {
                        most_recent_id,
                        sample_account_ids,
                        count,
                        page_min_id: range.and(min_id),
                        latest_page_notification_at: range.and(latest),
                    },
                )
            },
        )
        .collect())
}

/// `expand_accounts_param`: `full` unless `partial_avatars` is asked for.
fn expand_accounts_param(qs: Option<&str>) -> AppResult<bool> {
    let value = url::form_urlencoded::parse(qs.unwrap_or("").as_bytes())
        .find(|(k, _)| k == "expand_accounts")
        .map(|(_, v)| v.into_owned());
    match value.as_deref() {
        None | Some("full") => Ok(false),
        Some("partial_avatars") => Ok(true),
        Some(other) => Err(AppError::BadRequest(format!(
            "Invalid value for 'expand_accounts': '{other}', allowed values are 'full' and 'partial_avatars'"
        ))),
    }
}

/// `NotificationFallbackConcern`: the `fallback` a client that did not list
/// the notification's type among its `supported_types` is given, or `None`
/// when it needs none. `accounts` are the group's sample accounts, the
/// sender first.
#[allow(clippy::too_many_arguments)]
fn notification_fallback(
    state: &AppState,
    notification_type: &str,
    supported_types: Option<&[String]>,
    accounts: &[&super::types::Account],
    report: Option<&super::types::Report>,
    event: Option<&serde_json::Value>,
    moderation_warning: Option<&serde_json::Value>,
    collection: Option<&serde_json::Value>,
) -> Option<serde_json::Value> {
    use crate::formatter::text::{link_to_mention, MentionTarget};
    fn present(v: Option<&serde_json::Value>) -> Option<&serde_json::Value> {
        v.filter(|v| !v.is_null())
    }
    let supported = supported_types?;
    let baseline = NOTIFICATION_TYPES
        .iter()
        .any(|(t, baseline)| *t == notification_type && *baseline);
    if baseline || supported.iter().any(|t| t == notification_type) {
        return None;
    }
    let h = crate::translation::html_escape;
    let mention = |account: &super::types::Account| {
        let domain = account.acct.split_once('@').map(|(_, d)| d.to_owned());
        link_to_mention(
            &MentionTarget {
                username: account.username.clone(),
                domain,
                url: account.url.clone(),
            },
            false,
        )
    };
    let base = format!("https://{}", state.urls.local_domain);
    let sign_in = |url: &str| {
        format!(
            r#"<a href="{}">Sign in to the Mastodon web app</a>"#,
            h(url)
        )
    };
    let generic = || {
        format!(
            "You're on an app that does not support the most recent version of Mastodon. {} for full functionality.",
            sign_in(&format!("{base}/"))
        )
    };
    let account = accounts.first().copied();
    let (title, summary) = match notification_type {
        "severed_relationships" => {
            let event = present(event)?;
            let target = event["target_name"].as_str().unwrap_or_default();
            (
                format!("Lost connections with {target}"),
                format!(
                    "An admin from {} has suspended {}, which means you can no longer receive updates from them or interact with them. {} to retrieve a list of the lost relationships.",
                    h(&state.urls.local_domain),
                    h(target),
                    sign_in(&format!("{base}/severed_relationships")),
                ),
            )
        }
        "moderation_warning" => {
            let id = present(moderation_warning)?["id"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            (
                "You have received a moderation warning.".to_owned(),
                format!(
                    "You're on an app that does not support the most recent version of Mastodon. {}.",
                    sign_in(&format!("{base}/disputes/strikes/{id}"))
                ),
            )
        }
        "admin.sign_up" => {
            let name = mention(account?);
            let title = match accounts.len() {
                0 | 1 => format!("{name} signed up"),
                2 => format!("{name} and one other signed up"),
                n => format!("{name} and {} others signed up", n - 1),
            };
            (title, generic())
        }
        "admin.report" => {
            let report = report?;
            let account = account?;
            let name = match account.acct.split_once('@') {
                Some((_, domain)) => h(domain),
                None => mention(account),
            };
            (
                format!("{name} reported {}", mention(&report.target_account)),
                generic(),
            )
        }
        "added_to_collection" => {
            present(collection)?;
            (
                format!("{} added you to a collection", mention(account?)),
                generic(),
            )
        }
        "collection_update" => {
            present(collection)?;
            (
                format!("{} updated a collection you are in", mention(account?)),
                generic(),
            )
        }
        _ => return None,
    };
    Some(serde_json::json!({
        "title": title,
        "summary": summary,
        "description": null,
    }))
}

/// The notifications' statuses as the viewer sees them, by id, and which
/// status each notification is about.
async fn notification_statuses(
    state: &AppState,
    viewer: i64,
    notifications: &[DbNotification],
) -> AppResult<(
    std::collections::HashMap<i64, i64>,
    std::collections::HashMap<i64, super::types::Status>,
)> {
    let notif_ids: Vec<i64> = notifications.iter().map(|n| n.id).collect();
    let notif_status_map = batch_notification_status_ids(state, &notif_ids).await;
    let notif_status_ids: Vec<i64> = notif_status_map
        .values()
        .copied()
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let status_api_map: std::collections::HashMap<i64, super::types::Status> = if !notif_status_ids
        .is_empty()
    {
        let statuses: Vec<crate::db::models::Status> = sqlx::query_as!(
            crate::db::models::Status,
            "SELECT * FROM statuses WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL",
            &notif_status_ids,
        )
        .fetch_all(&state.db)
        .await?;

        let stat_account_ids: Vec<i64> = statuses
            .iter()
            .map(|s| s.account_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let stat_accounts: Vec<Account> = sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &stat_account_ids,
        )
        .fetch_all(&state.db)
        .await?;
        let stat_account_map: std::collections::HashMap<i64, Account> =
            stat_accounts.into_iter().map(|a| (a.id, a)).collect();

        let all_ids: Vec<i64> = statuses.iter().map(|s| s.id).collect();
        let media_map = batch_status_media(state, &all_ids).await?;
        let reblog_map = batch_reblog_data(state, &statuses).await?;
        let reblog_ids: Vec<i64> = reblog_map.values().map(|(rs, _, _)| rs.id).collect();
        let mut enrich_ids = all_ids.clone();
        enrich_ids.extend_from_slice(&reblog_ids);
        let tags_map = batch_statuses_tags(state, &enrich_ids).await?;
        let mentions_map = batch_status_mentions(state, &enrich_ids).await?;
        let all_statuses_for_emoji: Vec<crate::db::models::Status> = statuses
            .iter()
            .cloned()
            .chain(reblog_map.values().map(|(rs, _, _)| rs.clone()))
            .collect();
        let emojis_map = batch_status_emojis(state, &all_statuses_for_emoji).await?;
        let polls_map = batch_status_polls(state, &enrich_ids, Some(viewer)).await?;
        let cards_map = batch_status_cards(state, &enrich_ids, Some(viewer)).await?;
        let viewer_ctxs = super::statuses::batch_viewer_contexts(state, viewer, &all_ids).await?;
        let notif_filter_map =
            super::timelines::compute_filter_results(&state.db, viewer, &statuses).await;
        let all_accounts_for_emoji_v2: Vec<Account> = {
            let mut seen = std::collections::HashSet::new();
            stat_account_map
                .values()
                .chain(reblog_map.values().map(|(_, ra, _)| ra))
                .filter(|a| seen.insert(a.id))
                .cloned()
                .collect()
        };
        let stat_account_emojis_map_v2 =
            batch_account_emojis(state, &all_accounts_for_emoji_v2).await;
        let stat_account_roles_map_v2 =
            batch_account_roles(state, &all_accounts_for_emoji_v2).await;

        let mut map = std::collections::HashMap::new();
        for s in &statuses {
            let Some(account) = stat_account_map.get(&s.account_id) else {
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
            let ctx = viewer_ctxs.get(&s.id).cloned();
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
            api.account.emojis = stat_account_emojis_map_v2
                .get(&account.id)
                .cloned()
                .unwrap_or_default();
            api.account.roles = stat_account_roles_map_v2
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
                rb.account.emojis = stat_account_emojis_map_v2
                    .get(&rb_id)
                    .cloned()
                    .unwrap_or_default();
                rb.account.roles = stat_account_roles_map_v2
                    .get(&rb_id)
                    .cloned()
                    .unwrap_or_default();
                rb.tags = tags_map.get(&rid).cloned().unwrap_or_default();
                rb.mentions = rb_mentions;
                rb.emojis = emojis_map.get(&rid).cloned().unwrap_or_default();
                rb.poll = polls_map.get(&rid).cloned();
                rb.card = cards_map.get(&rid).cloned();
            }
            if let Some(filter_json) = notif_filter_map.get(&s.id) {
                if let Some(arr) = filter_json.as_array() {
                    if !arr.is_empty() {
                        api.filtered = Some(arr.clone());
                    }
                }
            }
            map.insert(s.id, api);
        }
        hydrate_status_stats(state, map.values_mut(), viewer).await;
        map
    } else {
        std::collections::HashMap::new()
    };
    Ok((notif_status_map, status_api_map))
}

/// `NotificationGroup.from_notifications` and `GroupedNotificationsPresenter`
/// under `REST::DedupNotificationGroupSerializer`: one group for each of the
/// `notifications`, the accounts and statuses they name, and, with
/// `partial_avatars`, only each group's first account in full.
#[allow(clippy::too_many_arguments)]
async fn render_groups(
    state: &AppState,
    viewer: i64,
    notifications: &[DbNotification],
    range: Option<&PageRange>,
    grouped_types: Option<&[String]>,
    partial_avatars: bool,
    supported_types: Option<&[String]>,
) -> AppResult<NotificationGroupsResponse> {
    let grouped = |n: &DbNotification| {
        n.group_key.is_some()
            && n.r#type.as_deref().is_some_and(|t| match grouped_types {
                Some(types) => types.iter().any(|g| g == t),
                None => GROUPABLE_NOTIFICATION_TYPES.contains(&t),
            })
    };
    let group_keys: Vec<String> = notifications
        .iter()
        .filter(|n| grouped(n))
        .filter_map(|n| n.group_key.clone())
        .collect();
    let groups_data = match notifications.first() {
        Some(first) => load_groups_data(state, first.account_id, &group_keys, range).await?,
        None => std::collections::HashMap::new(),
    };

    let (notif_status_map, status_api_map) =
        notification_statuses(state, viewer, notifications).await?;

    // Every account a group samples.
    let mut account_ids: Vec<i64> = notifications.iter().map(|n| n.from_account_id).collect();
    account_ids.extend(
        groups_data
            .values()
            .flat_map(|g| g.sample_account_ids.iter().copied()),
    );
    account_ids.sort_unstable();
    account_ids.dedup();
    let db_accounts: Vec<Account> = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
        &account_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let api_accounts: std::collections::HashMap<i64, super::types::Account> =
        batch_accounts_to_api(state, &db_accounts)
            .await
            .into_iter()
            .filter_map(|a| a.id.parse::<i64>().ok().map(|id| (id, a)))
            .collect();

    let report_ids: Vec<i64> = notifications
        .iter()
        .filter(|n| {
            n.r#type.as_deref() == Some("admin.report")
                && n.activity_type.as_deref() == Some("Report")
        })
        .filter_map(|n| n.activity_id)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let report_map = fetch_reports_map(state, &report_ids).await?;

    let mut groups = Vec::with_capacity(notifications.len());
    let mut group_samples: Vec<Vec<i64>> = Vec::with_capacity(notifications.len());
    let mut status_order: Vec<i64> = Vec::new();
    for n in notifications {
        let target_sid = notif_status_map.get(&n.id).copied();
        // A notification about a status that is gone would be a group with
        // no status, which clients cannot render; Mastodon deletes such
        // notifications with the status.
        if let Some(sid) = target_sid {
            if !status_api_map.contains_key(&sid) {
                continue;
            }
            status_order.push(sid);
        }
        let (group_key, sample_ids, count, most_recent_id, pagination) = match n
            .group_key
            .as_ref()
            .filter(|_| grouped(n))
            .and_then(|key| groups_data.get(key).map(|data| (key, data)))
        {
            Some((key, data)) => (
                key.clone(),
                data.sample_account_ids.clone(),
                data.count,
                data.most_recent_id.unwrap_or(n.id),
                range.map(|_| (data.page_min_id, data.latest_page_notification_at)),
            ),
            None => (
                format!("ungrouped-{}", n.id),
                vec![n.from_account_id],
                1,
                n.id,
                range.map(|_| (Some(n.id), Some(n.created_at))),
            ),
        };
        let sample_ids: Vec<i64> = sample_ids
            .into_iter()
            .filter(|id| api_accounts.contains_key(id))
            .collect();
        let samples: Vec<&super::types::Account> = sample_ids
            .iter()
            .filter_map(|id| api_accounts.get(id))
            .collect();
        let report = n
            .activity_id
            .filter(|_| n.activity_type.as_deref() == Some("Report"))
            .and_then(|rid| report_map.get(&rid))
            .cloned();
        let event = severance_event_of(state, n).await;
        let moderation_warning = moderation_warning_of(state, n).await;
        let collection = collection_of(state, n).await;
        let notification_type = n.r#type.clone().unwrap_or_default();
        let fallback = notification_fallback(
            state,
            &notification_type,
            supported_types,
            &samples,
            report.as_ref(),
            event.as_ref(),
            moderation_warning.as_ref(),
            collection.as_ref(),
        );
        groups.push(NotificationGroup {
            group_key,
            notifications_count: count,
            notification_type,
            most_recent_notification_id: most_recent_id,
            page_min_id: pagination
                .map(|(min_id, _)| min_id.map(|id| id.to_string()).unwrap_or_default()),
            page_max_id: pagination.map(|_| most_recent_id.to_string()),
            latest_page_notification_at: pagination
                .map(|(_, at)| at.map(super::convert::mastodon_date)),
            sample_account_ids: sample_ids.iter().map(i64::to_string).collect(),
            status_id: target_sid.map(|sid| sid.to_string()),
            report,
            event,
            moderation_warning,
            annual_report: None,
            collection,
            fallback,
        });
        group_samples.push(sample_ids);
    }

    // `GroupedNotificationsPresenter#accounts` and `#partial_accounts`.
    let mut seen = std::collections::HashSet::new();
    let full_ids: Vec<i64> = if partial_avatars {
        group_samples
            .iter()
            .filter_map(|s| s.first().copied())
            .filter(|id| seen.insert(*id))
            .collect()
    } else {
        group_samples
            .iter()
            .flatten()
            .copied()
            .filter(|id| seen.insert(*id))
            .collect()
    };
    let partial_ids: Vec<i64> = if partial_avatars {
        let mut partial_seen = std::collections::HashSet::new();
        group_samples
            .iter()
            .flat_map(|s| s.iter().skip(1).copied())
            .filter(|id| partial_seen.insert(*id) && !seen.contains(id))
            .collect()
    } else {
        Vec::new()
    };
    let accounts: Vec<super::types::Account> = full_ids
        .iter()
        .filter_map(|id| api_accounts.get(id).cloned())
        .collect();
    let partial_accounts = partial_avatars.then(|| {
        partial_ids
            .iter()
            .filter_map(|id| api_accounts.get(id))
            .map(|a| PartialAccount {
                id: a.id.clone(),
                acct: a.acct.clone(),
                locked: a.locked,
                bot: a.bot,
                url: a.url.clone(),
                avatar: a.avatar.clone(),
                avatar_static: a.avatar_static.clone(),
                avatar_description: a.avatar_description.clone(),
            })
            .collect()
    });

    let mut status_seen = std::collections::HashSet::new();
    let statuses = status_order
        .into_iter()
        .filter(|sid| status_seen.insert(*sid))
        .filter_map(|sid| status_api_map.get(&sid).cloned())
        .collect();

    Ok(NotificationGroupsResponse {
        notification_groups: groups,
        accounts,
        statuses,
        partial_accounts,
    })
}

/// `Api::V2::NotificationsController#index`.
pub async fn get_notifications_v2(
    state: AppState,
    Query(pagination): Query<PaginationParams>,
    RawQuery(qs): RawQuery,
    uri: Uri,
    req_headers: HeaderMap,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:notifications")?;
    let partial_avatars = expand_accounts_param(qs.as_deref())?;
    let limit = pagination.limit_clamped(40, 80);
    let parse = |v: &Option<String>| v.as_deref().and_then(|s| s.parse::<i64>().ok());
    let max_id = parse(&pagination.max_id);
    let since_id = parse(&pagination.since_id);
    let min_id = parse(&pagination.min_id);

    // `browserable_account_notifications` passes no `from_account_id`.
    let (types, exclude_types, _, include_filtered) = parse_notif_filters(qs.as_deref());
    let filter = Browserable::new(types, exclude_types, None, include_filtered);
    let grouped_types = query_values(qs.as_deref(), "grouped_types");
    let normalized = normalize_grouped_types(grouped_types.as_deref());
    let supported_types = query_values(qs.as_deref(), "supported_types");

    // `to_a_grouped_paginated_by_id`.
    let notifications = if min_id.is_some() {
        let mut page = paginate_groups(
            &state,
            auth.account_id,
            &filter,
            limit,
            PageOrder::Asc,
            max_id,
            min_id,
            normalized.as_deref(),
        )
        .await?;
        page.reverse();
        page
    } else {
        paginate_groups(
            &state,
            auth.account_id,
            &filter,
            limit,
            PageOrder::Desc,
            max_id,
            since_id,
            normalized.as_deref(),
        )
        .await?
    };

    // `load_grouped_notifications`: the page's ids, or, for an incomplete
    // page — the last one — up to `max_id` going up, or from `since_id`
    // going down.
    let range = notifications
        .first()
        .zip(notifications.last())
        .map(|(first, last)| {
            if (notifications.len() as i64) >= limit {
                PageRange {
                    begin: Some(last.id),
                    end: Some(first.id),
                    exclusive: false,
                }
            } else if min_id.is_some() {
                PageRange {
                    begin: Some(last.id),
                    end: max_id,
                    exclusive: true,
                }
            } else {
                PageRange {
                    begin: since_id.map(|id| id + 1),
                    end: Some(first.id),
                    exclusive: false,
                }
            }
        });

    let body = render_groups(
        &state,
        auth.account_id,
        &notifications,
        range.as_ref(),
        grouped_types.as_deref(),
        partial_avatars,
        supported_types.as_deref(),
    )
    .await?;

    let bounds = notifications
        .first()
        .zip(notifications.last())
        .map(|(n, o)| (n.id.to_string(), o.id.to_string()));
    let headers = super::link_headers(
        &req_headers,
        &uri,
        bounds.as_ref().map(|(n, o)| (n.as_str(), o.as_str())),
    );
    Ok((headers, Json(body)))
}

// ── GET /api/v2/notifications/:group_key ─────────────────────────────────

/// `Api::V2::NotificationsController#show`: a notification of the group,
/// from an account not suspended, as a group of its own page.
pub async fn get_notification_group(
    state: AppState,
    Path(group_key): Path<String>,
    RawQuery(qs): RawQuery,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<NotificationGroupsResponse>> {
    auth.require_scope("read:notifications")?;
    // `without_suspended.by_group_key(...).take!`
    let ungrouped_id = match group_key.strip_prefix("ungrouped-") {
        Some(id) => Some(id.parse::<i64>().map_err(|_| AppError::NotFound)?),
        None => None,
    };
    let notification: DbNotification = sqlx::query_as(
        "SELECT n.* FROM notifications n
         JOIN accounts a ON a.id = n.from_account_id
           AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
         WHERE n.account_id = $1
           AND CASE WHEN $2::bigint IS NULL THEN n.group_key = $3 ELSE n.id = $2 END
         ORDER BY n.id DESC LIMIT 1",
    )
    .bind(auth.account_id)
    .bind(ungrouped_id)
    .bind(&group_key)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let supported_types = query_values(qs.as_deref(), "supported_types");
    Ok(Json(
        render_groups(
            &state,
            auth.account_id,
            std::slice::from_ref(&notification),
            None,
            None,
            false,
            supported_types.as_deref(),
        )
        .await?,
    ))
}

// ── POST /api/v2/notifications/:group_key/dismiss ─────────────────────────

pub async fn dismiss_notification_group(
    state: AppState,
    Path(group_key): Path<String>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:notifications")?;
    let notifs = notifications_for_group_key(&state, auth.account_id, &group_key).await?;
    let ids: Vec<i64> = notifs.iter().map(|n| n.id).collect();
    if !ids.is_empty() {
        sqlx::query!(
            "DELETE FROM notifications WHERE account_id = $1 AND id = ANY($2::bigint[])",
            auth.account_id,
            &ids,
        )
        .execute(&state.db)
        .await?;
    }

    Ok(Json(serde_json::json!({})))
}

// ── GET /api/v2/notifications/:group_key/accounts ────────────────────────

pub async fn get_notification_group_accounts(
    state: AppState,
    Path(group_key): Path<String>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<super::types::Account>>> {
    auth.require_scope("read:notifications")?;
    let notifs = notifications_for_group_key(&state, auth.account_id, &group_key).await?;
    if notifs.is_empty() {
        return Err(AppError::NotFound);
    }

    // Distinct source accounts, newest first (the notifications are id DESC).
    let mut ordered_ids: Vec<i64> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for n in &notifs {
        if seen.insert(n.from_account_id) {
            ordered_ids.push(n.from_account_id);
        }
    }

    let accounts: Vec<Account> = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
        &ordered_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let account_map: std::collections::HashMap<i64, Account> =
        accounts.into_iter().map(|a| (a.id, a)).collect();

    let ordered: Vec<Account> = ordered_ids
        .iter()
        .filter_map(|id| account_map.get(id).cloned())
        .collect();
    Ok(Json(batch_accounts_to_api(&state, &ordered).await))
}

// ── GET /api/v1/notifications/unread_count ───────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UnreadCountParams {
    pub limit: Option<i64>,
}

/// The notifications marker's `last_read_id`, if the user has one.
async fn notifications_last_read_id(
    state: &AppState,
    auth: &AuthenticatedUser,
) -> AppResult<Option<i64>> {
    let Some(uid) = auth.user_id else {
        return Ok(None);
    };
    Ok(sqlx::query_scalar!(
        "SELECT NULLIF(last_read_id, 0) FROM markers WHERE user_id = $1 AND timeline = 'notifications'",
        uid,
    )
    .fetch_optional(&state.db)
    .await?
    .flatten())
}

/// `Api::V1::NotificationsController#unread_count`: the browserable
/// notifications past the marker, up to `limit`.
pub async fn get_notifications_unread_count(
    state: AppState,
    RawQuery(qs): RawQuery,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(params): Query<UnreadCountParams>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("read:notifications")?;
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);
    let last_read_id = notifications_last_read_id(&state, &auth).await?;
    let (types, exclude_types, account_id, include_filtered) = parse_notif_filters(qs.as_deref());
    let filter = Browserable::new(types, exclude_types, account_id, include_filtered);
    // `paginate_by_min_id(limit, last_read_id).count`
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
           SELECT 1 FROM notifications n
           JOIN accounts a ON a.id = n.from_account_id
             AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
           WHERE n.account_id = $1
             AND ($3::text[] IS NULL OR n.type = ANY($3::text[]))
             AND ($4::boolean OR NOT n.filtered)
             AND ($5::bigint IS NULL OR n.from_account_id = $5)
             AND ($6::bigint IS NULL OR n.id > $6)
           ORDER BY n.id ASC
           LIMIT $2) page",
    )
    .bind(auth.account_id)
    .bind(limit)
    .bind(filter.types.as_deref())
    .bind(filter.include_filtered)
    .bind(filter.from_account_id)
    .bind(last_read_id)
    .fetch_one(&state.db)
    .await?;
    Ok(Json(serde_json::json!({ "count": count })))
}

/// `Api::V2::NotificationsController#unread_count`: the groups past the
/// marker, up to `limit`, as `paginate_groups_by_min_id` pages them.
pub async fn get_notifications_unread_count_v2(
    state: AppState,
    RawQuery(qs): RawQuery,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(params): Query<UnreadCountParams>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("read:notifications")?;
    let limit = params.limit.unwrap_or(100).clamp(1, 1000);
    let last_read_id = notifications_last_read_id(&state, &auth).await?;
    let (types, exclude_types, _, include_filtered) = parse_notif_filters(qs.as_deref());
    let filter = Browserable::new(types, exclude_types, None, include_filtered);
    let grouped_types =
        normalize_grouped_types(query_values(qs.as_deref(), "grouped_types").as_deref());
    let groups = paginate_groups(
        &state,
        auth.account_id,
        &filter,
        limit,
        PageOrder::Asc,
        None,
        last_read_id,
        grouped_types.as_deref(),
    )
    .await?;
    Ok(Json(serde_json::json!({ "count": groups.len() })))
}

// ── GET /api/v2/notifications/policy ─────────────────────────────────────

/// A `NotificationPolicy` row, or the column defaults (limited accounts and
/// private mentions filtered), in `[not_following, not_followers,
/// new_accounts, private_mentions, limited_accounts, bots]` order.
async fn load_policy(state: &AppState, account_id: i64) -> AppResult<[i32; 6]> {
    Ok(sqlx::query!(
        r#"SELECT for_not_following, for_not_followers, for_new_accounts,
                  for_private_mentions, for_limited_accounts, for_bots
           FROM notification_policies WHERE account_id = $1"#,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .map_or([0, 0, 0, 1, 1, 0], |p| {
        [
            p.for_not_following,
            p.for_not_followers,
            p.for_new_accounts,
            p.for_private_mentions,
            p.for_limited_accounts,
            p.for_bots,
        ]
    }))
}

/// `NotificationPolicy#summarize!`: of the first 100 requests from accounts not
/// suspended, how many there are and how many notifications they hold.
async fn policy_summary(state: &AppState, account_id: i64) -> AppResult<NotificationPolicySummary> {
    let row = sqlx::query!(
        r#"SELECT count(*) AS "requests!", COALESCE(sum(notifications_count), 0)::bigint AS "notifications!"
           FROM (SELECT r.notifications_count FROM notification_requests r
                 JOIN accounts a ON a.id = r.from_account_id
                 WHERE r.account_id = $1 AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                 LIMIT 100) r"#,
        account_id,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(NotificationPolicySummary {
        pending_requests_count: row.requests,
        pending_notifications_count: row.notifications,
    })
}

/// `{ accept: 0, filter: 1, drop: 2 }`.
fn policy_name(v: i32) -> String {
    match v {
        1 => "filter",
        2 => "drop",
        _ => "accept",
    }
    .to_string()
}

fn parse_policy(s: &str) -> AppResult<i32> {
    match s {
        "accept" => Ok(0),
        "filter" => Ok(1),
        "drop" => Ok(2),
        other => Err(AppError::Unprocessable(format!(
            "'{other}' is not a valid policy"
        ))),
    }
}

async fn store_policy(state: &AppState, account_id: i64, p: [i32; 6]) -> AppResult<()> {
    sqlx::query!(
        r#"INSERT INTO notification_policies
             (account_id, for_not_following, for_not_followers, for_new_accounts,
              for_private_mentions, for_limited_accounts, for_bots, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, now(), now())
           ON CONFLICT (account_id) DO UPDATE SET
             for_not_following = $2, for_not_followers = $3, for_new_accounts = $4,
             for_private_mentions = $5, for_limited_accounts = $6, for_bots = $7,
             updated_at = now()"#,
        account_id,
        p[0],
        p[1],
        p[2],
        p[3],
        p[4],
        p[5],
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

// ── /api/v2/notifications/policy ──────────────────────────────────────────

/// `REST::NotificationPolicySerializer`.
pub async fn get_notification_policy(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<NotificationPolicy>> {
    auth.require_scope("read:notifications")?;
    let p = load_policy(&state, auth.account_id).await?;
    Ok(Json(NotificationPolicy {
        for_not_following: policy_name(p[0]),
        for_not_followers: policy_name(p[1]),
        for_new_accounts: policy_name(p[2]),
        for_private_mentions: policy_name(p[3]),
        for_limited_accounts: policy_name(p[4]),
        for_bots: policy_name(p[5]),
        summary: policy_summary(&state, auth.account_id).await?,
    }))
}

#[derive(Debug, Deserialize)]
pub struct UpdateNotificationPolicyForm {
    pub for_not_following: Option<String>,
    pub for_not_followers: Option<String>,
    pub for_new_accounts: Option<String>,
    pub for_private_mentions: Option<String>,
    pub for_limited_accounts: Option<String>,
    pub for_bots: Option<String>,
}

pub async fn update_notification_policy(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    super::extractors::Params(form): super::extractors::Params<UpdateNotificationPolicyForm>,
) -> AppResult<Json<NotificationPolicy>> {
    auth.require_scope("write:notifications")?;
    let mut p = load_policy(&state, auth.account_id).await?;
    for (slot, value) in [
        (0, &form.for_not_following),
        (1, &form.for_not_followers),
        (2, &form.for_new_accounts),
        (3, &form.for_private_mentions),
        (4, &form.for_limited_accounts),
        (5, &form.for_bots),
    ] {
        if let Some(v) = value {
            p[slot] = parse_policy(v)?;
        }
    }
    store_policy(&state, auth.account_id, p).await?;
    get_notification_policy(state, Extension(auth)).await
}

// ── /api/v1/notifications/policy ──────────────────────────────────────────

/// `REST::V1::NotificationPolicySerializer`: anything but accept is a filter.
pub async fn get_notification_policy_v1(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<NotificationPolicyV1>> {
    auth.require_scope("read:notifications")?;
    let p = load_policy(&state, auth.account_id).await?;
    Ok(Json(NotificationPolicyV1 {
        filter_not_following: p[0] != 0,
        filter_not_followers: p[1] != 0,
        filter_new_accounts: p[2] != 0,
        filter_private_mentions: p[3] != 0,
        filter_bots: p[5] != 0,
        summary: policy_summary(&state, auth.account_id).await?,
    }))
}

#[derive(Debug, Deserialize)]
pub struct UpdateNotificationPolicyV1Form {
    pub filter_not_following: Option<super::extractors::FlexBool>,
    pub filter_not_followers: Option<super::extractors::FlexBool>,
    pub filter_new_accounts: Option<super::extractors::FlexBool>,
    pub filter_private_mentions: Option<super::extractors::FlexBool>,
    pub filter_bots: Option<super::extractors::FlexBool>,
}

/// The V1 compatibility setters: true filters, false accepts.
pub async fn update_notification_policy_v1(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    super::extractors::Params(form): super::extractors::Params<UpdateNotificationPolicyV1Form>,
) -> AppResult<Json<NotificationPolicyV1>> {
    auth.require_scope("write:notifications")?;
    let mut p = load_policy(&state, auth.account_id).await?;
    for (slot, value) in [
        (0, form.filter_not_following),
        (1, form.filter_not_followers),
        (2, form.filter_new_accounts),
        (3, form.filter_private_mentions),
        (5, form.filter_bots),
    ] {
        if let Some(v) = value {
            p[slot] = i32::from(v.0);
        }
    }
    store_policy(&state, auth.account_id, p).await?;
    get_notification_policy_v1(state, Extension(auth)).await
}

// ── PATCH /api/v2/notifications/policy ───────────────────────────────────

// ── GET /api/v1/notifications/policy ─────────────────────────────────────────

// ── PATCH /api/v1/notifications/policy ───────────────────────────────────────

// ── GET /api/v1/notifications/requests ───────────────────────────────────

pub async fn get_notification_requests(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(pagination): Query<NotificationPagination>,
    uri: Uri,
    req_headers: HeaderMap,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:notifications")?;
    let limit = pagination.limit.unwrap_or(40).clamp(1, 80);
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

    let rows = sqlx::query!(
        r#"SELECT nr.id, nr.from_account_id, nr.last_status_id, nr.notifications_count, nr.created_at, nr.updated_at
           FROM notification_requests nr
           -- `without_suspended`: none from a suspended or deleting account.
           JOIN accounts a ON a.id = nr.from_account_id
             AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
           WHERE nr.account_id = $1
             AND ($3::bigint IS NULL OR nr.id < $3)
             AND ($4::bigint IS NULL OR nr.id > $4)
             AND ($5::bigint IS NULL OR nr.id > $5)
           ORDER BY CASE WHEN $5::bigint IS NULL THEN -nr.id ELSE nr.id END
           LIMIT $2"#,
        auth.account_id, limit, max_id, since_id, min_id,
    )
    .fetch_all(&state.db)
    .await?;
    let rows = super::timelines::newest_first(min_id, rows);

    // Batch-fetch and enrich all last statuses up front
    let last_status_ids: Vec<i64> = rows
        .iter()
        .filter_map(|r| r.last_status_id)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let mut last_status_map: std::collections::HashMap<i64, super::types::Status> =
        std::collections::HashMap::new();

    if !last_status_ids.is_empty() {
        let ls_statuses: Vec<crate::db::models::Status> = sqlx::query_as!(
            crate::db::models::Status,
            "SELECT * FROM statuses WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL",
            &last_status_ids,
        )
        .fetch_all(&state.db)
        .await?;

        let ls_media_map = batch_status_media(&state, &last_status_ids).await?;
        let ls_reblog_map = batch_reblog_data(&state, &ls_statuses).await?;
        let ls_reblog_ids: Vec<i64> = ls_reblog_map.values().map(|(rs, _, _)| rs.id).collect();
        let mut ls_enrich_ids = last_status_ids.clone();
        ls_enrich_ids.extend_from_slice(&ls_reblog_ids);
        let ls_tags_map = batch_statuses_tags(&state, &ls_enrich_ids).await?;
        let ls_mentions_map = batch_status_mentions(&state, &ls_enrich_ids).await?;
        let ls_all_for_emoji: Vec<crate::db::models::Status> = ls_statuses
            .iter()
            .cloned()
            .chain(ls_reblog_map.values().map(|(rs, _, _)| rs.clone()))
            .collect();
        let ls_emojis_map = batch_status_emojis(&state, &ls_all_for_emoji).await?;
        let ls_polls_map =
            batch_status_polls(&state, &ls_enrich_ids, Some(auth.account_id)).await?;
        let ls_cards_map =
            batch_status_cards(&state, &ls_enrich_ids, Some(auth.account_id)).await?;

        let ls_account_ids: Vec<i64> = ls_statuses
            .iter()
            .map(|s| s.account_id)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        let ls_accounts: Vec<Account> = sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &ls_account_ids,
        )
        .fetch_all(&state.db)
        .await?;
        let ls_account_map: std::collections::HashMap<i64, Account> =
            ls_accounts.into_iter().map(|a| (a.id, a)).collect();
        let ls_all_accounts_for_emoji: Vec<Account> = {
            let mut seen = std::collections::HashSet::new();
            ls_account_map
                .values()
                .chain(ls_reblog_map.values().map(|(_, ra, _)| ra))
                .filter(|a| seen.insert(a.id))
                .cloned()
                .collect()
        };
        let ls_account_emojis_map = batch_account_emojis(&state, &ls_all_accounts_for_emoji).await;
        let ls_account_roles_map = batch_account_roles(&state, &ls_all_accounts_for_emoji).await;

        for s in &ls_statuses {
            let Some(account) = ls_account_map.get(&s.account_id) else {
                continue;
            };
            let media = ls_media_map.get(&s.id).cloned().unwrap_or_default();
            let reblog = ls_reblog_map.get(&s.id).cloned();
            let mentions = ls_mentions_map.get(&s.id).cloned().unwrap_or_default();
            let rb_mentions = reblog
                .as_ref()
                .and_then(|(rs, _, _)| ls_mentions_map.get(&rs.id))
                .cloned()
                .unwrap_or_default();
            let mut api = status_from_db(
                &state.urls,
                s,
                account,
                media,
                reblog,
                None,
                &mentions,
                &rb_mentions,
            );
            api.account.emojis = ls_account_emojis_map
                .get(&account.id)
                .cloned()
                .unwrap_or_default();
            api.account.roles = ls_account_roles_map
                .get(&account.id)
                .cloned()
                .unwrap_or_default();
            api.tags = ls_tags_map.get(&s.id).cloned().unwrap_or_default();
            api.mentions = mentions;
            api.emojis = ls_emojis_map.get(&s.id).cloned().unwrap_or_default();
            api.poll = ls_polls_map.get(&s.id).cloned();
            api.card = ls_cards_map.get(&s.id).cloned();
            if let Some(ref mut rb) = api.reblog {
                let rid: i64 = rb.id.parse().unwrap_or(0);
                let rb_id: i64 = rb.account.id.parse().unwrap_or(0);
                rb.account.emojis = ls_account_emojis_map
                    .get(&rb_id)
                    .cloned()
                    .unwrap_or_default();
                rb.account.roles = ls_account_roles_map
                    .get(&rb_id)
                    .cloned()
                    .unwrap_or_default();
                rb.tags = ls_tags_map.get(&rid).cloned().unwrap_or_default();
                rb.mentions = rb_mentions;
                rb.emojis = ls_emojis_map.get(&rid).cloned().unwrap_or_default();
                rb.poll = ls_polls_map.get(&rid).cloned();
                rb.card = ls_cards_map.get(&rid).cloned();
            }
            last_status_map.insert(s.id, api);
        }
        hydrate_status_stats(&state, last_status_map.values_mut(), auth.account_id).await;
    }

    // Batch-fetch account emojis/roles for notification request senders
    let req_account_ids: Vec<i64> = rows.iter().map(|r| r.from_account_id).collect();
    let req_db_accounts: Vec<Account> = if !req_account_ids.is_empty() {
        sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &req_account_ids
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
    } else {
        vec![]
    };
    let req_acc_emojis_map = batch_account_emojis(&state, &req_db_accounts).await;
    let req_acc_roles_map = batch_account_roles(&state, &req_db_accounts).await;
    let req_acc_stats_map = batch_account_stats(
        &state,
        &req_db_accounts.iter().map(|a| a.id).collect::<Vec<_>>(),
    )
    .await;
    let req_acc_map: std::collections::HashMap<i64, Account> =
        req_db_accounts.into_iter().map(|a| (a.id, a)).collect();

    let mut result: Vec<NotificationRequest> = Vec::with_capacity(rows.len());
    for r in rows {
        let Some(acc) = req_acc_map.get(&r.from_account_id) else {
            continue;
        };
        let last_status = r.last_status_id.and_then(|id| last_status_map.remove(&id));
        let mut api_account = super::convert::account_from_db(&state.urls, acc);
        api_account.emojis = req_acc_emojis_map.get(&acc.id).cloned().unwrap_or_default();
        api_account.roles = req_acc_roles_map.get(&acc.id).cloned().unwrap_or_default();
        if let Some(&(statuses_c, following, followers)) = req_acc_stats_map.get(&acc.id) {
            api_account.statuses_count = statuses_c;
            api_account.following_count = following;
            api_account.followers_count = followers;
        }
        result.push(NotificationRequest {
            id: r.id.to_string(),
            created_at: super::convert::mastodon_date(r.created_at),
            updated_at: super::convert::mastodon_date(r.updated_at),
            notifications_count: r.notifications_count.to_string(),
            last_status,
            account: api_account,
        });
    }

    let bounds = result
        .first()
        .zip(result.last())
        .map(|(n, o)| (n.id.as_str(), o.id.as_str()));
    let resp_headers = super::link_headers(&req_headers, &uri, bounds);
    crate::email_subscriptions::fill(&state, result.iter_mut().map(|n| &mut n.account)).await;
    Ok((resp_headers, Json(result)))
}

// ── POST /api/v1/notifications/requests/:id/accept ───────────────────────

/// `notification_unfilter_jobs:<account id>`: how many of the account's
/// `UnfilterNotificationsWorker`s are yet to finish.
fn unfilter_jobs_key(state: &AppState, account_id: i64) -> String {
    state
        .redis_keys
        .key(format!("notification_unfilter_jobs:{account_id}"))
}

/// `AcceptNotificationRequestService`: let the sender through from now on,
/// count and queue the `UnfilterNotificationsWorker` that brings back what
/// was filtered from it, and drop the request.
pub(crate) async fn accept_request(
    state: &AppState,
    account_id: i64,
    from_account_id: i64,
) -> AppResult<()> {
    sqlx::query!(
        r#"INSERT INTO notification_permissions (account_id, from_account_id, created_at, updated_at)
           VALUES ($1, $2, now(), now())
           ON CONFLICT DO NOTHING"#,
        account_id,
        from_account_id,
    )
    .execute(&state.db)
    .await?;
    // `increment_worker_count!`
    let key = unfilter_jobs_key(state, account_id);
    let mut redis = state.redis_coordination.clone();
    let counted: redis::RedisResult<()> = redis::pipe()
        .cmd("INCRBY")
        .arg(&key)
        .arg(1)
        .ignore()
        .cmd("EXPIRE")
        .arg(&key)
        .arg(30 * 60)
        .ignore()
        .query_async(&mut redis)
        .await;
    if let Err(error) = counted {
        tracing::warn!(%error, "could not count a notification unfiltering job");
    }
    crate::jobs::push(
        state,
        UnfilterNotificationsWorker {
            account_id,
            from_account_id,
        },
    )
    .await;
    sqlx::query!(
        "DELETE FROM notification_requests WHERE account_id = $1 AND from_account_id = $2",
        account_id,
        from_account_id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `UnfilterNotificationsWorker`: the filtered direct mentions from the
/// sender into the recipient's conversations, every notification filtered
/// from the sender let through, and, once the last of the recipient's such
/// jobs is done, `notifications_merged` to the recipient's streams.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UnfilterNotificationsWorker {
    pub account_id: i64,
    pub from_account_id: i64,
}

impl crate::jobs::Job for UnfilterNotificationsWorker {
    const KIND: &'static str = "UnfilterNotificationsWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT;

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        // `return if @from_account.nil? || @recipient.nil?`
        let found = sqlx::query_scalar!(
            r#"SELECT count(*) AS "n!" FROM accounts WHERE id = ANY($1)"#,
            &[self.account_id, self.from_account_id][..],
        )
        .fetch_one(&state.db)
        .await?;
        let wanted = if self.account_id == self.from_account_id {
            1
        } else {
            2
        };
        if found < wanted {
            return Ok(());
        }

        // `push_to_conversations!`, newest first as `find_each(order: :desc)`
        // goes.
        let direct = sqlx::query_scalar!(
            r#"SELECT s.id FROM notifications n
               JOIN mentions m ON n.activity_type = 'Mention' AND m.id = n.activity_id
               JOIN statuses s ON s.id = m.status_id
               WHERE n.account_id = $1 AND n.from_account_id = $2 AND n.filtered
                 AND n."type" = 'mention' AND s.visibility = $3
               ORDER BY n.id DESC"#,
            self.account_id,
            self.from_account_id,
            crate::db::models::vis::DIRECT,
        )
        .fetch_all(&state.db)
        .await?;
        for status_id in direct {
            super::conversations::add_status(state, self.account_id, status_id).await;
        }

        // `unfilter_notifications!`
        sqlx::query!(
            "UPDATE notifications SET filtered = false
             WHERE account_id = $1 AND from_account_id = $2 AND filtered",
            self.account_id,
            self.from_account_id,
        )
        .execute(&state.db)
        .await?;

        // `decrement_worker_count!`, and `push_streaming_event!` for the
        // last one when the recipient is streaming.
        let mut redis = state.redis_coordination.clone();
        let left: i64 = redis::cmd("INCRBY")
            .arg(unfilter_jobs_key(state, self.account_id))
            .arg(-1)
            .query_async(&mut redis)
            .await?;
        if left <= 0 {
            state.streaming.notifications_merged(self.account_id).await;
        }
        Ok(())
    }
}

/// `DismissNotificationRequestService`: drop the request and the
/// notifications it held (`FilteredNotificationCleanupWorker`).
pub(crate) async fn dismiss_request(
    state: &AppState,
    account_id: i64,
    from_account_id: i64,
) -> AppResult<()> {
    sqlx::query!(
        "DELETE FROM notifications WHERE account_id = $1 AND from_account_id = $2 AND filtered",
        account_id,
        from_account_id,
    )
    .execute(&state.db)
    .await?;
    sqlx::query!(
        "DELETE FROM notification_requests WHERE account_id = $1 AND from_account_id = $2",
        account_id,
        from_account_id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

async fn request_sender(state: &AppState, account_id: i64, id: i64) -> AppResult<i64> {
    sqlx::query_scalar!(
        "SELECT from_account_id FROM notification_requests WHERE id = $1 AND account_id = $2",
        id,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

pub async fn accept_notification_request(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:notifications")?;
    let from = request_sender(&state, auth.account_id, id).await?;
    accept_request(&state, auth.account_id, from).await?;
    Ok(Json(serde_json::json!({})))
}

// ── POST /api/v1/notifications/requests/:id/dismiss ──────────────────────

pub async fn dismiss_notification_request(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:notifications")?;
    let from = request_sender(&state, auth.account_id, id).await?;
    dismiss_request(&state, auth.account_id, from).await?;
    Ok(Json(serde_json::json!({})))
}

#[derive(Debug, Deserialize)]
pub struct BulkRequestsForm {
    #[serde(default)]
    pub id: super::extractors::FlexIds,
}

/// `set_requests`: the caller's requests among the `id[]` given.
async fn bulk_senders(state: &AppState, account_id: i64, ids: &[i64]) -> AppResult<Vec<i64>> {
    Ok(sqlx::query_scalar!(
        "SELECT from_account_id FROM notification_requests WHERE account_id = $1 AND id = ANY($2)",
        account_id,
        ids,
    )
    .fetch_all(&state.db)
    .await?)
}

// ── POST /api/v1/notifications/requests/accept_all ───────────────────────

/// `accept_bulk`.
pub async fn accept_all_notification_requests(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    super::extractors::Params(form): super::extractors::Params<BulkRequestsForm>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:notifications")?;
    for from in bulk_senders(&state, auth.account_id, &form.id.0).await? {
        accept_request(&state, auth.account_id, from).await?;
    }
    Ok(Json(serde_json::json!({})))
}

// ── POST /api/v1/notifications/requests/dismiss_all ──────────────────────

/// `dismiss_bulk`.
pub async fn dismiss_all_notification_requests(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    super::extractors::Params(form): super::extractors::Params<BulkRequestsForm>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:notifications")?;
    for from in bulk_senders(&state, auth.account_id, &form.id.0).await? {
        dismiss_request(&state, auth.account_id, from).await?;
    }
    Ok(Json(serde_json::json!({})))
}

// ── GET /api/v1/notifications/requests/merged ────────────────────────────

/// `merged?`: whether every `UnfilterNotificationsWorker` queued for the
/// account has finished.
pub async fn get_notification_requests_merged(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("read:notifications")?;
    let mut redis = state.redis_coordination.clone();
    let pending: Option<i64> = redis::cmd("GET")
        .arg(unfilter_jobs_key(&state, auth.account_id))
        .query_async(&mut redis)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(Json(
        serde_json::json!({ "merged": pending.unwrap_or(0) <= 0 }),
    ))
}

// ── GET /api/v1/notifications/requests/:id ───────────────────────────────

pub async fn get_notification_request(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<NotificationRequest>> {
    auth.require_scope("read:notifications")?;
    let r = sqlx::query!(
        r#"SELECT nr.id, nr.from_account_id, nr.last_status_id, nr.notifications_count, nr.created_at, nr.updated_at
           FROM notification_requests nr
           WHERE nr.id = $1 AND nr.account_id = $2"#,
        id, auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let acc = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        r.from_account_id,
    )
    .fetch_one(&state.db)
    .await?;

    let last_status = fetch_last_status(&state, r.last_status_id).await;
    let mut api_account = super::convert::account_from_db(&state.urls, &acc);
    api_account.emojis = fetch_account_emojis(&state, &acc).await;
    api_account.roles = {
        let m = batch_account_roles(&state, std::slice::from_ref(&acc)).await;
        m.get(&acc.id).cloned().unwrap_or_default()
    };
    apply_account_stats(&state, &mut api_account, acc.id).await;
    crate::email_subscriptions::fill(&state, std::iter::once(&mut api_account)).await;
    Ok(Json(NotificationRequest {
        id: r.id.to_string(),
        created_at: super::convert::mastodon_date(r.created_at),
        updated_at: super::convert::mastodon_date(r.updated_at),
        notifications_count: r.notifications_count.to_string(),
        last_status,
        account: api_account,
    }))
}

// ── Helpers ────────────────────────────────────────────────────────────────

async fn fetch_last_status(
    state: &AppState,
    last_status_id: Option<i64>,
) -> Option<super::types::Status> {
    let status_id = last_status_id?;
    let s = sqlx::query_as!(
        crate::db::models::Status,
        "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        status_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()??;
    let account = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        s.account_id,
    )
    .fetch_one(&state.db)
    .await
    .ok()?;
    let media = fetch_status_media(state, s.id).await.ok()?;
    let reblog = fetch_reblog_data(state, &s).await.ok()?;
    build_status(state, &s, &account, media, reblog, None)
        .await
        .ok()
}

/// Parse `types[]=x`, `types=x`, `exclude_types[]=x`, `exclude_types=x`,
/// `account_id=x`, and `include_filtered=true` from the raw query string.
/// Returns (types, exclude_types, account_id, include_filtered).
#[allow(clippy::type_complexity)]
fn parse_notif_filters(
    qs: Option<&str>,
) -> (Option<Vec<String>>, Option<Vec<String>>, Option<i64>, bool) {
    let pairs: Vec<(std::borrow::Cow<str>, std::borrow::Cow<str>)> =
        url::form_urlencoded::parse(qs.unwrap_or("").as_bytes()).collect();

    let collect_arr = |plain: &str, bracket: &str| -> Option<Vec<String>> {
        let v: Vec<String> = pairs
            .iter()
            .filter(|(k, _)| k == plain || k == bracket)
            .map(|(_, v)| v.to_string())
            .collect();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    };

    let types = collect_arr("types", "types[]");
    let exclude_types = collect_arr("exclude_types", "exclude_types[]");
    let account_id = pairs
        .iter()
        .find(|(k, _)| k == "account_id")
        .and_then(|(_, v)| v.parse::<i64>().ok());
    let include_filtered = pairs
        .iter()
        .find(|(k, _)| k == "include_filtered")
        .map(|(_, v)| matches!(v.as_ref(), "true" | "1"))
        .unwrap_or(false);

    (types, exclude_types, account_id, include_filtered)
}

async fn build_notification(
    state: &AppState,
    n: &DbNotification,
    supported_types: Option<&[String]>,
) -> AppResult<Notification> {
    let from_account = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = $1",
        n.from_account_id
    )
    .fetch_one(&state.db)
    .await?;

    let resolved_status_id = batch_notification_status_ids(state, &[n.id]).await;
    let status = if let Some(status_id) = resolved_status_id.get(&n.id).copied() {
        let s = sqlx::query_as!(
            crate::db::models::Status,
            "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
            status_id
        )
        .fetch_optional(&state.db)
        .await?;

        if let Some(s) = s {
            let account = sqlx::query_as!(
                Account,
                "SELECT * FROM accounts WHERE id = $1",
                s.account_id
            )
            .fetch_one(&state.db)
            .await?;
            let media = fetch_status_media(state, s.id).await?;
            let reblog = fetch_reblog_data(state, &s).await?;
            Some(build_status(state, &s, &account, media, reblog, None).await?)
        } else {
            None
        }
    } else {
        None
    };

    let report = match (
        n.r#type.as_deref(),
        n.activity_type.as_deref(),
        n.activity_id,
    ) {
        (Some("admin.report"), Some("Report"), Some(rid)) => {
            fetch_reports_map(state, &[rid]).await?.remove(&rid)
        }
        _ => None,
    };

    let mut notif_account = account_from_db(&state.urls, &from_account);
    notif_account.emojis = fetch_account_emojis(state, &from_account).await;
    notif_account.roles = {
        let m = batch_account_roles(state, std::slice::from_ref(&from_account)).await;
        m.get(&from_account.id).cloned().unwrap_or_default()
    };
    apply_account_stats(state, &mut notif_account, from_account.id).await;
    crate::email_subscriptions::fill(state, std::iter::once(&mut notif_account)).await;
    let notification_type = n.r#type.clone().unwrap_or_default();
    let event = severance_event_of(state, n).await;
    let moderation_warning = moderation_warning_of(state, n).await;
    let collection = collection_of(state, n).await;
    let fallback = notification_fallback(
        state,
        &notification_type,
        supported_types,
        &[&notif_account],
        report.as_ref(),
        event.as_ref(),
        moderation_warning.as_ref(),
        collection.as_ref(),
    );
    Ok(Notification {
        id: n.id.to_string(),
        notification_type,
        created_at: super::convert::mastodon_date(n.created_at),
        group_key: n
            .group_key
            .clone()
            .unwrap_or_else(|| format!("ungrouped-{}", n.id)),
        account: notif_account,
        status,
        report,
        filtered: n.filtered.then_some(true),
        event,
        moderation_warning,
        fallback,
        collection,
    })
}
