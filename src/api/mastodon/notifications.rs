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
    let notification = build_notification(state, &n).await.ok()?;
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
        hydrate_status_stats(&state, map.values_mut()).await;
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
        result.push(Notification {
            id: n.id.to_string(),
            notification_type: n.r#type.clone().unwrap_or_default(),
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
            event: severance_event_of(&state, n).await,
            moderation_warning: moderation_warning_of(&state, n).await,
            fallback: None,
            collection: collection_of(&state, n).await,
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
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Notification>> {
    auth.require_scope("read:notifications")?;
    let n = sqlx::query_as!(
        DbNotification,
        "SELECT * FROM notifications WHERE id = $1 AND account_id = $2",
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    build_notification(&state, &n).await.map(Json)
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

/// Compute a notification's group key, mirroring Mastodon's
/// `Notification::Groups`: `favourite`/`reblog` group by their target status,
/// `follow`/`admin.sign_up` group by type, everything else stays ungrouped.
/// (The 12h hour-bucket split Mastodon adds is omitted; same-target
/// notifications simply share one group.)
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

fn notification_group_key(
    notif_type: &str,
    target_status_id: Option<i64>,
    notif_id: i64,
) -> String {
    // Only reached for rows written before group keys were stored; a stored key
    // is preferred wherever one exists.
    match group_type_prefix(notif_type, target_status_id) {
        Some(prefix) => prefix,
        None => format!("ungrouped-{notif_id}"),
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
    // A stored key identifies its members directly; matching by type and status
    // instead would gather every group of that shape, ignoring the time bucket
    // that separated them.
    let stored: Vec<DbNotification> = sqlx::query_as(
        "SELECT * FROM notifications
         WHERE account_id = $1 AND group_key = $2 ORDER BY id DESC",
    )
    .bind(account_id)
    .bind(group_key)
    .fetch_all(&state.db)
    .await?;
    if !stored.is_empty() {
        return Ok(stored);
    }

    // Rows written before keys were stored have none, so fall back to the shape
    // the key describes.
    if let Some(prefix) = group_key.rsplit_once('-').map(|(head, _)| head) {
        if prefix == "follow" || prefix == "admin.sign_up" {
            return Ok(sqlx::query_as(
                "SELECT * FROM notifications
                 WHERE account_id = $1 AND type = $2 AND group_key IS NULL
                 ORDER BY id DESC",
            )
            .bind(account_id)
            .bind(prefix)
            .fetch_all(&state.db)
            .await?);
        }
    }
    Ok(Vec::new())
}

pub async fn get_notifications_v2(
    state: AppState,
    Query(pagination): Query<PaginationParams>,
    RawQuery(qs): RawQuery,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<NotificationGroupsResponse>> {
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

    let expand_accounts = qs
        .as_deref()
        .and_then(|q| {
            q.split('&').find_map(|part| {
                let (k, v) = part.split_once('=')?;

                if k == "expand_accounts" {
                    Some(v.to_string())
                } else {
                    None
                }
            })
        })
        .unwrap_or_default();

    let (types, exclude_types, account_id, include_filtered) = parse_notif_filters(qs.as_deref());
    let exclude_filtered = !include_filtered && account_id.is_none();

    let notifications: Vec<DbNotification> = sqlx::query_as(
        r#"SELECT n.* FROM notifications n
           JOIN accounts a ON a.id = n.from_account_id AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
           WHERE n.account_id = $1
             AND ($2::bigint IS NULL OR n.id < $2)
             AND ($3::bigint IS NULL OR n.id > $3)
             AND ($5::text[] IS NULL OR n.type = ANY($5))
             AND ($6::text[] IS NULL OR NOT (n.type = ANY($6)))
             AND ($7::bigint IS NULL OR n.from_account_id = $7)
             AND (NOT $8::boolean OR NOT n.filtered)
             AND ($9::bigint IS NULL OR n.id > $9)
           -- `to_a_grouped_paginated_by_id`: with `min_id`, the oldest past it.
           ORDER BY CASE WHEN $9::bigint IS NULL THEN -n.id ELSE n.id END
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
    .bind(min_id)
    .fetch_all(&state.db)
    .await?;
    let notifications = crate::api::mastodon::timelines::newest_first(min_id, notifications);

    // Batch-fetch from_accounts
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
    let from_account_stats_map_v2 = batch_account_stats(
        &state,
        &from_account_map.keys().copied().collect::<Vec<_>>(),
    )
    .await;
    let (from_account_emojis_map_v2, from_account_roles_map_v2) = {
        let accs: Vec<Account> = from_account_map.values().cloned().collect();
        (
            batch_account_emojis(&state, &accs).await,
            batch_account_roles(&state, &accs).await,
        )
    };

    // Batch-fetch reports for admin.report groups (via activity_id/activity_type)
    let report_ids_v2: Vec<i64> = notifications
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
    let report_map_v2: std::collections::HashMap<i64, super::types::Report> =
        if !report_ids_v2.is_empty() {
            fetch_reports_map(&state, &report_ids_v2).await?
        } else {
            std::collections::HashMap::new()
        };

    // Batch-fetch statuses
    let notif_ids_v2: Vec<i64> = notifications.iter().map(|n| n.id).collect();
    let notif_status_map_v2 = batch_notification_status_ids(&state, &notif_ids_v2).await;
    let notif_status_ids: Vec<i64> = notif_status_map_v2
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
            batch_account_emojis(&state, &all_accounts_for_emoji_v2).await;
        let stat_account_roles_map_v2 =
            batch_account_roles(&state, &all_accounts_for_emoji_v2).await;

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
        hydrate_status_stats(&state, map.values_mut()).await;
        map
    } else {
        std::collections::HashMap::new()
    };

    // Build accounts and statuses deduplicated maps for the response
    let mut accounts_map: std::collections::HashMap<String, super::types::Account> =
        std::collections::HashMap::new();
    for a in from_account_map.values() {
        let mut api_account = account_from_db(&state.urls, a);
        api_account.emojis = from_account_emojis_map_v2
            .get(&a.id)
            .cloned()
            .unwrap_or_default();
        api_account.roles = from_account_roles_map_v2
            .get(&a.id)
            .cloned()
            .unwrap_or_default();
        if let Some(&(statuses_c, following, followers)) = from_account_stats_map_v2.get(&a.id) {
            api_account.statuses_count = statuses_c;
            api_account.following_count = following;
            api_account.followers_count = followers;
        }
        accounts_map.insert(a.id.to_string(), api_account);
    }
    let mut statuses_resp_map: std::collections::HashMap<String, super::types::Status> =
        std::collections::HashMap::new();

    // Aggregate notifications into groups (Mastodon Notification::Groups). The
    // list is ordered id DESC, so the first time a group key is seen is its
    // most-recent member (the group's representative).
    struct GroupAcc {
        rep_index: usize,
        count: i64,
        sample_account_ids: Vec<String>,
        page_min_id: i64,
    }
    let mut order: Vec<String> = Vec::new();
    let mut acc_map: std::collections::HashMap<String, GroupAcc> = std::collections::HashMap::new();
    for (idx, n) in notifications.iter().enumerate() {
        let target_sid = notif_status_map_v2.get(&n.id).copied();
        // Skip a status-bearing notification whose target status is unavailable
        // (deleted). Mastodon cascade-deletes such notifications, so they never
        // reach the client; emitting one here yields a group with no status,
        // which the iOS client cannot render (mention/status/quote require a
        // full post layout) and falls back to an error placeholder.
        if let Some(sid) = target_sid {
            if !status_api_map.contains_key(&sid) {
                continue;
            }
        }
        // The key written when the notification arrived. Computing one now
        // would lose the time bucket, which depends on what had arrived before
        // and cannot be recovered from the row alone; the fallback is only for
        // rows written before keys were stored.
        let gk = n.group_key.clone().unwrap_or_else(|| {
            notification_group_key(n.r#type.as_deref().unwrap_or(""), target_sid, n.id)
        });
        if let Some(a) = acc_map.get_mut(&gk) {
            a.count += 1;
            if a.sample_account_ids.len() < SAMPLE_ACCOUNTS_SIZE {
                a.sample_account_ids.push(n.from_account_id.to_string());
            }
            a.page_min_id = n.id; // DESC order → each later member is older
        } else {
            order.push(gk.clone());
            acc_map.insert(
                gk,
                GroupAcc {
                    rep_index: idx,
                    count: 1,
                    sample_account_ids: vec![n.from_account_id.to_string()],
                    page_min_id: n.id,
                },
            );
        }
    }

    let mut groups = Vec::with_capacity(order.len());
    for gk in &order {
        let a = &acc_map[gk];
        let n = &notifications[a.rep_index];
        let status_id = notif_status_map_v2.get(&n.id).and_then(|sid| {
            if let Some(api) = status_api_map.get(sid) {
                statuses_resp_map.insert(sid.to_string(), api.clone());
                Some(sid.to_string())
            } else {
                None
            }
        });

        let report_id_v2 = if n.activity_type.as_deref() == Some("Report") {
            n.activity_id
        } else {
            None
        };
        let report = report_id_v2
            .and_then(|rid| report_map_v2.get(&rid))
            .cloned();

        groups.push(NotificationGroup {
            group_key: gk.clone(),
            notifications_count: a.count,
            notification_type: n.r#type.clone().unwrap_or_default(),
            most_recent_notification_id: n.id,
            page_max_id: n.id.to_string(),
            page_min_id: a.page_min_id.to_string(),
            latest_page_notification_at: super::convert::mastodon_date(n.created_at),
            sample_account_ids: a.sample_account_ids.clone(),
            status_id,
            report,
            event: severance_event_of(&state, n).await,
            moderation_warning: moderation_warning_of(&state, n).await,
            annual_report: None,
            collection: collection_of(&state, n).await,
            fallback: None,
        });
    }

    let mut accounts_vec: Vec<_> = accounts_map.into_values().collect();
    crate::email_subscriptions::fill(&state, accounts_vec.iter_mut()).await;
    let partial_accounts = if expand_accounts == "partial_avatars" {
        Some(
            accounts_vec
                .iter()
                .map(|a| PartialAccount {
                    id: a.id.clone(),
                    acct: a.acct.clone(),
                    locked: a.locked,
                    bot: a.bot,
                    url: a.url.clone(),
                    avatar: a.avatar.clone(),
                    avatar_static: a.avatar_static.clone(),
                })
                .collect(),
        )
    } else {
        None
    };

    Ok(Json(NotificationGroupsResponse {
        notification_groups: groups,
        accounts: accounts_vec,
        statuses: statuses_resp_map.into_values().collect(),
        partial_accounts,
    }))
}

// ── GET /api/v2/notifications/:group_key ─────────────────────────────────

pub async fn get_notification_group(
    state: AppState,
    Path(group_key): Path<String>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<NotificationGroup>> {
    auth.require_scope("read:notifications")?;
    let notifs = notifications_for_group_key(&state, auth.account_id, &group_key).await?;
    let rep = notifs.first().ok_or(AppError::NotFound)?;

    let report = if rep.r#type.as_deref() == Some("admin.report")
        && rep.activity_type.as_deref() == Some("Report")
    {
        if let Some(rid) = rep.activity_id {
            fetch_reports_map(&state, &[rid]).await?.remove(&rid)
        } else {
            None
        }
    } else {
        None
    };

    let status_id_for_group = batch_notification_status_ids(&state, &[rep.id]).await;
    let sample_account_ids: Vec<String> = notifs
        .iter()
        .take(SAMPLE_ACCOUNTS_SIZE)
        .map(|n| n.from_account_id.to_string())
        .collect();
    let page_max_id = rep.id.to_string();
    let page_min_id = notifs.last().map(|n| n.id).unwrap_or(rep.id).to_string();
    Ok(Json(NotificationGroup {
        group_key: group_key.clone(),
        notifications_count: notifs.len() as i64,
        notification_type: rep.r#type.clone().unwrap_or_default(),
        most_recent_notification_id: rep.id,
        page_max_id,
        page_min_id,
        latest_page_notification_at: super::convert::mastodon_date(rep.created_at),
        sample_account_ids,
        status_id: status_id_for_group.get(&rep.id).map(|s| s.to_string()),
        report,
        event: severance_event_of(&state, rep).await,
        moderation_warning: moderation_warning_of(&state, rep).await,
        annual_report: None,
        collection: collection_of(&state, rep).await,
        fallback: None,
    }))
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

pub async fn get_notifications_unread_count(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(params): Query<UnreadCountParams>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("read:notifications")?;

    let limit = params.limit.unwrap_or(100).clamp(1, 1000);

    // Find last read ID from markers (0 means never read)
    let last_read_id: Option<i64> = if let Some(uid) = auth.user_id {
        sqlx::query_scalar!(
            "SELECT NULLIF(last_read_id, 0) FROM markers WHERE user_id = $1 AND timeline = 'notifications'",
            uid,
        )
        .fetch_optional(&state.db)
        .await?
        .flatten()
    } else {
        None
    };

    let count: i64 = if let Some(last_id) = last_read_id {
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) FROM (
               SELECT 1 FROM notifications n
               JOIN accounts a ON a.id = n.from_account_id AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
               WHERE n.account_id = $1 AND n.id > $2 AND NOT n.filtered LIMIT $3) sub"#,
            auth.account_id,
            last_id,
            limit,
        )
        .fetch_one(&state.db)
        .await?
        .unwrap_or(0)
    } else {
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) FROM (
               SELECT 1 FROM notifications n
               JOIN accounts a ON a.id = n.from_account_id AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
               WHERE n.account_id = $1 AND NOT n.filtered LIMIT $2) sub"#,
            auth.account_id,
            limit,
        )
        .fetch_one(&state.db)
        .await?
        .unwrap_or(0)
    };

    Ok(Json(serde_json::json!({ "count": count })))
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
        hydrate_status_stats(&state, last_status_map.values_mut()).await;
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

/// `AcceptNotificationRequestService`: let the sender through from now on,
/// bring back what was filtered from it, and drop the request.
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
    // `UnfilterNotificationsWorker`.
    sqlx::query!(
        "UPDATE notifications SET filtered = false WHERE account_id = $1 AND from_account_id = $2 AND filtered",
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
    // The `UnfilterNotificationsWorker` that was the last one queued.
    state.streaming.notifications_merged(auth.account_id).await;
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
    let senders = bulk_senders(&state, auth.account_id, &form.id.0).await?;
    for &from in &senders {
        accept_request(&state, auth.account_id, from).await?;
    }
    // Only the last of the `UnfilterNotificationsWorker`s streams.
    if !senders.is_empty() {
        state.streaming.notifications_merged(auth.account_id).await;
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

pub async fn get_notification_requests_merged(
    _state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("read:notifications")?;
    Ok(Json(serde_json::json!({ "merged": true })))
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

async fn build_notification(state: &AppState, n: &DbNotification) -> AppResult<Notification> {
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
    Ok(Notification {
        id: n.id.to_string(),
        notification_type: n.r#type.clone().unwrap_or_default(),
        created_at: super::convert::mastodon_date(n.created_at),
        group_key: format!("ungrouped-{}", n.id),
        account: notif_account,
        status,
        report,
        filtered: None,
        event: severance_event_of(state, n).await,
        moderation_warning: moderation_warning_of(state, n).await,
        fallback: None,
        collection: collection_of(state, n).await,
    })
}
