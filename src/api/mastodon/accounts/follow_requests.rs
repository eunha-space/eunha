//! Follow requests: list pending inbound follow requests and authorize or
//! reject them (updating relationships and notifying via federation).

use super::*;

// ── GET /api/v1/follow_requests ───────────────────────────────────────────

pub async fn get_follow_requests(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Query(q): Query<PaginationParams>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:follows")?;
    let limit = q.limit_clamped(40, 80);
    let max_id = q.max_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let since_id = q.since_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let min_id = q.min_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    // Paginate by follow_request.id (matching Mastodon's FollowRequest.paginate_by_max_id)
    let rows = sqlx::query!(
        r#"SELECT f.id AS req_id, f.account_id FROM follow_requests f
           WHERE f.target_account_id = $1
             AND ($2::bigint IS NULL OR f.id < $2)
             AND ($3::bigint IS NULL OR f.id > $3)
             AND ($5::bigint IS NULL OR f.id > $5)
           ORDER BY f.id DESC LIMIT $4"#,
        auth.account_id,
        max_id,
        since_id,
        limit,
        min_id
    )
    .fetch_all(&state.db)
    .await?;

    let first_req_id = rows.first().map(|r| r.req_id.to_string());
    let last_req_id = rows.last().map(|r| r.req_id.to_string());
    let account_ids: Vec<i64> = rows.iter().map(|r| r.account_id).collect();

    let accounts = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
        &account_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let account_map: std::collections::HashMap<i64, Account> =
        accounts.into_iter().map(|a| (a.id, a)).collect();
    let accounts_ordered: Vec<Account> = account_ids
        .iter()
        .filter_map(|id| account_map.get(id).cloned())
        .collect();

    let api_accounts = batch_accounts_to_api(&state, &accounts_ordered).await;
    let bounds = first_req_id.zip(last_req_id);
    let resp_headers = crate::api::mastodon::link_headers(
        &req_headers,
        &uri,
        bounds.as_ref().map(|(n, o)| (n.as_str(), o.as_str())),
    );
    Ok((resp_headers, Json(api_accounts)))
}

// ── POST /api/v1/follow_requests/:id/authorize ────────────────────────────

pub async fn authorize_follow_request(
    state: AppState,
    Path(requester_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Relationship>> {
    auth.require_scope("write:follows")?;
    let follow_uri = authorize(&state, requester_id, auth.account_id).await?;

    // Mastodon's FollowRequest has_one :notification, dependent: :destroy —
    // resolving the request removes its follow_request notification so it stops
    // reappearing with Accept/Reject buttons. Run this unconditionally: the
    // follow_requests row may already be gone (e.g. an earlier accept, or a
    // pre-fix orphan) while its notification lingers, so it must be cleared even
    // when nothing was deleted above.
    sqlx::query!(
        "DELETE FROM notifications WHERE account_id = $1 AND from_account_id = $2 AND type = 'follow_request'",
        auth.account_id,
        requester_id,
    )
    .execute(&state.db)
    .await?;

    if let Some(follow_uri) = follow_uri {
        let accepter = fetch_account(&state, auth.account_id).await?;
        let requester = fetch_account(&state, requester_id).await?;
        // Mastodon's authorize action enqueues LocalNotificationWorker for
        // current_account (the accepter), so accepting turns the follow_request
        // notification into a `follow` notification in the accepter's own column
        // — "the requester now follows you". (Mastodon sends no notification to
        // the requester; remote requesters learn via the federated Accept below.)
        push::create_and_push(
            &state,
            auth.account_id,
            requester_id,
            "follow",
            None,
            format!("{} followed you", requester.display_name),
            requester.acct().clone(),
            crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &requester),
        )
        .await;

        // A remote requester always has an actor id to address the Accept to.
        let requester_uri = requester.stored_uri().unwrap_or_default().to_string();
        if let Some(follow_uri) = follow_uri {
            if requester.domain.is_some()
                && !requester_uri.is_empty()
                && crate::federation::keypair::has_signing_key(&state, accepter.id)
                    .await
                    .unwrap_or(false)
            {
                let accepter_actor_url =
                    crate::federation::tag::account_uri_of(&state.instance.domain, &accepter);
                let key_id = format!("{}#main-key", accepter_actor_url);
                let accept_id = format!(
                    "https://{}/activities/{}",
                    state.instance.domain,
                    crate::snowflake::next_id()
                );
                let activity = crate::federation::activity::accept_follow(
                    &accept_id,
                    &accepter_actor_url,
                    &follow_uri,
                    &requester_uri,
                    &accepter_actor_url,
                )?;
                let inbox = requester.inbox_url.clone();
                if inbox.is_empty() {
                    tracing::warn!(
                        requester_uri,
                        "cannot deliver Accept: remote actor has no inbox URL"
                    );
                } else {
                    tracing::debug!(inbox, requester_uri, "enqueueing Accept");
                    if let Err(e) = crate::federation::delivery::deliver_to_inboxes(
                        &state,
                        activity,
                        vec![inbox],
                        key_id,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "failed to enqueue Accept");
                    }
                }
            }
        }
    }

    build_relationship(&state, auth.account_id, requester_id)
        .await
        .map(Json)
}

/// Mastodon's `FollowRequest#authorize!`: the request becomes a follow with
/// the request's options, the requester's list memberships that hung on the
/// request now hang on the follow, and the target's posts are merged into the
/// requester's home feed. `None` when there was no request; otherwise the
/// request's activity id, if it had one.
pub async fn authorize(
    state: &AppState,
    requester_id: i64,
    target_id: i64,
) -> AppResult<Option<Option<String>>> {
    // Move from follow_requests to follows (atomic: delete pending, insert accepted)
    let Some(request) = sqlx::query!(
        "DELETE FROM follow_requests WHERE account_id = $1 AND target_account_id = $2
         RETURNING id, uri, show_reblogs, notify, languages",
        requester_id,
        target_id
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(None);
    };

    let follow_id = sqlx::query_scalar!(
        r#"INSERT INTO follows (account_id, target_account_id, show_reblogs, notify, languages, uri, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, now(), now()) ON CONFLICT DO NOTHING
           RETURNING id"#,
        requester_id,
        target_id,
        request.show_reblogs,
        request.notify,
        request.languages.as_deref(),
        request.uri,
    )
    .fetch_optional(&state.db)
    .await?;
    if let Some(follow_id) = follow_id {
        crate::counters::on_follow_created(&state.db, requester_id, target_id).await?;
        sqlx::query!(
            "UPDATE list_accounts SET follow_request_id = NULL, follow_id = $2
             WHERE follow_request_id = $1",
            request.id,
            follow_id,
        )
        .execute(&state.db)
        .await?;
    }

    // `MergeWorker` into the home feed, which only a local requester has.
    let requester_is_local = sqlx::query_scalar!(
        r#"SELECT domain IS NULL AS "local!" FROM accounts WHERE id = $1"#,
        requester_id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    if requester_is_local {
        let mut redis = state.redis.clone();
        let redis_keys = state.redis_keys.clone();
        let db = state.db.clone();
        if feed::sync_fanout() {
            feed::backfill_follow(&mut redis, &redis_keys, &db, requester_id, target_id).await;
        } else {
            crate::tenants::spawn(async move {
                feed::backfill_follow(&mut redis, &redis_keys, &db, requester_id, target_id).await;
            });
        }
    }

    Ok(Some(request.uri))
}

// ── POST /api/v1/follow_requests/:id/reject ───────────────────────────────

pub async fn reject_follow_request(
    state: AppState,
    Path(requester_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Relationship>> {
    auth.require_scope("write:follows")?;
    let deleted = sqlx::query!(
        "DELETE FROM follow_requests WHERE account_id = $1 AND target_account_id = $2 RETURNING account_id, uri",
        requester_id, auth.account_id
    )
    .fetch_optional(&state.db)
    .await?;

    // Mastodon's FollowRequest has_one :notification, dependent: :destroy —
    // resolving the request removes its follow_request notification so it stops
    // reappearing with Accept/Reject buttons. Run this unconditionally: the
    // follow_requests row may already be gone (e.g. an earlier reject, or a
    // pre-fix orphan) while its notification lingers, so it must be cleared even
    // when nothing was deleted above.
    sqlx::query!(
        "DELETE FROM notifications WHERE account_id = $1 AND from_account_id = $2 AND type = 'follow_request'",
        auth.account_id,
        requester_id,
    )
    .execute(&state.db)
    .await?;

    if let Some(deleted_row) = deleted {
        if let Some(follow_uri) = deleted_row.uri {
            let requester = fetch_account(&state, requester_id).await?;
            let requester_uri = requester.stored_uri().unwrap_or_default().to_string();
            if requester.domain.is_some() && !requester_uri.is_empty() {
                let rejecter = fetch_account(&state, auth.account_id).await?;
                if crate::federation::keypair::has_signing_key(&state, rejecter.id)
                    .await
                    .unwrap_or(false)
                {
                    let rejecter_actor_url =
                        crate::federation::tag::account_uri_of(&state.instance.domain, &rejecter);
                    let key_id = format!("{}#main-key", rejecter_actor_url);
                    let reject_id = format!(
                        "https://{}/activities/{}",
                        state.instance.domain,
                        crate::snowflake::next_id()
                    );
                    let activity = crate::federation::activity::reject_follow(
                        &reject_id,
                        &rejecter_actor_url,
                        &follow_uri,
                        &requester_uri,
                        &rejecter_actor_url,
                    )?;
                    let inbox = requester.inbox_url.clone();
                    if !inbox.is_empty() {
                        if let Err(e) = crate::federation::delivery::deliver_to_inboxes(
                            &state,
                            activity,
                            vec![inbox],
                            key_id,
                        )
                        .await
                        {
                            tracing::warn!(error = %e, "failed to enqueue Reject");
                        }
                    }
                }
            }
        }
    }

    build_relationship(&state, auth.account_id, requester_id)
        .await
        .map(Json)
}
