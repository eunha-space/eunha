//! Mutes and blocks: mute/unmute (with optional notification muting and
//! duration), block/unblock (with follow teardown + federation), and the
//! `/blocks` and `/mutes` list endpoints.

use super::*;

// ── POST /api/v1/accounts/:id/mute ────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct MuteParams {
    /// Whether to also mute notifications from this account (default true).
    pub notifications: Option<bool>,
    /// Mute duration in seconds; 0 or absent means indefinite.
    pub duration: Option<i64>,
}

pub async fn mute_account(
    state: AppState,
    Path(target_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    body: Option<Json<MuteParams>>,
) -> AppResult<Json<Relationship>> {
    auth.require_scope("write:mutes")?;
    // Mastodon MuteService: muting yourself is a no-op.
    if auth.account_id == target_id {
        return build_relationship(&state, auth.account_id, target_id)
            .await
            .map(Json);
    }
    let params = body.map(|Json(p)| p).unwrap_or_default();
    let hide_notifications = params.notifications.unwrap_or(true);
    mute(
        &state,
        auth.account_id,
        target_id,
        hide_notifications,
        params.duration.unwrap_or(0),
    )
    .await?;

    build_relationship(&state, auth.account_id, target_id)
        .await
        .map(Json)
}

/// Mastodon's `MuteService`: `account_id` mutes `target_id`, its
/// notifications too when `hide_notifications`, for `duration` seconds or,
/// at 0, until unmuted.
pub async fn mute(
    state: &AppState,
    account_id: i64,
    target_id: i64,
    hide_notifications: bool,
    duration: i64,
) -> AppResult<()> {
    if account_id == target_id {
        return Ok(());
    }
    // `mute.expires_in = duration.zero? ? nil : duration`: any duration but
    // nought expires, a negative one at once.
    let expires_at: Option<chrono::NaiveDateTime> = Some(duration)
        .filter(|&d| d != 0)
        .map(|d| chrono::Utc::now().naive_utc() + chrono::Duration::seconds(d));

    let mute_id = sqlx::query_scalar!(
        r#"INSERT INTO mutes (account_id, target_account_id, hide_notifications, expires_at, created_at, updated_at)
           VALUES ($1, $2, $3, $4, now(), now())
           ON CONFLICT (account_id, target_account_id)
           DO UPDATE SET hide_notifications = EXCLUDED.hide_notifications,
                         expires_at = EXCLUDED.expires_at,
                         updated_at = now()
           RETURNING id"#,
        account_id, target_id, hide_notifications, expires_at,
    )
    .fetch_one(&state.db)
    .await?;
    // `BlockWorker` when the notifications are muted too, `MuteWorker`
    // otherwise.
    if hide_notifications {
        queue_block_worker(state, account_id, target_id).await;
    } else {
        queue_mute_worker(state, account_id, target_id).await;
    }
    // `DeleteMuteWorker.perform_at(duration.seconds, mute.id) if duration != 0`.
    if duration != 0 {
        let delay = std::time::Duration::from_secs(duration.max(0).unsigned_abs());
        crate::jobs::push_in(state, delay, DeleteMuteWorker { mute_id }).await;
    }
    Ok(())
}

/// `DeleteMuteWorker`, queued for when a timed mute expires: the mute is
/// lifted by [`unmute`] if it has expired by then. A mute renewed in the
/// meantime is left to the job its renewal queued.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeleteMuteWorker {
    pub mute_id: i64,
}

impl crate::jobs::Job for DeleteMuteWorker {
    const KIND: &'static str = "DeleteMuteWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT;

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        // `mute&.expired?`.
        let expired = sqlx::query!(
            r#"SELECT account_id, target_account_id FROM mutes
               WHERE id = $1 AND expires_at IS NOT NULL AND expires_at < now()"#,
            self.mute_id,
        )
        .fetch_optional(&state.db)
        .await?;
        if let Some(mute) = expired {
            unmute(state, mute.account_id, mute.target_account_id).await?;
        }
        Ok(())
    }
}

// ── POST /api/v1/accounts/:id/unmute ──────────────────────────────────────

pub async fn unmute_account(
    state: AppState,
    Path(target_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Relationship>> {
    auth.require_scope("write:mutes")?;
    unmute(&state, auth.account_id, target_id).await?;
    build_relationship(&state, auth.account_id, target_id)
        .await
        .map(Json)
}

/// A [`DeleteMuteWorker`] for every timed mute that has none, due when it
/// expires: for mutes that arrived without the jobs Mastodon's Sidekiq held
/// for them, as an imported instance's do. Migration 030 did the same for the
/// mutes already there. Returns how many were queued.
pub async fn queue_expiries(db: &sqlx::PgPool) -> sqlx::Result<u64> {
    let queued = sqlx::query!(
        r#"INSERT INTO eunha.jobs (queue, kind, args, run_at, max_retries, keep_dead)
           SELECT 'default', 'DeleteMuteWorker', jsonb_build_object('mute_id', m.id),
                  m.expires_at AT TIME ZONE 'UTC', 25, true
           FROM mutes m
           WHERE m.expires_at IS NOT NULL
             AND NOT EXISTS (
               SELECT 1 FROM eunha.jobs j
               WHERE j.kind = 'DeleteMuteWorker' AND j.dead_at IS NULL
                 AND j.args = jsonb_build_object('mute_id', m.id)
             )"#,
    )
    .execute(db)
    .await?;
    Ok(queued.rows_affected())
}

/// Mastodon's `UnmuteService`.
pub async fn unmute(state: &AppState, account_id: i64, target_id: i64) -> AppResult<()> {
    let unmuted = sqlx::query!(
        "DELETE FROM mutes WHERE account_id = $1 AND target_account_id = $2",
        account_id,
        target_id
    )
    .execute(&state.db)
    .await?;
    // `MergeWorker`s into the home feed and the lists holding the account,
    // when it is followed.
    if unmuted.rows_affected() > 0 {
        let following = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM follows
                              WHERE account_id = $1 AND target_account_id = $2) AS "e!""#,
            account_id,
            target_id,
        )
        .fetch_one(&state.db)
        .await?;
        if following {
            crate::home_feed::merge_into_home_and_lists(state, target_id, account_id).await;
        }
    }
    Ok(())
}

// ── POST /api/v1/accounts/:id/block ───────────────────────────────────────

pub async fn block_account(
    state: AppState,
    Path(target_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Relationship>> {
    auth.require_scope("write:blocks")?;
    block(&state, auth.account_id, target_id).await?;
    build_relationship(&state, auth.account_id, target_id)
        .await
        .map(Json)
}

/// Mastodon's `BlockService`: `account_id` blocks `target_id`, which ends
/// the follows and requests between them, and tells a remote target.
pub async fn block(state: &AppState, account_id: i64, target_id: i64) -> AppResult<()> {
    // Mastodon BlockService: blocking yourself is a no-op.
    if account_id == target_id {
        return Ok(());
    }
    sqlx::query!(
        r#"INSERT INTO blocks (account_id, target_account_id, created_at, updated_at) VALUES ($1, $2, now(), now())
           ON CONFLICT (account_id, target_account_id) DO NOTHING"#,
        account_id, target_id
    )
    .execute(&state.db)
    .await?;

    // `UnfollowService` for each direction unmerges from the lists that held
    // the other account, whose memberships go with the follow.
    let blocker_lists = crate::home_feed::lists_with_account(state, account_id, target_id).await;
    let target_lists = crate::home_feed::lists_with_account(state, target_id, account_id).await;
    // Remove accepted follows in both directions and update counts. Capture the
    // direction + Follow activity uri so we can federate the termination.
    let deleted = sqlx::query!(
        "DELETE FROM follows WHERE (account_id = $1 AND target_account_id = $2) OR (account_id = $2 AND target_account_id = $1) RETURNING account_id, target_account_id, uri",
        account_id, target_id
    )
    .fetch_all(&state.db)
    .await?;
    for row in &deleted {
        // `AccountStat`'s `update_index('accounts', :account)`.
        crate::search::elasticsearch::indexing::accounts(
            state,
            &[row.account_id, row.target_account_id],
        )
        .await;
        let _ =
            crate::counters::on_follow_removed(state, row.account_id, row.target_account_id).await;
    }
    // Also delete any pending follow requests in both directions, keeping uris.
    let deleted_requests = sqlx::query!(
        "DELETE FROM follow_requests WHERE (account_id = $1 AND target_account_id = $2) OR (account_id = $2 AND target_account_id = $1) RETURNING account_id, uri",
        account_id, target_id
    )
    .fetch_all(&state.db)
    .await?;
    // Mirror Mastodon's FollowRequest dependent: :destroy — clear follow_request
    // notifications in both directions between blocker and blocked.
    sqlx::query!(
        "DELETE FROM notifications WHERE type = 'follow_request' AND ((account_id = $1 AND from_account_id = $2) OR (account_id = $2 AND from_account_id = $1))",
        account_id, target_id,
    )
    .execute(&state.db)
    .await?;

    // `UnfollowService`'s `UnmergeWorker`s, for each follow that went.
    for row in &deleted {
        let lists = if row.account_id == account_id {
            blocker_lists.clone()
        } else {
            target_lists.clone()
        };
        crate::home_feed::unmerge_from_home_and_lists(
            state,
            row.target_account_id,
            row.account_id,
            lists,
        )
        .await;
    }

    // `handle_collections`: out of each other's collections.
    crate::api::mastodon::collections::handle_block(state, account_id, target_id).await?;
    // `NotificationPermission.where(account:, from_account: target_account)
    // .destroy_all`: the blocked account's private mentions are filtered
    // again.
    sqlx::query!(
        "DELETE FROM notification_permissions WHERE account_id = $1 AND from_account_id = $2",
        account_id,
        target_id,
    )
    .execute(&state.db)
    .await?;

    // `BlockWorker.perform_async(account.id, target_account.id)`.
    queue_block_worker(state, account_id, target_id).await;

    // Federate to a remote target (Mastodon BlockService#handle_following_relationships
    // + the Block itself): Undo(Follow) for our follow, Reject(Follow) for their
    // follow / pending request.
    if let Some(target) = sqlx::query!(
        "SELECT uri, inbox_url, shared_inbox_url, domain FROM accounts WHERE id = $1",
        target_id,
    )
    .fetch_optional(&state.db)
    .await?
    {
        // A remote target without an actor id cannot be addressed.
        let target_uri = target.uri.clone().unwrap_or_default();
        if target.domain.is_some() && !target_uri.is_empty() {
            if let Some(actor_row) = sqlx::query!(
                "SELECT username, id_scheme FROM accounts WHERE id = $1 AND domain IS NULL",
                account_id,
            )
            .fetch_optional(&state.db)
            .await?
            {
                if crate::federation::keypair::has_signing_key(state, account_id)
                    .await
                    .unwrap_or(false)
                {
                    let domain = state.instance.domain.clone();
                    let actor_url = crate::federation::tag::account_uri(
                        &domain,
                        account_id,
                        actor_row.id_scheme,
                        &actor_row.username,
                    );
                    let key_id = format!("{}#main-key", actor_url);
                    let inbox = if !target.shared_inbox_url.is_empty() {
                        target.shared_inbox_url.clone()
                    } else {
                        target.inbox_url.clone()
                    };

                    let activity_id = || {
                        format!(
                            "https://{}/activities/{}",
                            domain,
                            crate::snowflake::next_id()
                        )
                    };
                    let mut activities: Vec<serde_json::Value> = Vec::new();

                    for f in &deleted {
                        let Some(uri) = f.uri.clone().filter(|s| !s.is_empty()) else {
                            continue;
                        };
                        if f.account_id == account_id {
                            // Our follow of the remote target -> Undo(Follow).
                            if let Ok(a) = crate::federation::activity::undo_follow(
                                &activity_id(),
                                &actor_url,
                                &uri,
                                &actor_url,
                                &target_uri,
                            ) {
                                activities.push(a);
                            }
                        } else {
                            // The remote target's follow of us -> Reject(Follow).
                            if let Ok(a) = crate::federation::activity::reject_follow(
                                &activity_id(),
                                &actor_url,
                                &uri,
                                &target_uri,
                                &actor_url,
                            ) {
                                activities.push(a);
                            }
                        }
                    }
                    for r in &deleted_requests {
                        // The remote target's pending request to us -> Reject(Follow).
                        if r.account_id == target_id {
                            if let Some(uri) = r.uri.clone().filter(|s| !s.is_empty()) {
                                if let Ok(a) = crate::federation::activity::reject_follow(
                                    &activity_id(),
                                    &actor_url,
                                    &uri,
                                    &target_uri,
                                    &actor_url,
                                ) {
                                    activities.push(a);
                                }
                            }
                        }
                    }

                    // The Block activity itself.
                    let block_id = format!(
                        "https://{}/users/{}/blocks/{}",
                        domain, actor_row.username, target_id
                    );
                    if let Ok(b) =
                        crate::federation::activity::block(&block_id, &actor_url, &target_uri)
                    {
                        activities.push(b);
                    }

                    if !inbox.is_empty() {
                        for act in activities {
                            if let Err(e) = crate::federation::delivery::deliver_to_inboxes(
                                state,
                                act,
                                vec![inbox.clone()],
                                key_id.clone(),
                            )
                            .await
                            {
                                tracing::warn!(error = %e, "failed to enqueue block-related activity");
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

// ── POST /api/v1/accounts/:id/unblock ─────────────────────────────────────

pub async fn unblock_account(
    state: AppState,
    Path(target_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Relationship>> {
    auth.require_scope("write:blocks")?;
    unblock(&state, auth.account_id, target_id).await?;
    build_relationship(&state, auth.account_id, target_id)
        .await
        .map(Json)
}

/// Mastodon's `UnblockService`: the block goes, and a remote target is told.
pub async fn unblock(state: &AppState, account_id: i64, target_id: i64) -> AppResult<()> {
    // Mastodon UnblockService: a no-op (and no Undo) when not actually blocking.
    let was_blocking = sqlx::query!(
        "DELETE FROM blocks WHERE account_id = $1 AND target_account_id = $2 RETURNING account_id",
        account_id,
        target_id
    )
    .fetch_optional(&state.db)
    .await?
    .is_some();
    if !was_blocking {
        return Ok(());
    }

    // Send Undo(Block) activity to remote target
    if let Some(target) = sqlx::query!(
        "SELECT uri, inbox_url, shared_inbox_url, domain FROM accounts WHERE id = $1",
        target_id,
    )
    .fetch_optional(&state.db)
    .await?
    {
        // A remote target without an actor id cannot be addressed.
        let target_uri = target.uri.clone().unwrap_or_default();
        if target.domain.is_some() && !target_uri.is_empty() {
            if let Some(actor_row) = sqlx::query!(
                "SELECT username, id_scheme FROM accounts WHERE id = $1 AND domain IS NULL",
                account_id,
            )
            .fetch_optional(&state.db)
            .await?
            {
                if crate::federation::keypair::has_signing_key(state, account_id)
                    .await
                    .unwrap_or(false)
                {
                    let domain = state.instance.domain.clone();
                    let actor_url = crate::federation::tag::account_uri(
                        &domain,
                        account_id,
                        actor_row.id_scheme,
                        &actor_row.username,
                    );
                    let block_id = format!(
                        "https://{}/users/{}/blocks/{}",
                        domain, actor_row.username, target_id
                    );
                    let undo_id = format!("{}#undo", block_id);
                    let undo = crate::federation::activity::undo_block(
                        &undo_id,
                        &actor_url,
                        &block_id,
                        &target_uri,
                    )?;
                    let key_id = format!("{}#main-key", actor_url);
                    let inbox = if !target.shared_inbox_url.is_empty() {
                        target.shared_inbox_url
                    } else {
                        target.inbox_url
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
                            tracing::warn!(error = %e, "failed to enqueue Undo(Block)");
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

// ── GET /api/v1/blocks ────────────────────────────────────────────────────

pub async fn get_blocks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Query(q): Query<PaginationParams>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:blocks")?;
    let limit = q.limit_clamped(40, 80);
    let max_id = q.max_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let since_id = q.since_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let min_id = q.min_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    // Paginate by block.id (matching Mastodon's Block.paginate_by_max_id)
    let rows = sqlx::query!(
        r#"SELECT b.id AS block_id, b.target_account_id FROM blocks b
           JOIN accounts a ON a.id = b.target_account_id AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
           WHERE b.account_id = $1
             AND ($2::bigint IS NULL OR b.id < $2)
             AND ($3::bigint IS NULL OR b.id > $3)
             AND ($5::bigint IS NULL OR b.id > $5)
           ORDER BY b.id DESC LIMIT $4"#,
        auth.account_id,
        max_id,
        since_id,
        limit,
        min_id,
    )
    .fetch_all(&state.db)
    .await?;

    let first_block_id = rows.first().map(|r| r.block_id.to_string());
    let last_block_id = rows.last().map(|r| r.block_id.to_string());
    let target_ids: Vec<i64> = rows.iter().map(|r| r.target_account_id).collect();

    let accounts = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
        &target_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let account_map: std::collections::HashMap<i64, Account> =
        accounts.into_iter().map(|a| (a.id, a)).collect();
    let accounts_ordered: Vec<Account> = target_ids
        .iter()
        .filter_map(|id| account_map.get(id).cloned())
        .collect();

    let api_accounts = batch_accounts_to_api(&state, &accounts_ordered).await;
    let bounds = first_block_id.zip(last_block_id);
    let resp_headers = crate::api::mastodon::link_headers(
        &req_headers,
        &uri,
        bounds.as_ref().map(|(n, o)| (n.as_str(), o.as_str())),
    );
    Ok((resp_headers, Json(api_accounts)))
}

// ── GET /api/v1/mutes ─────────────────────────────────────────────────────

pub async fn get_mutes(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Query(q): Query<PaginationParams>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:mutes")?;
    let limit = q.limit_clamped(40, 80);
    let max_id = q.max_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let since_id = q.since_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let min_id = q.min_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    // Paginate by mute.id (matching Mastodon's Mute.paginate_by_max_id)
    let rows = sqlx::query!(
        r#"SELECT m.id AS mute_id, m.target_account_id, m.expires_at FROM mutes m
           JOIN accounts a ON a.id = m.target_account_id AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
           WHERE m.account_id = $1
             AND ($2::bigint IS NULL OR m.id < $2)
             AND ($3::bigint IS NULL OR m.id > $3)
             AND ($5::bigint IS NULL OR m.id > $5)
           ORDER BY m.id DESC LIMIT $4"#,
        auth.account_id,
        max_id,
        since_id,
        limit,
        min_id,
    )
    .fetch_all(&state.db)
    .await?;

    let first_mute_id = rows.first().map(|r| r.mute_id.to_string());
    let last_mute_id = rows.last().map(|r| r.mute_id.to_string());

    let mute_expiries: std::collections::HashMap<i64, Option<chrono::NaiveDateTime>> = rows
        .iter()
        .map(|r| (r.target_account_id, r.expires_at))
        .collect();
    let target_ids: Vec<i64> = rows.iter().map(|r| r.target_account_id).collect();

    let accounts = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
        &target_ids,
    )
    .fetch_all(&state.db)
    .await?;
    // Restore mute-ordered sequence
    let account_map: std::collections::HashMap<i64, Account> =
        accounts.into_iter().map(|a| (a.id, a)).collect();
    let accounts_ordered: Vec<Account> = target_ids
        .iter()
        .filter_map(|id| account_map.get(id).cloned())
        .collect();

    let mute_emojis_map = batch_account_emojis(&state, &accounts_ordered).await;
    let mute_roles_map = batch_account_roles(&state, &accounts_ordered).await;
    let api_accounts: Vec<ApiAccount> = accounts_ordered
        .iter()
        .map(|a| {
            let mut api = account_from_db(&state.urls, a);
            api.emojis = mute_emojis_map.get(&a.id).cloned().unwrap_or_default();
            api.roles = mute_roles_map.get(&a.id).cloned().unwrap_or_default();
            if let Some(expires_at) = mute_expiries.get(&a.id).and_then(|e| *e) {
                api.mute_expires_at =
                    Some(crate::api::mastodon::convert::mastodon_date(expires_at));
            }
            api
        })
        .collect();
    let bounds = first_mute_id.zip(last_mute_id);
    let resp_headers = crate::api::mastodon::link_headers(
        &req_headers,
        &uri,
        bounds.as_ref().map(|(n, o)| (n.as_str(), o.as_str())),
    );
    Ok((resp_headers, Json(api_accounts)))
}

// ── BlockWorker and MuteWorker ─────────────────────────────────────────────

/// `BlockWorker`, which is `AfterBlockService`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BlockWorker {
    pub account_id: i64,
    pub target_account_id: i64,
}

impl crate::jobs::Job for BlockWorker {
    const KIND: &'static str = "BlockWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT;

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        after_block(state, self.account_id, self.target_account_id).await?;
        Ok(())
    }
}

/// `MuteWorker`: the muted account's posts out of the home feed and the
/// lists.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MuteWorker {
    pub account_id: i64,
    pub target_account_id: i64,
}

impl crate::jobs::Job for MuteWorker {
    const KIND: &'static str = "MuteWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT;

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        crate::feed::clear_from_home(state, self.account_id, self.target_account_id).await;
        crate::feed::clear_from_lists(state, self.account_id, self.target_account_id).await;
        Ok(())
    }
}

/// `AfterBlockService#call`: the target out of the home feed and the lists,
/// and its notification requests, notifications and the conversations it
/// is in, gone.
pub async fn after_block(state: &AppState, account_id: i64, target_id: i64) -> sqlx::Result<()> {
    crate::feed::clear_from_home(state, account_id, target_id).await;
    crate::feed::clear_from_lists(state, account_id, target_id).await;
    sqlx::query!(
        "DELETE FROM notification_requests WHERE account_id = $1 AND from_account_id = $2",
        account_id,
        target_id,
    )
    .execute(&state.db)
    .await?;
    sqlx::query!(
        "DELETE FROM notifications WHERE account_id = $1 AND from_account_id = $2",
        account_id,
        target_id,
    )
    .execute(&state.db)
    .await?;
    sqlx::query!(
        "DELETE FROM account_conversations
         WHERE account_id = $1 AND $2 = ANY(participant_account_ids)",
        account_id,
        target_id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `BlockWorker.perform_async`, or the worker at once when the tests ask
/// for background work inline.
pub async fn queue_block_worker(state: &AppState, account_id: i64, target_id: i64) {
    if feed::sync_fanout() {
        if let Err(error) = after_block(state, account_id, target_id).await {
            tracing::warn!(%error, account_id, target_id, "AfterBlockService failed");
        }
    } else {
        crate::jobs::push(
            state,
            BlockWorker {
                account_id,
                target_account_id: target_id,
            },
        )
        .await;
    }
}

/// `MuteWorker.perform_async`, or the worker at once.
async fn queue_mute_worker(state: &AppState, account_id: i64, target_id: i64) {
    if feed::sync_fanout() {
        crate::feed::clear_from_home(state, account_id, target_id).await;
        crate::feed::clear_from_lists(state, account_id, target_id).await;
    } else {
        crate::jobs::push(
            state,
            MuteWorker {
                account_id,
                target_account_id: target_id,
            },
        )
        .await;
    }
}
