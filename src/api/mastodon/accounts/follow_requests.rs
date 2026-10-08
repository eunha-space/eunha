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

/// `Api::V1::FollowRequestsController#authorize`: `AuthorizeFollowService`,
/// which answers 404 when there is no request, and a `follow` notification
/// of the new follower (`LocalNotificationWorker`).
pub async fn authorize_follow_request(
    state: AppState,
    Path(requester_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Relationship>> {
    auth.require_scope("write:follows")?;
    if !authorize_follow(&state, requester_id, auth.account_id).await? {
        return Err(AppError::NotFound);
    }

    fetch_account(&state, requester_id).await?;
    push::create_and_push(&state, auth.account_id, requester_id, "follow", None).await;

    build_relationship(&state, auth.account_id, requester_id)
        .await
        .map(Json)
}

/// What [`authorize`] made a follow of: the request's id and `uri`.
pub struct Authorized {
    pub id: i64,
    pub uri: Option<String>,
}

/// Mastodon's `FollowRequest#authorize!`: the request becomes a follow with
/// the request's options and `uri` (`account.follow!`, which updates a
/// follow already there), the requester's list memberships that hung on the
/// request now hang on the follow, the target's posts are merged into a
/// local requester's home feed, and the request goes with its notification.
/// `None` when there was no request.
pub async fn authorize(
    state: &AppState,
    requester_id: i64,
    target_id: i64,
) -> AppResult<Option<Authorized>> {
    let Some(request) = sqlx::query!(
        "SELECT id, uri, show_reblogs, notify, languages FROM follow_requests
         WHERE account_id = $1 AND target_account_id = $2",
        requester_id,
        target_id
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(None);
    };

    let follow = sqlx::query!(
        r#"INSERT INTO follows (account_id, target_account_id, show_reblogs, notify, languages, uri, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, now(), now())
           ON CONFLICT (account_id, target_account_id) DO UPDATE
             SET show_reblogs = EXCLUDED.show_reblogs, notify = EXCLUDED.notify,
                 languages = COALESCE(EXCLUDED.languages, follows.languages),
                 updated_at = now()
           RETURNING id, (xmax = 0) AS "inserted!""#,
        requester_id,
        target_id,
        request.show_reblogs,
        request.notify,
        request.languages.as_deref(),
        // `set_uri`, for a request that came without one.
        request
            .uri
            .clone()
            .unwrap_or_else(|| crate::federation::relationships::generate_uri(&state.instance.domain)),
    )
    .fetch_one(&state.db)
    .await?;
    if follow.inserted {
        // `AccountStat`'s `update_index('accounts', :account)`.
        crate::search::elasticsearch::indexing::accounts(state, &[requester_id, target_id]).await;
        crate::counters::on_follow_created(state, requester_id, target_id).await?;
    }
    // The memberships move before the request goes, which would take them
    // with it.
    sqlx::query!(
        "UPDATE list_accounts SET follow_request_id = NULL, follow_id = $2
         WHERE follow_request_id = $1",
        request.id,
        follow.id,
    )
    .execute(&state.db)
    .await?;
    sqlx::query!("DELETE FROM follow_requests WHERE id = $1", request.id)
        .execute(&state.db)
        .await?;
    // `has_one :notification, dependent: :destroy`.
    sqlx::query!(
        "DELETE FROM notifications WHERE activity_type = 'FollowRequest' AND activity_id = $1",
        request.id,
    )
    .execute(&state.db)
    .await?;

    // `MergeWorker` into the home feed, which only a local requester has.
    let requester_is_local = sqlx::query_scalar!(
        r#"SELECT domain IS NULL AS "local!" FROM accounts WHERE id = $1"#,
        requester_id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    if requester_is_local {
        crate::home_feed::merge_into_home_and_lists(state, target_id, requester_id).await;
    }

    Ok(Some(Authorized {
        id: request.id,
        uri: request.uri,
    }))
}

/// Mastodon's `AuthorizeFollowService`: the request authorized, and a
/// remote requester told with an `Accept` of it. Says whether there was a
/// request (`find_by!`).
pub async fn authorize_follow(state: &AppState, source_id: i64, target_id: i64) -> AppResult<bool> {
    let Some(request) = authorize(state, source_id, target_id).await? else {
        return Ok(false);
    };
    let source = fetch_account(state, source_id).await?;
    if source.domain.is_some() {
        let target = fetch_account(state, target_id).await?;
        super::relationships::send_accept_follow(
            state,
            &target,
            &source,
            Some(request.id),
            request.uri.as_deref(),
        )
        .await;
    }
    Ok(true)
}

/// `AuthorizeFollowWorker`: `AuthorizeFollowService`, for a request that
/// may have gone since (`rescue ActiveRecord::RecordNotFound`).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct AuthorizeFollowWorker {
    pub source_account_id: i64,
    pub target_account_id: i64,
}

impl crate::jobs::Job for AuthorizeFollowWorker {
    const KIND: &'static str = "AuthorizeFollowWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT;

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        authorize_follow(state, self.source_account_id, self.target_account_id)
            .await
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        Ok(())
    }
}

// ── POST /api/v1/follow_requests/:id/reject ───────────────────────────────

/// `Api::V1::FollowRequestsController#reject`: `RejectFollowService`, which
/// answers 404 when there is no request.
pub async fn reject_follow_request(
    state: AppState,
    Path(requester_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Relationship>> {
    auth.require_scope("write:follows")?;
    if !super::relationships::reject_follow(&state, requester_id, auth.account_id).await? {
        return Err(AppError::NotFound);
    }
    build_relationship(&state, auth.account_id, requester_id)
        .await
        .map(Json)
}
