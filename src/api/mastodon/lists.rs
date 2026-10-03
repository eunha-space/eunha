use axum::{
    extract::{Extension, Json, Path, Query},
    http::{HeaderMap, Uri},
    response::IntoResponse,
};
use serde::Deserialize;

use super::{
    accounts::batch_accounts_to_api,
    types::{Account, List},
};
use crate::{
    db::models,
    error::{AppError, AppResult},
    feed,
    middleware::{AuthenticatedUser, ResolvedInstance},
    state::AppState,
};

// ── GET /api/v1/lists ──────────────────────────────────────────────────────

pub async fn get_lists(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<List>>> {
    auth.require_scope("read:lists")?;
    let lists = sqlx::query_as!(
        models::List,
        "SELECT * FROM lists WHERE account_id = $1 ORDER BY id ASC",
        auth.account_id,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(lists.iter().map(list_from_db).collect()))
}

// ── GET /api/v1/lists/:id ─────────────────────────────────────────────────

pub async fn get_list(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<List>> {
    auth.require_scope("read:lists")?;
    let list = fetch_list(&state, id, auth.account_id).await?;
    Ok(Json(list_from_db(&list)))
}

// ── POST /api/v1/lists ────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListForm {
    pub title: String,
    pub replies_policy: Option<String>,
    pub exclusive: Option<bool>,
}

const VALID_REPLIES_POLICIES: &[&str] = &["followed", "list", "none"];

/// Maximum list title length (Mastodon `List::TITLE_LENGTH_LIMIT`).
const LIST_TITLE_MAX: usize = 256;
/// Maximum lists per account (Mastodon `List::PER_ACCOUNT_LIMIT`).
const LIST_PER_ACCOUNT_LIMIT: i64 = 50;

/// Validate list title + replies_policy the way Mastodon's List model does.
/// Applies to both create and update.
fn validate_list_form(form: &ListForm) -> AppResult<()> {
    if form.title.trim().is_empty() {
        return Err(AppError::Unprocessable("Title can't be blank".into()));
    }
    if form.title.chars().count() > LIST_TITLE_MAX {
        return Err(AppError::Unprocessable(format!(
            "Title is too long (maximum is {LIST_TITLE_MAX} characters)"
        )));
    }
    let replies_policy = form.replies_policy.as_deref().unwrap_or("list");
    if !VALID_REPLIES_POLICIES.contains(&replies_policy) {
        return Err(AppError::Unprocessable(format!(
            "Replies policy is not included in the list: {replies_policy}"
        )));
    }
    Ok(())
}

pub async fn create_list(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<ListForm>,
) -> AppResult<Json<List>> {
    auth.require_scope("write:lists")?;
    validate_list_form(&form)?;

    // Per-account list cap (Mastodon validate_account_lists_limit, create only).
    let list_count = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM lists WHERE account_id = $1",
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);
    if list_count >= LIST_PER_ACCOUNT_LIMIT {
        return Err(AppError::Unprocessable(
            "Validation failed: You have reached the maximum number of lists".into(),
        ));
    }

    let replies_policy = form.replies_policy.as_deref().unwrap_or("list");
    let replies_policy_int = models::replies::from_str(replies_policy);
    let list = sqlx::query_as!(
        models::List,
        r#"INSERT INTO lists (account_id, title, replies_policy, exclusive, created_at, updated_at)
           VALUES ($1, $2, $3, $4, now(), now())
           RETURNING *"#,
        auth.account_id,
        form.title,
        replies_policy_int,
        form.exclusive.unwrap_or(false),
    )
    .fetch_one(&state.db)
    .await?;

    Ok(Json(list_from_db(&list)))
}

// ── PUT /api/v1/lists/:id ─────────────────────────────────────────────────

pub async fn update_list(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<ListForm>,
) -> AppResult<Json<List>> {
    auth.require_scope("write:lists")?;
    fetch_list(&state, id, auth.account_id).await?;
    validate_list_form(&form)?;

    let list = sqlx::query_as!(
        models::List,
        r#"UPDATE lists SET title = $1, replies_policy = $2, exclusive = $3, updated_at = now()
           WHERE id = $4 AND account_id = $5
           RETURNING *"#,
        form.title,
        models::replies::from_str(form.replies_policy.as_deref().unwrap_or("list")),
        form.exclusive.unwrap_or(false),
        id,
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?;

    Ok(Json(list_from_db(&list)))
}

// ── DELETE /api/v1/lists/:id ──────────────────────────────────────────────

pub async fn delete_list(
    state: AppState,
    Path(id): Path<i64>,
    Extension(ResolvedInstance(_instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:lists")?;
    fetch_list(&state, id, auth.account_id).await?;
    sqlx::query!(
        "DELETE FROM lists WHERE id = $1 AND account_id = $2",
        id,
        auth.account_id
    )
    .execute(&state.db)
    .await?;
    {
        let mut redis = state.redis.clone();
        feed::delete_list_feed(&mut redis, &state.redis_keys, id).await;
    }
    Ok(Json(serde_json::json!({})))
}

// ── GET /api/v1/lists/:id/accounts ───────────────────────────────────────

pub async fn get_list_accounts(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(pagination): Query<super::types::PaginationParams>,
    uri: Uri,
    req_headers: HeaderMap,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:lists")?;
    fetch_list(&state, id, auth.account_id).await?;

    let unlimited = pagination.limit.as_deref() == Some("0");
    let limit = if unlimited {
        i64::MAX
    } else {
        pagination.limit_clamped(40, 80)
    };
    let max_id: Option<i64> = pagination.max_id.as_deref().and_then(|s| s.parse().ok());
    let since_id: Option<i64> = pagination.since_id.as_deref().and_then(|s| s.parse().ok());
    let min_id: Option<i64> = pagination.min_id.as_deref().and_then(|s| s.parse().ok());

    let accounts = sqlx::query_as!(
        models::Account,
        r#"SELECT a.* FROM accounts a
           JOIN list_accounts la ON la.account_id = a.id
           WHERE la.list_id = $1
             AND ($2::bigint IS NULL OR a.id < $2)
             AND ($3::bigint IS NULL OR a.id > $3)
             AND ($5::bigint IS NULL OR a.id > $5)
           ORDER BY a.id DESC
           LIMIT $4"#,
        id,
        max_id,
        since_id,
        limit,
        min_id,
    )
    .fetch_all(&state.db)
    .await?;

    let result: Vec<Account> = batch_accounts_to_api(&state, &accounts).await;
    let bounds = if unlimited {
        None
    } else {
        result
            .first()
            .zip(result.last())
            .map(|(n, o)| (n.id.as_str(), o.id.as_str()))
    };
    let resp_headers = super::link_headers(&req_headers, &uri, bounds);
    Ok((resp_headers, Json(result)))
}

// ── POST /api/v1/lists/:id/accounts ──────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListAccountsForm {
    pub account_ids: Vec<String>,
}

pub async fn add_list_accounts(
    state: AppState,
    Path(id): Path<i64>,
    Extension(ResolvedInstance(_instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<ListAccountsForm>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:lists")?;
    fetch_list(&state, id, auth.account_id).await?;

    let mut added: Vec<i64> = Vec::new();
    for id_str in &form.account_ids {
        if let Ok(account_id) = id_str.parse::<i64>() {
            // Mastodon ListAccount#validate_relationship: you may add an account
            // you follow, one you have a pending follow request to, or yourself
            // (the list owner).
            let allowed = account_id == auth.account_id
                || sqlx::query_scalar!(
                    r#"SELECT EXISTS(
                         SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2
                         UNION ALL
                         SELECT 1 FROM follow_requests WHERE account_id = $1 AND target_account_id = $2
                       )"#,
                    auth.account_id, account_id,
                )
                .fetch_one(&state.db)
                .await?
                .unwrap_or(false);
            if !allowed {
                return Err(AppError::Unprocessable(
                    "Account must be followed before adding to a list".into(),
                ));
            }
            // `ListAccount#set_follow`: the follow, or else the follow
            // request, the membership hangs on; the owner's own has neither.
            sqlx::query!(
                "INSERT INTO list_accounts (list_id, account_id, follow_id, follow_request_id)
                 SELECT $1, $2, f.id, CASE WHEN f.id IS NULL THEN fr.id END
                 FROM (SELECT 1) one
                 LEFT JOIN follows f
                   ON f.account_id = $3 AND f.target_account_id = $2 AND $2 <> $3
                 LEFT JOIN follow_requests fr
                   ON fr.account_id = $3 AND fr.target_account_id = $2 AND $2 <> $3
                 ON CONFLICT DO NOTHING",
                id,
                account_id,
                auth.account_id,
            )
            .execute(&state.db)
            .await?;
            added.push(account_id);
        }
    }

    // `AddAccountsToListService#merge_into_list!`: a `MergeWorker` for each
    // added account the owner follows.
    let merge_ids: Vec<i64> = sqlx::query_scalar!(
        "SELECT account_id FROM list_accounts
         WHERE list_id = $1 AND account_id = ANY($2) AND follow_id IS NOT NULL",
        id,
        &added,
    )
    .fetch_all(&state.db)
    .await?;
    for account_id in merge_ids {
        crate::home_feed::enqueue_merge(&state, account_id, id, crate::home_feed::FeedType::List)
            .await;
    }

    Ok(Json(serde_json::json!({})))
}

// ── DELETE /api/v1/lists/:id/accounts ────────────────────────────────────

pub async fn remove_list_accounts(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<ListAccountsForm>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:lists")?;
    fetch_list(&state, id, auth.account_id).await?;

    let account_ids: Vec<i64> = form
        .account_ids
        .iter()
        .filter_map(|id| id.parse::<i64>().ok())
        .collect();
    // `RemoveAccountsFromListService#unmerge_from_list!`: an `UnmergeWorker`
    // for each followed member, queued before the memberships go.
    let unmerge_ids: Vec<i64> = sqlx::query_scalar!(
        "SELECT account_id FROM list_accounts
         WHERE list_id = $1 AND account_id = ANY($2) AND follow_id IS NOT NULL",
        id,
        &account_ids,
    )
    .fetch_all(&state.db)
    .await?;
    for account_id in unmerge_ids {
        crate::home_feed::enqueue_unmerge(&state, account_id, id, crate::home_feed::FeedType::List)
            .await;
    }
    sqlx::query!(
        "DELETE FROM list_accounts WHERE list_id = $1 AND account_id = ANY($2)",
        id,
        &account_ids,
    )
    .execute(&state.db)
    .await?;

    Ok(Json(serde_json::json!({})))
}

// ── Helpers ────────────────────────────────────────────────────────────────

async fn fetch_list(state: &AppState, id: i64, account_id: i64) -> AppResult<models::List> {
    sqlx::query_as!(
        models::List,
        "SELECT * FROM lists WHERE id = $1 AND account_id = $2",
        id,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

fn list_from_db(l: &models::List) -> List {
    List {
        id: l.id.to_string(),
        title: l.title.clone(),
        replies_policy: models::replies::to_str(l.replies_policy).to_owned(),
        exclusive: l.exclusive,
    }
}
