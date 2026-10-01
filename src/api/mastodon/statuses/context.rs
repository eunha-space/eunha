//! The reply-tree context endpoint (`GET /statuses/:id/context`):
//! ancestors and descendants, with viewer filtering.

use super::*;

// ── GET /api/v1/statuses/:id/context ──────────────────────────────────────

pub async fn get_status_context(
    state: AppState,
    Path(id): Path<i64>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<StatusContext>> {
    let root = sqlx::query_as!(
        DbStatus,
        "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let viewer_id = auth.map(|Extension(a)| a.account_id);

    // Enforce the same visibility rules as GET /api/v1/statuses/:id
    match viewer_id {
        Some(vid) => check_status_visible(&state, &root, vid).await?,
        None => {
            if !matches!(
                root.visibility,
                crate::db::models::vis::PUBLIC | crate::db::models::vis::UNLISTED
            ) {
                return Err(AppError::NotFound);
            }
        }
    }

    // Mastodon limits: authenticated=4096 each; unauthenticated=40 ancestors, 60 descendants (depth 20).
    let (ancestor_limit, descendant_limit, depth_limit): (i64, i64, i64) = if viewer_id.is_some() {
        (4096, 4096, 4096)
    } else {
        (40, 60, 20)
    };

    let ancestor_rows = sqlx::query_as::<_, DbStatus>(
        r#"WITH RECURSIVE ancestor_chain AS (
             SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL
             UNION ALL
             SELECT s.* FROM statuses s
               JOIN ancestor_chain a ON s.id = a.in_reply_to_id
             WHERE s.deleted_at IS NULL
           )
           SELECT * FROM ancestor_chain WHERE id != $1 ORDER BY id ASC LIMIT $2"#,
    )
    .bind(id)
    .bind(ancestor_limit)
    .fetch_all(&state.db)
    .await?;

    // Descendants are ordered by tree path (depth-first pre-order) so each
    // subtree stays contiguous, matching Mastodon's `descendant_ids` ORDER BY path.
    let descendant_rows = sqlx::query_as::<_, DbStatus>(
        r#"WITH RECURSIVE reply_tree(id, path, depth) AS (
             SELECT id, ARRAY[id]::bigint[] AS path, 1::int AS depth FROM statuses
             WHERE in_reply_to_id = $1 AND deleted_at IS NULL
             UNION ALL
             SELECT s.id, r.path || s.id, r.depth + 1 FROM statuses s
               JOIN reply_tree r ON s.in_reply_to_id = r.id
             WHERE s.deleted_at IS NULL AND r.depth < $3 AND NOT s.id = ANY(r.path)
           ),
           bounded AS (SELECT id, path FROM reply_tree ORDER BY path LIMIT $2)
           SELECT s.* FROM statuses s JOIN bounded b ON s.id = b.id ORDER BY b.path"#,
    )
    .bind(id)
    .bind(descendant_limit)
    .bind(depth_limit)
    .fetch_all(&state.db)
    .await?;

    // Collect blocked account IDs for the viewer (batch query, avoids n+1 per status).
    let blocked_accounts: std::collections::HashSet<i64> = if let Some(vid) = viewer_id {
        let all_account_ids: Vec<i64> = ancestor_rows
            .iter()
            .chain(descendant_rows.iter())
            .map(|s| s.account_id)
            .filter(|aid| *aid != vid)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        if all_account_ids.is_empty() {
            Default::default()
        } else {
            sqlx::query_scalar!(
                r#"SELECT target_account_id FROM blocks
                   WHERE account_id = $1 AND target_account_id = ANY($2::bigint[])
                   UNION
                   SELECT account_id FROM blocks
                   WHERE target_account_id = $1 AND account_id = ANY($2::bigint[])"#,
                vid,
                &all_account_ids,
            )
            .fetch_all(&state.db)
            .await
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .collect()
        }
    } else {
        Default::default()
    };

    // Filter by visibility first, then apply "thread" context custom filters.
    let visible_ancestors: Vec<&DbStatus> = ancestor_rows
        .iter()
        .filter(|s| {
            if viewer_id.is_some_and(|vid| vid != s.account_id)
                && blocked_accounts.contains(&s.account_id)
            {
                return false;
            }
            if matches!(
                s.visibility,
                crate::db::models::vis::PRIVATE | crate::db::models::vis::DIRECT
            ) {
                viewer_id.is_some()
            } else {
                true
            }
        })
        .collect();
    let visible_descendants: Vec<&DbStatus> = {
        let filtered = descendant_rows.iter().filter(|s| {
            if viewer_id.is_some_and(|vid| vid != s.account_id)
                && blocked_accounts.contains(&s.account_id)
            {
                return false;
            }
            if matches!(
                s.visibility,
                crate::db::models::vis::PRIVATE | crate::db::models::vis::DIRECT
            ) {
                viewer_id.is_some()
            } else {
                true
            }
        });
        // Mastodon `promote: true` — self-replies (author continuing their own
        // thread) are pulled to the front, preserving relative order (a stable
        // partition), so the OP's thread reads first.
        let (self_replies, others): (Vec<&DbStatus>, Vec<&DbStatus>) =
            filtered.partition(|s| s.in_reply_to_account_id == Some(s.account_id));
        self_replies.into_iter().chain(others).collect()
    };

    // For private/direct: do the per-status visibility check and compute thread filters.
    let anc_owned: Vec<DbStatus> = {
        let mut v = Vec::new();
        for s in &visible_ancestors {
            if matches!(
                s.visibility,
                crate::db::models::vis::PRIVATE | crate::db::models::vis::DIRECT
            ) {
                if let Some(vid) = viewer_id {
                    if check_status_visible(&state, s, vid).await.is_err() {
                        continue;
                    }
                }
            }
            v.push((*s).clone());
        }
        v
    };
    let desc_owned: Vec<DbStatus> = {
        let mut v = Vec::new();
        for s in &visible_descendants {
            if matches!(
                s.visibility,
                crate::db::models::vis::PRIVATE | crate::db::models::vis::DIRECT
            ) {
                if let Some(vid) = viewer_id {
                    if check_status_visible(&state, s, vid).await.is_err() {
                        continue;
                    }
                }
            }
            v.push((*s).clone());
        }
        v
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
            let cards_map = batch_status_cards(&state, &enrich_ids).await?;
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
                if let Some(fj) = filters.get(&s.id) {
                    if let Some(arr) = fj.as_array() {
                        if !arr.is_empty() {
                            api.filtered = Some(arr.clone());
                        }
                    }
                }
                result.push(api);
            }
            hydrate_status_stats(&state, result.iter_mut()).await;
            Ok(result)
        }
    };

    let ancestors = build_batch(anc_owned, anc_filters).await?;
    let descendants = build_batch(desc_owned, desc_filters).await?;

    Ok(Json(StatusContext {
        ancestors,
        descendants,
    }))
}
