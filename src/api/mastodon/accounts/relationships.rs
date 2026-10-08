//! Relationships: `/relationships`, follow/unfollow (with reblog/notify/
//! languages settings and federation), and the followers/following lists.

use super::*;

// ── GET /api/v1/accounts/relationships ────────────────────────────────────

pub async fn get_relationships(
    state: AppState,
    RawQuery(qs): RawQuery,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<Relationship>>> {
    auth.require_scope("read:follows")?;
    // serde_urlencoded treats id[]=v1&id[]=v2 as a duplicate field → 400.
    // Parse with form_urlencoded which correctly returns each pair separately.
    let pairs: Vec<(String, String)> =
        url::form_urlencoded::parse(qs.as_deref().unwrap_or("").as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

    let with_suspended = pairs
        .iter()
        .any(|(k, v)| k == "with_suspended" && (v == "true" || v == "1"));

    let mut ids: Vec<i64> = pairs
        .iter()
        .filter(|(k, _)| k == "id[]" || k == "id")
        .filter_map(|(_, v)| v.parse::<i64>().ok())
        .collect();

    if ids.is_empty() {
        return Ok(Json(vec![]));
    }

    // Without with_suspended, filter out suspended accounts (matches Mastodon default)
    if !with_suspended {
        let non_suspended: Vec<i64> = sqlx::query_scalar!(
            "SELECT id FROM accounts WHERE id = ANY($1::bigint[]) AND suspended_at IS NULL AND requested_deletion_at IS NULL",
            &ids,
        )
        .fetch_all(&state.db)
        .await?;
        let allowed: std::collections::HashSet<i64> = non_suspended.into_iter().collect();
        ids.retain(|id| allowed.contains(id));
    }

    if ids.is_empty() {
        return Ok(Json(vec![]));
    }
    let results = batch_build_relationships(&state, auth.account_id, &ids).await?;
    Ok(Json(results))
}

// ── POST /api/v1/accounts/:id/follow ──────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct FollowParams {
    pub reblogs: Option<bool>,
    pub notify: Option<bool>,
    pub languages: Option<Vec<String>>,
}

pub async fn follow_account(
    state: AppState,
    Path(target_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    body: Option<Json<FollowParams>>,
) -> AppResult<Json<Relationship>> {
    auth.require_scope("write:follows")?;
    if auth.account_id == target_id {
        return Err(AppError::Forbidden);
    }
    let params = body.map(|Json(p)| p).unwrap_or_default();
    let requester = fetch_account(&state, auth.account_id).await?;
    let target = fetch_account(&state, target_id).await?;
    follow(
        &state,
        &requester,
        &target,
        FollowOptions {
            reblogs: params.reblogs,
            notify: params.notify,
            languages: params.languages,
            ..Default::default()
        },
    )
    .await?;
    build_relationship(&state, auth.account_id, target_id)
        .await
        .map(Json)
}

/// `FollowService`'s options. `None` leaves an existing follow's setting as
/// it is, and gives a new one Mastodon's default.
#[derive(Debug, Default, Clone)]
pub struct FollowOptions {
    pub reblogs: Option<bool>,
    pub notify: Option<bool>,
    pub languages: Option<Vec<String>>,
    /// Follow at once even when the target approves its followers.
    pub bypass_locked: bool,
    /// Follow past `FollowLimitValidator`'s limit.
    pub bypass_limit: bool,
    /// `FollowMigrationService`: the account the follower moved from, left
    /// once a remote target's `Follow` has been delivered.
    pub migrated_from: Option<i64>,
}

/// What [`follow`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowOutcome {
    /// Already following: the options were changed.
    Updated,
    /// Already requested: the request's options were changed.
    RequestUpdated,
    /// A follow request was made, and sent if the target is remote.
    Requested,
    /// The account now follows the target.
    Followed,
}

/// Mastodon's `FollowService`: `source` follows `target`, or asks to.
///
/// An unavailable target is [`AppError::NotFound`] (`following_not_possible?`);
/// a blocked, blocking, domain-blocked or moved one is
/// [`AppError::Forbidden`] (`following_not_allowed?`).
pub async fn follow(
    state: &AppState,
    source: &Account,
    target: &Account,
    options: FollowOptions,
) -> AppResult<FollowOutcome> {
    let target_id = target.id;
    if target.id == source.id || target.is_unavailable() {
        return Err(AppError::NotFound);
    }
    // `following_not_allowed?`, `domain_not_allowed?` first.
    if let Some(domain) = target.domain.as_deref() {
        if crate::federation::moderation::domain_not_allowed(state, domain).await {
            return Err(AppError::Forbidden);
        }
    }
    if target.moved_to_account_id.is_some() {
        return Err(AppError::Forbidden);
    }
    let blocked_either = sqlx::query_scalar!(
        r#"SELECT 1 FROM blocks
           WHERE (account_id = $1 AND target_account_id = $2)
              OR (account_id = $2 AND target_account_id = $1)
           LIMIT 1"#,
        source.id,
        target_id,
    )
    .fetch_optional(&state.db)
    .await?
    .is_some();
    if blocked_either {
        return Err(AppError::Forbidden);
    }
    if let Some(ref dom) = target.domain {
        // `@source_account.domain_blocking?(@target_account.domain)`. An
        // instance block suspends the domain's accounts, and an unavailable
        // account cannot be followed anyway.
        let domain_blocked = sqlx::query_scalar!(
            r#"SELECT 1 FROM account_domain_blocks WHERE account_id = $2 AND domain = $1
               LIMIT 1"#,
            dom,
            source.id,
        )
        .fetch_optional(&state.db)
        .await?
        .is_some();
        if domain_blocked {
            return Err(AppError::Forbidden);
        }
    }

    // `change_follow_options!`: `Account#follow!` on an existing follow sets
    // only the options it was given.
    let updated = sqlx::query!(
        "UPDATE follows
         SET show_reblogs = COALESCE($3, show_reblogs),
             notify = COALESCE($4, notify),
             languages = CASE WHEN $5::text[] IS NULL THEN languages ELSE $5 END
         WHERE account_id = $1 AND target_account_id = $2",
        source.id,
        target_id,
        options.reblogs,
        options.notify,
        options.languages.as_deref(),
    )
    .execute(&state.db)
    .await?;
    if updated.rows_affected() > 0 {
        return Ok(FollowOutcome::Updated);
    }

    // `change_follow_request_options!`.
    let updated = sqlx::query!(
        "UPDATE follow_requests
         SET show_reblogs = COALESCE($3, show_reblogs),
             notify = COALESCE($4, notify),
             languages = CASE WHEN $5::text[] IS NULL THEN languages ELSE $5 END
         WHERE account_id = $1 AND target_account_id = $2",
        source.id,
        target_id,
        options.reblogs,
        options.notify,
        options.languages.as_deref(),
    )
    .execute(&state.db)
    .await?;
    if updated.rows_affected() > 0 {
        return Ok(FollowOutcome::RequestUpdated);
    }

    // `ActivityTracker.increment('activity:interactions')`, for a follow or
    // request that is new.
    crate::activity_tracker::increment(state, crate::activity_tracker::INTERACTIONS).await;

    let show_reblogs = options.reblogs.unwrap_or(true);
    let notify = options.notify.unwrap_or(false);
    let languages: Vec<String> = options.languages.clone().unwrap_or_default();

    // `mark_home_feed_as_partial! if @source_account.not_following_anyone?`:
    // the first follow leaves the home feed regenerating until the followed
    // account's posts are merged into it.
    let following_anyone = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM follows WHERE account_id = $1) AS "e!""#,
        source.id,
    )
    .fetch_one(&state.db)
    .await?;
    if !following_anyone {
        crate::home_feed::regeneration_in_progress(state, source.id).await;
    }

    // Mastodon FollowLimitValidator: cap new follows/requests. Free up to LIMIT,
    // then max(round(followers * RATIO), LIMIT).
    if !options.bypass_limit {
        const FOLLOW_LIMIT: i64 = 7_500;
        const FOLLOW_RATIO: f64 = 1.1;
        let stats = sqlx::query!(
            "SELECT following_count, followers_count FROM account_stats WHERE account_id = $1",
            source.id,
        )
        .fetch_optional(&state.db)
        .await?;
        let following = stats.as_ref().map(|s| s.following_count).unwrap_or(0);
        let followers = stats.as_ref().map(|s| s.followers_count).unwrap_or(0);
        let limit = if following < FOLLOW_LIMIT {
            FOLLOW_LIMIT
        } else {
            ((followers as f64 * FOLLOW_RATIO).round() as i64).max(FOLLOW_LIMIT)
        };
        if following >= limit {
            return Err(AppError::Unprocessable(format!(
                "Validation failed: You are trying to follow too many people (limit: {limit})"
            )));
        }
    }

    let requester = source;
    // Remote account: always use follow_requests and send a Follow activity.
    if target.domain.is_some() {
        let follow_uri = format!(
            "https://{}/users/{}/follows/{}",
            state.instance.domain,
            requester.username,
            crate::snowflake::next_id()
        );
        sqlx::query!(
            r#"INSERT INTO follow_requests (account_id, target_account_id, show_reblogs, notify, languages, uri, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, now(), now())
               ON CONFLICT (account_id, target_account_id) DO UPDATE SET uri = EXCLUDED.uri"#,
            source.id,
            target_id,
            show_reblogs,
            notify,
            &languages,
            follow_uri,
        )
        .execute(&state.db)
        .await?;

        let has_signing_key = crate::federation::keypair::has_signing_key(state, requester.id)
            .await
            .unwrap_or(false);
        if !has_signing_key {
            tracing::warn!(username = %requester.username, "local account has no private key; cannot deliver Follow");
        }
        if has_signing_key {
            let actor_url =
                crate::federation::tag::account_uri_of(&state.instance.domain, requester);
            let key_id = format!("{}#main-key", actor_url);
            // The target is remote, so it has an actor id to address.
            let target_uri = target.uri.clone().unwrap_or_default();
            let follow_activity =
                crate::federation::activity::follow(&follow_uri, &actor_url, &target_uri)?;
            let inbox = if !target.shared_inbox_url.is_empty() {
                target.shared_inbox_url.clone()
            } else {
                target.inbox_url.clone()
            };
            let inbox = if inbox.is_empty() {
                tracing::warn!(target_uri, "inbox URL missing; re-fetching actor profile");
                match crate::api::ap::inbox::resolve_or_fetch_remote_account(state, &target_uri).await {
                    Err(e) => {
                        tracing::warn!(target_uri, error = %e, "failed to re-fetch actor; dropping Follow");
                        None
                    }
                    Ok(_) => {
                        sqlx::query!(
                            r#"SELECT CASE WHEN shared_inbox_url <> '' THEN shared_inbox_url ELSE inbox_url END AS inbox
                               FROM accounts WHERE uri = $1"#,
                            target_uri,
                        )
                        .fetch_optional(&state.db)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|r| r.inbox)
                        .filter(|s| !s.is_empty())
                    }
                }
            } else {
                Some(inbox)
            };
            if let Some(inbox) = inbox {
                // `ActivityPub::MigratedFollowDeliveryWorker` for a migrated
                // follower, whose delivery leaves the old account.
                let sent = match options.migrated_from {
                    Some(old_target) => {
                        let batch = ojak::deliverer::Batch {
                            tag: Some(crate::federation::delivery::migrated_follow_tag(
                                source.id, old_target,
                            )),
                            ..ojak::deliverer::Batch::default()
                        };
                        crate::federation::delivery::deliver_to_inboxes_tagged(
                            state,
                            follow_activity,
                            vec![inbox],
                            key_id,
                            &batch,
                        )
                        .await
                    }
                    None => {
                        crate::federation::delivery::deliver_to_inboxes(
                            state,
                            follow_activity,
                            vec![inbox],
                            key_id,
                        )
                        .await
                    }
                };
                if let Err(e) = sent {
                    tracing::warn!(error = %e, "failed to enqueue Follow");
                }
            } else {
                tracing::warn!(
                    target_uri,
                    "still no inbox URL after re-fetch; dropping Follow"
                );
            }
        }

        return Ok(FollowOutcome::Requested);
    }

    // Locked target, or a silenced requester, goes through a follow request
    // (Mastodon FollowService: (target.locked? && !bypass_locked) ||
    // source.silenced?).
    if (target.locked && !options.bypass_locked) || requester.silenced_at.is_some() {
        sqlx::query!(
            r#"INSERT INTO follow_requests (account_id, target_account_id, show_reblogs, notify, languages, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, now(), now())"#,
            source.id, target_id, show_reblogs, notify, &languages,
        )
        .execute(&state.db)
        .await?;
        push::create_and_push(
            state,
            target_id,
            source.id,
            "follow_request",
            None,
            format!("{} wants to follow you", requester.display_name),
            requester.acct().clone(),
            crate::api::mastodon::convert::account_avatar_url_for(&state.urls, requester),
        )
        .await;
        return Ok(FollowOutcome::Requested);
    }

    sqlx::query!(
        r#"INSERT INTO follows (account_id, target_account_id, show_reblogs, notify, languages, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, now(), now())"#,
        source.id, target_id, show_reblogs, notify, &languages,
    )
    .execute(&state.db)
    .await?;
    // `AccountStat`'s `update_index('accounts', :account)`.
    crate::search::elasticsearch::indexing::accounts(state, &[source.id, target_id]).await;

    crate::counters::on_follow_created(state, source.id, target_id).await?;

    push::create_and_push(
        state,
        target_id,
        source.id,
        "follow",
        None,
        format!("{} followed you", requester.display_name),
        requester.acct().clone(),
        crate::api::mastodon::convert::account_avatar_url_for(&state.urls, requester),
    )
    .await;

    crate::home_feed::merge_into_home_and_lists(state, target_id, source.id).await;

    Ok(FollowOutcome::Followed)
}

// ── POST /api/v1/accounts/:id/unfollow ────────────────────────────────────

pub async fn unfollow_account(
    state: AppState,
    Path(target_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Relationship>> {
    auth.require_scope("write:follows")?;
    unfollow(&state, auth.account_id, target_id, false).await?;
    build_relationship(&state, auth.account_id, target_id)
        .await
        .map(Json)
}

/// Mastodon's `UnfollowService`: remove the follow, or cancel the request,
/// and tell a remote followee. `skip_unmerge` leaves the followee's posts in
/// the follower's home feed, as a migrated follow does.
pub async fn unfollow(
    state: &AppState,
    follower_id: i64,
    target_id: i64,
    skip_unmerge: bool,
) -> AppResult<()> {
    // List members go with the follow, so the lists holding the followee are
    // read first.
    let list_ids = if skip_unmerge {
        Vec::new()
    } else {
        crate::home_feed::lists_with_account(state, follower_id, target_id).await
    };
    let deleted = sqlx::query!(
        "DELETE FROM follows WHERE account_id = $1 AND target_account_id = $2 RETURNING uri",
        follower_id,
        target_id,
    )
    .fetch_optional(&state.db)
    .await?;

    let follow_uri_opt: Option<String> = if let Some(ref d) = deleted {
        // `AccountStat`'s `update_index('accounts', :account)`.
        crate::search::elasticsearch::indexing::accounts(state, &[follower_id, target_id]).await;
        crate::counters::on_follow_removed(state, follower_id, target_id).await?;
        d.uri.clone()
    } else {
        // Canceling a pending request: keep its uri so the Undo(Follow)
        // references the original Follow activity (matches Mastodon).
        let cancelled = sqlx::query!(
            "DELETE FROM follow_requests WHERE account_id = $1 AND target_account_id = $2 RETURNING uri",
            follower_id,
            target_id,
        )
        .fetch_optional(&state.db)
        .await?;
        if cancelled.is_some() {
            // Mirror Mastodon's FollowRequest dependent: :destroy — clear the
            // recipient's follow_request notification for the cancelled request.
            sqlx::query!(
                "DELETE FROM notifications WHERE account_id = $1 AND from_account_id = $2 AND type = 'follow_request'",
                target_id,
                follower_id,
            )
            .execute(&state.db)
            .await?;
        }
        cancelled.and_then(|r| r.uri)
    };

    // `UnmergeWorker`s of the ex-followee's posts out of the home feed and the
    // lists that held it. Only when an accepted follow was removed; a
    // cancelled request never fanned anything out.
    if deleted.is_some() && !skip_unmerge {
        crate::home_feed::unmerge_from_home_and_lists(state, target_id, follower_id, list_ids)
            .await;
    }

    // Send Undo(Follow) to remote target
    let target = fetch_account(state, target_id).await?;
    if target.domain.is_some() {
        let requester = fetch_account(state, follower_id).await?;
        if crate::federation::keypair::has_signing_key(state, requester.id)
            .await
            .unwrap_or(false)
        {
            let actor_url =
                crate::federation::tag::account_uri_of(&state.instance.domain, &requester);
            let key_id = format!("{}#main-key", actor_url);
            let follow_uri = follow_uri_opt.clone().unwrap_or_else(|| actor_url.clone());
            let undo_id = format!(
                "https://{}/activities/{}",
                state.instance.domain,
                crate::snowflake::next_id()
            );
            let undo = crate::federation::activity::undo_follow(
                &undo_id,
                &actor_url,
                &follow_uri,
                &actor_url,
                &target.uri.clone().unwrap_or_default(),
            )?;
            let inbox = if !target.shared_inbox_url.is_empty() {
                target.shared_inbox_url.clone()
            } else {
                target.inbox_url.clone()
            };
            if !inbox.is_empty() {
                if let Err(e) = crate::federation::delivery::deliver_to_inboxes(
                    state,
                    undo,
                    vec![inbox],
                    key_id,
                )
                .await
                {
                    tracing::warn!(error = %e, "failed to enqueue Undo(Follow)");
                }
            }
        }
    }

    Ok(())
}

pub async fn get_account_followers(
    state: AppState,
    Path(id): Path<i64>,
    uri: Uri,
    req_headers: HeaderMap,
    Query(q): Query<FollowersQuery>,
    viewer: Option<Extension<AuthenticatedUser>>,
) -> AppResult<impl IntoResponse> {
    let target = fetch_account(&state, id).await?;
    if target.is_unavailable() {
        return Ok((HeaderMap::new(), Json(Vec::<ApiAccount>::new())));
    }
    let viewer_id = viewer.map(|Extension(a)| a.account_id);
    // Respect hide_collections unless the viewer is the account owner
    if target.hide_collections.unwrap_or(false) && viewer_id != Some(id) {
        return Ok((HeaderMap::new(), Json(Vec::<ApiAccount>::new())));
    }
    // If target has blocked the viewer, return empty list
    if let Some(vid) = viewer_id {
        if vid != id {
            let blocked = sqlx::query_scalar!(
                "SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2",
                id,
                vid,
            )
            .fetch_optional(&state.db)
            .await?
            .is_some();
            if blocked {
                return Ok((HeaderMap::new(), Json(Vec::<ApiAccount>::new())));
            }
        }
    }

    let limit = q.pagination.limit_clamped(40, 80);
    let max_id = q
        .pagination
        .max_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let since_id = q
        .pagination
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let min_id = q
        .pagination
        .min_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());

    // Paginate by follow.id (matching Mastodon's Follow.paginate_by_max_id)
    let follow_rows = sqlx::query!(
        r#"SELECT f.id as follow_id, f.account_id FROM follows f
           JOIN accounts a ON a.id = f.account_id
           WHERE f.target_account_id = $1
             AND ($2::bigint IS NULL OR f.id < $2)
             AND ($3::bigint IS NULL OR f.id > $3)
             AND ($6::bigint IS NULL OR f.id > $6)
             AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
             AND ($4::bigint IS NULL OR NOT EXISTS (
                 SELECT 1 FROM blocks b
                 WHERE (b.account_id = $4 AND b.target_account_id = a.id)
                    OR (b.account_id = a.id AND b.target_account_id = $4)
             ))
             AND ($4::bigint IS NULL OR NOT EXISTS (
                 SELECT 1 FROM mutes WHERE account_id = $4 AND target_account_id = a.id
             ))
           ORDER BY f.id DESC LIMIT $5"#,
        id,
        max_id,
        since_id,
        viewer_id,
        limit,
        min_id
    )
    .fetch_all(&state.db)
    .await?;

    let first_follow_id = follow_rows.first().map(|r| r.follow_id.to_string());
    let last_follow_id = follow_rows.last().map(|r| r.follow_id.to_string());
    let account_ids: Vec<i64> = follow_rows.iter().map(|r| r.account_id).collect();
    let account_map: std::collections::HashMap<i64, Account> = if account_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &account_ids
        )
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect()
    };
    // Preserve follow-id ordering
    let accounts: Vec<Account> = follow_rows
        .iter()
        .filter_map(|r| account_map.get(&r.account_id).cloned())
        .collect();

    let api_accounts = batch_accounts_to_api(&state, &accounts).await;
    let bounds = first_follow_id.zip(last_follow_id);
    let resp_headers = crate::api::mastodon::link_headers(
        &req_headers,
        &uri,
        bounds.as_ref().map(|(n, o)| (n.as_str(), o.as_str())),
    );
    Ok((resp_headers, Json(api_accounts)))
}

// ── GET /api/v1/accounts/:id/following ────────────────────────────────────

pub async fn get_account_following(
    state: AppState,
    Path(id): Path<i64>,
    uri: Uri,
    req_headers: HeaderMap,
    Query(q): Query<FollowersQuery>,
    viewer: Option<Extension<AuthenticatedUser>>,
) -> AppResult<impl IntoResponse> {
    let target = fetch_account(&state, id).await?;
    if target.is_unavailable() {
        return Ok((HeaderMap::new(), Json(Vec::<ApiAccount>::new())));
    }
    let viewer_id = viewer.map(|Extension(a)| a.account_id);
    // Respect hide_collections unless the viewer is the account owner
    if target.hide_collections.unwrap_or(false) && viewer_id != Some(id) {
        return Ok((HeaderMap::new(), Json(Vec::<ApiAccount>::new())));
    }
    // If target has blocked the viewer, return empty list
    if let Some(vid) = viewer_id {
        if vid != id {
            let blocked = sqlx::query_scalar!(
                "SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2",
                id,
                vid,
            )
            .fetch_optional(&state.db)
            .await?
            .is_some();
            if blocked {
                return Ok((HeaderMap::new(), Json(Vec::<ApiAccount>::new())));
            }
        }
    }

    let limit = q.pagination.limit_clamped(40, 80);
    let max_id = q
        .pagination
        .max_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let since_id = q
        .pagination
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let min_id = q
        .pagination
        .min_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());

    // Paginate by follow.id (matching Mastodon's Follow.paginate_by_max_id)
    let follow_rows = sqlx::query!(
        r#"SELECT f.id as follow_id, f.target_account_id FROM follows f
           JOIN accounts a ON a.id = f.target_account_id
           WHERE f.account_id = $1
             AND ($2::bigint IS NULL OR f.id < $2)
             AND ($3::bigint IS NULL OR f.id > $3)
             AND ($6::bigint IS NULL OR f.id > $6)
             AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
             AND ($4::bigint IS NULL OR NOT EXISTS (
                 SELECT 1 FROM blocks b
                 WHERE (b.account_id = $4 AND b.target_account_id = a.id)
                    OR (b.account_id = a.id AND b.target_account_id = $4)
             ))
             AND ($4::bigint IS NULL OR NOT EXISTS (
                 SELECT 1 FROM mutes WHERE account_id = $4 AND target_account_id = a.id
             ))
           ORDER BY f.id DESC LIMIT $5"#,
        id,
        max_id,
        since_id,
        viewer_id,
        limit,
        min_id
    )
    .fetch_all(&state.db)
    .await?;

    let first_follow_id = follow_rows.first().map(|r| r.follow_id.to_string());
    let last_follow_id = follow_rows.last().map(|r| r.follow_id.to_string());
    let account_ids: Vec<i64> = follow_rows.iter().map(|r| r.target_account_id).collect();
    let account_map: std::collections::HashMap<i64, Account> = if account_ids.is_empty() {
        std::collections::HashMap::new()
    } else {
        sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
            &account_ids
        )
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .map(|a| (a.id, a))
        .collect()
    };
    // Preserve follow-id ordering
    let accounts: Vec<Account> = follow_rows
        .iter()
        .filter_map(|r| account_map.get(&r.target_account_id).cloned())
        .collect();

    let api_accounts = batch_accounts_to_api(&state, &accounts).await;
    let bounds = first_follow_id.zip(last_follow_id);
    let resp_headers = crate::api::mastodon::link_headers(
        &req_headers,
        &uri,
        bounds.as_ref().map(|(n, o)| (n.as_str(), o.as_str())),
    );
    Ok((resp_headers, Json(api_accounts)))
}
