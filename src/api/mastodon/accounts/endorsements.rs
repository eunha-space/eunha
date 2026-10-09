//! Endorsements (profile "featured accounts"): endorse/unendorse a followed
//! account and list a profile's or your own endorsed accounts.

use super::*;

pub async fn endorse_account(
    state: AppState,
    Path(target_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Relationship>> {
    auth.require_scope("write:accounts")?;
    find_account(&state, target_id).await?;
    // Mastodon AccountPin#validate_follow_relationship: you can only endorse
    // accounts you follow.
    let following = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
        auth.account_id,
        target_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false);
    if !following {
        return Err(AppError::Unprocessable(
            "Validation failed: Account must be one you are following".into(),
        ));
    }
    sqlx::query!(
        "INSERT INTO account_pins (account_id, target_account_id, created_at, updated_at) VALUES ($1, $2, now(), now()) ON CONFLICT DO NOTHING",
        auth.account_id, target_id,
    )
    .execute(&state.db)
    .await?;
    build_relationship(&state, auth.account_id, target_id)
        .await
        .map(Json)
}

// ── POST /api/v1/accounts/:id/unendorse ──────────────────────────────────

pub async fn unendorse_account(
    state: AppState,
    Path(target_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Relationship>> {
    auth.require_scope("write:accounts")?;
    find_account(&state, target_id).await?;
    sqlx::query!(
        "DELETE FROM account_pins WHERE account_id = $1 AND target_account_id = $2",
        auth.account_id,
        target_id,
    )
    .execute(&state.db)
    .await?;
    build_relationship(&state, auth.account_id, target_id)
        .await
        .map(Json)
}

// ── GET /api/v1/accounts/:id/endorsements ────────────────────────────────

pub async fn get_endorsements(
    state: AppState,
    Path(id): Path<i64>,
    uri: Uri,
    req_headers: HeaderMap,
    Query(q): Query<PaginationParams>,
) -> AppResult<impl IntoResponse> {
    // `@account.unavailable? ? [] : paginated_endorsed_accounts`.
    if find_account(&state, id).await?.is_unavailable() {
        return Ok((HeaderMap::new(), Json(Vec::new())));
    }
    let limit = q.limit_clamped(40, 80);
    let max_id = q.max_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let since_id = q.since_id.as_deref().and_then(|s| s.parse::<i64>().ok());

    let accounts = sqlx::query_as!(
        Account,
        r#"SELECT a.* FROM accounts a
           JOIN account_pins ap ON ap.target_account_id = a.id
           WHERE ap.account_id = $1
             AND ($2::bigint IS NULL OR a.id < $2)
             AND ($3::bigint IS NULL OR a.id > $3)
             AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
           ORDER BY a.id DESC
           LIMIT $4"#,
        id,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(&state.db)
    .await?;

    let api_accounts = batch_accounts_to_api(&state, &accounts).await;
    let bounds = api_accounts
        .first()
        .zip(api_accounts.last())
        .map(|(n, o)| (n.id.as_str(), o.id.as_str()));
    let resp_headers = crate::api::mastodon::link_headers(&req_headers, &uri, bounds);
    Ok((resp_headers, Json(api_accounts)))
}

// ── GET /api/v1/endorsements ──────────────────────────────────────────────

pub async fn get_my_endorsements(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Query(q): Query<PaginationParams>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:accounts")?;
    let unlimited = q.limit.as_deref() == Some("0");
    let limit = if unlimited {
        i64::MAX
    } else {
        q.limit_clamped(40, 80)
    };
    let max_id = q.max_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let since_id = q.since_id.as_deref().and_then(|s| s.parse::<i64>().ok());

    // Paginate by account.id (matching Mastodon's paginate_by_max_id on endorsed_accounts)
    let accounts = sqlx::query_as!(
        Account,
        r#"SELECT a.* FROM accounts a
           JOIN account_pins ap ON ap.target_account_id = a.id
           WHERE ap.account_id = $1
             AND ($2::bigint IS NULL OR a.id < $2)
             AND ($3::bigint IS NULL OR a.id > $3)
             AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
           ORDER BY a.id DESC
           LIMIT $4"#,
        auth.account_id,
        max_id,
        since_id,
        limit,
    )
    .fetch_all(&state.db)
    .await?;

    let api_accounts = batch_accounts_to_api(&state, &accounts).await;
    let bounds = if unlimited {
        None
    } else {
        api_accounts
            .first()
            .zip(api_accounts.last())
            .map(|(n, o)| (n.id.as_str(), o.id.as_str()))
    };
    let resp_headers = crate::api::mastodon::link_headers(&req_headers, &uri, bounds);
    Ok((resp_headers, Json(api_accounts)))
}
