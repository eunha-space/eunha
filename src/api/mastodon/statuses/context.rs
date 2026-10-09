//! The reply-tree context endpoint (`GET /statuses/:id/context`):
//! ancestors and descendants, with viewer filtering.

use super::*;

// ── GET /api/v1/statuses/:id/context ──────────────────────────────────────

pub async fn get_status_context(
    state: AppState,
    Path(id): Path<i64>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<axum::response::Response> {
    let root = sqlx::query_as!(
        DbStatus,
        "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let viewer_id = auth.map(|Extension(a)| a.account_id);

    // `set_status`: `authorize @status, :show?`.
    match viewer_id {
        Some(vid) => check_status_visible(&state, &root, vid).await?,
        None => check_status_public(&state, &root).await?,
    }

    // `CONTEXT_LIMIT` for a signed-in viewer, with no depth limit; else
    // `ANCESTORS_LIMIT`, `DESCENDANTS_LIMIT` and `DESCENDANTS_DEPTH_LIMIT`.
    let (ancestors_limit, descendants_limit, depth_limit): (i64, i64, Option<i32>) =
        if viewer_id.is_some() {
            (4096, 4096, None)
        } else {
            (40, 60, Some(20))
        };

    // `ancestor_ids`: the chain from the post replied to up to the root,
    // through posts since deleted too, ordered by path (the nearest
    // first), the nearest `limit` kept, then reversed so the root is first.
    let mut ancestor_ids: Vec<i64> = match root.in_reply_to_id {
        None => vec![],
        Some(parent) => {
            sqlx::query_scalar!(
                r#"WITH RECURSIVE search_tree(id, in_reply_to_id, path) AS (
                 SELECT id, in_reply_to_id, ARRAY[id]
                 FROM statuses
                 WHERE id = $1
               UNION ALL
                 SELECT statuses.id, statuses.in_reply_to_id, path || statuses.id
                 FROM search_tree
                 JOIN statuses ON statuses.id = search_tree.in_reply_to_id
                 WHERE NOT statuses.id = ANY(path)
               )
               SELECT id AS "id!" FROM search_tree ORDER BY path LIMIT $2"#,
                parent,
                ancestors_limit,
            )
            .fetch_all(&state.db)
            .await?
        }
    };
    ancestor_ids.reverse();

    // `descendant_ids`: the reply tree under the post, through posts since
    // deleted too, depth first (ordered by path), at most `limit` and
    // `depth` levels deep (each plus one, for the post itself).
    let descendant_ids: Vec<i64> = sqlx::query_scalar!(
        r#"WITH RECURSIVE search_tree(id, path) AS (
             SELECT id, ARRAY[id]
             FROM statuses
             WHERE id = $1
           UNION ALL
             SELECT statuses.id, path || statuses.id
             FROM search_tree
             JOIN statuses ON statuses.in_reply_to_id = search_tree.id
             WHERE COALESCE(array_length(path, 1) < $3, TRUE) AND NOT statuses.id = ANY(path)
           )
           SELECT id AS "id!" FROM search_tree ORDER BY path LIMIT $2"#,
        id,
        descendants_limit + 1,
        depth_limit.map(|depth| depth + 1),
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .filter(|descendant| *descendant != id)
    .collect();

    // `find_statuses_from_tree_path`: what the viewer may see of them, in
    // that order (`permitted_statuses_from_ids(…, stable: true)`), the
    // author's own replies to themself brought to the top of the
    // descendants (`promote: true`), keeping their order.
    let anc_owned = permitted_statuses(&state, &ancestor_ids, viewer_id).await?;
    let desc_owned: Vec<DbStatus> = {
        let permitted = permitted_statuses(&state, &descendant_ids, viewer_id).await?;
        let (self_replies, others): (Vec<DbStatus>, Vec<DbStatus>) = permitted
            .into_iter()
            .partition(|s| s.in_reply_to_account_id == Some(s.account_id));
        self_replies.into_iter().chain(others).collect()
    };
    let (anc_filters, desc_filters) = if let Some(vid) = viewer_id {
        let af =
            crate::api::mastodon::timelines::compute_filter_results(&state.db, vid, &anc_owned)
                .await;
        let df =
            crate::api::mastodon::timelines::compute_filter_results(&state.db, vid, &desc_owned)
                .await;
        (af, df)
    } else {
        (Default::default(), Default::default())
    };

    // Build ancestors and descendants using batch fetches instead of N+1 queries.
    let build_batch = |statuses: Vec<DbStatus>, filters: HashMap<i64, serde_json::Value>| {
        let state = state.clone();
        async move {
            if statuses.is_empty() {
                return Ok::<Vec<Status>, crate::error::AppError>(vec![]);
            }
            let visible = statuses;
            if visible.is_empty() {
                return Ok(vec![]);
            }

            let account_ids: Vec<i64> = visible
                .iter()
                .map(|s| s.account_id)
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect();
            let accounts_vec: Vec<Account> = sqlx::query_as!(
                Account,
                "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
                &account_ids,
            )
            .fetch_all(&state.db)
            .await?;
            let account_map: HashMap<i64, Account> =
                accounts_vec.into_iter().map(|a| (a.id, a)).collect();

            let all_ids: Vec<i64> = visible.iter().map(|s| s.id).collect();
            let media_map = batch_status_media(&state, &all_ids).await?;
            let reblog_map = batch_reblog_data(&state, &visible).await?;
            let reblog_ids: Vec<i64> = reblog_map.values().map(|(rs, _, _)| rs.id).collect();
            let mut enrich_ids = all_ids.clone();
            enrich_ids.extend_from_slice(&reblog_ids);
            let tags_map = batch_statuses_tags(&state, &enrich_ids).await?;
            let mentions_map = batch_status_mentions(&state, &enrich_ids).await?;
            let all_statuses_for_emoji: Vec<DbStatus> = visible
                .iter()
                .cloned()
                .chain(reblog_map.values().map(|(rs, _, _)| rs.clone()))
                .collect();
            let emojis_map = batch_status_emojis(&state, &all_statuses_for_emoji).await?;
            let polls_map = batch_status_polls(&state, &enrich_ids, viewer_id).await?;
            let cards_map = batch_status_cards(&state, &enrich_ids, viewer_id).await?;
            let viewer_ctxs = if let Some(vid) = viewer_id {
                batch_viewer_contexts(&state, vid, &all_ids).await?
            } else {
                HashMap::new()
            };
            let all_accounts_for_emoji: Vec<Account> = {
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

            let mut result = Vec::with_capacity(visible.len());
            for s in &visible {
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
                api.account.emojis = account_emojis_map
                    .get(&account.id)
                    .cloned()
                    .unwrap_or_default();
                api.account.set_roles(
                    account_roles_map
                        .get(&account.id)
                        .cloned()
                        .unwrap_or_default(),
                );
                api.tags = tags_map.get(&s.id).cloned().unwrap_or_default();
                api.mentions = mentions;
                api.emojis = emojis_map.get(&s.id).cloned().unwrap_or_default();
                api.poll = polls_map.get(&s.id).cloned();
                api.card = cards_map.get(&s.id).cloned();
                if let Some(ref mut rb) = api.reblog {
                    let rid: i64 = rb.id.parse().unwrap_or(0);
                    let rb_id: i64 = rb.account.id.parse().unwrap_or(0);
                    rb.account.emojis = account_emojis_map.get(&rb_id).cloned().unwrap_or_default();
                    rb.account
                        .set_roles(account_roles_map.get(&rb_id).cloned().unwrap_or_default());
                    rb.tags = tags_map.get(&rid).cloned().unwrap_or_default();
                    rb.mentions = rb_mentions;
                    rb.emojis = emojis_map.get(&rid).cloned().unwrap_or_default();
                    rb.poll = polls_map.get(&rid).cloned();
                    rb.card = cards_map.get(&rid).cloned();
                }
                if let Some(fj) = filters.get(&s.id) {
                    if let Some(arr) = fj.as_array() {
                        if !arr.is_empty() {
                            api.filtered = Some(arr.clone());
                        }
                    }
                }
                result.push(api);
            }
            hydrate_status_stats(&state, result.iter_mut(), viewer_id).await;
            Ok(result)
        }
    };

    let ancestors = build_batch(anc_owned, anc_filters).await?;
    let descendants = build_batch(desc_owned, desc_filters).await?;

    let refresh_header = context_refresh(&state, &root, viewer_id.is_some()).await;
    let mut response = Json(StatusContext {
        ancestors,
        descendants,
    })
    .into_response();
    if let Some(value) = refresh_header.and_then(|v| axum::http::HeaderValue::from_str(&v).ok()) {
        response
            .headers_mut()
            .insert(crate::async_refresh::HEADER, value);
    }
    Ok(response)
}

/// `Status.permitted_statuses_from_ids(ids, account, stable: true)`: the
/// posts of `ids` still there that `StatusFilter` lets the viewer (none for
/// one signed out) see, in the order of `ids`. A post of the viewer's own is
/// never filtered; any other is when `StatusPolicy#show?` refuses it (its
/// author unavailable; a direct or limited post not mentioning the viewer; a
/// private one of an author they do not follow and that does not mention
/// them; any other whose author blocks them), when the viewer blocks or
/// mutes its author or blocks its author's domain, or when its author is
/// silenced and not followed by the viewer.
pub(crate) async fn permitted_statuses(
    state: &AppState,
    ids: &[i64],
    viewer: Option<i64>,
) -> AppResult<Vec<DbStatus>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    let rows = sqlx::query_as!(
        DbStatus,
        r#"SELECT s.* FROM statuses s JOIN accounts a ON a.id = s.account_id
           WHERE s.id = ANY($1) AND s.deleted_at IS NULL
             AND (s.account_id = $2 OR (
               a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
               AND (CASE
                 WHEN s.visibility IN (3, 4) THEN
                   EXISTS (SELECT 1 FROM mentions m WHERE m.status_id = s.id AND m.account_id = $2)
                 WHEN s.visibility = 2 THEN
                   EXISTS (SELECT 1 FROM follows f
                           WHERE f.account_id = $2 AND f.target_account_id = s.account_id)
                   OR EXISTS (SELECT 1 FROM mentions m WHERE m.status_id = s.id AND m.account_id = $2)
                 ELSE NOT EXISTS (SELECT 1 FROM blocks b
                                  WHERE b.account_id = s.account_id AND b.target_account_id = $2)
               END)
               AND NOT EXISTS (SELECT 1 FROM blocks b
                               WHERE b.account_id = $2 AND b.target_account_id = s.account_id)
               AND NOT (a.domain IS NOT NULL AND EXISTS (
                 SELECT 1 FROM account_domain_blocks d WHERE d.account_id = $2 AND d.domain = a.domain))
               AND NOT EXISTS (SELECT 1 FROM mutes mu
                               WHERE mu.account_id = $2 AND mu.target_account_id = s.account_id)
               AND NOT (a.silenced_at IS NOT NULL AND NOT EXISTS (
                 SELECT 1 FROM follows f WHERE f.account_id = $2 AND f.target_account_id = s.account_id))
             ))"#,
        ids,
        viewer,
    )
    .fetch_all(&state.db)
    .await?;
    let mut by_id: HashMap<i64, DbStatus> = rows.into_iter().map(|s| (s.id, s)).collect();
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

/// The context controller's async refresh: report the reply fetch already
/// running for this status, or, for a signed-in viewer of a remote status
/// whose replies are due a fetch, start one.
async fn context_refresh(state: &AppState, root: &DbStatus, signed_in: bool) -> Option<String> {
    use crate::async_refresh::AsyncRefresh;
    use crate::federation::replies;

    let key = replies::refresh_key(root.id);
    let refresh = AsyncRefresh::new(state, &key).await;
    if refresh.is_running() {
        return refresh.header_value(state, 3);
    }
    if !signed_in || !replies::should_fetch_replies(root) {
        return None;
    }
    let refresh = AsyncRefresh::create(state, &key, true).await;
    replies::fetch_all_replies(state, root.id, key).await;
    refresh.header_value(state, 3)
}
