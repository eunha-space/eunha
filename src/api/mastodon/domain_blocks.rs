use axum::{
    extract::{Extension, Query},
    http::{HeaderMap, Uri},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use super::types::PaginationParams;
use crate::{error::AppResult, middleware::AuthenticatedUser, state::AppState};

// ── GET /api/v1/domain_blocks ─────────────────────────────────────────────

pub async fn get_domain_blocks(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(q): Query<PaginationParams>,
    uri: Uri,
    req_headers: HeaderMap,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:blocks")?;
    let limit = q.limit_clamped(100, 200);
    let max_id = q.max_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let since_id = q.since_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let min_id = q.min_id.as_deref().and_then(|s| s.parse::<i64>().ok());

    let rows = sqlx::query!(
        r#"SELECT id, domain FROM account_domain_blocks
           WHERE account_id = $1
             AND ($2::bigint IS NULL OR id < $2)
             AND ($3::bigint IS NULL OR id > $3)
             AND ($4::bigint IS NULL OR id > $4)
           ORDER BY id DESC LIMIT $5"#,
        auth.account_id,
        max_id,
        since_id,
        min_id,
        limit,
    )
    .fetch_all(&state.db)
    .await?;

    let domains: Vec<String> = rows.iter().map(|r| r.domain.clone()).collect();

    let bounds = rows
        .first()
        .zip(rows.last())
        .map(|(n, o)| (n.id.to_string(), o.id.to_string()));
    let resp_headers = super::link_headers(
        &req_headers,
        &uri,
        bounds.as_ref().map(|(n, o)| (n.as_str(), o.as_str())),
    );

    Ok((resp_headers, Json(domains)))
}

// ── POST /api/v1/domain_blocks ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DomainBlockForm {
    #[serde(default, deserialize_with = "super::extractors::rails::string")]
    pub domain: String,
}

pub async fn block_domain(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    super::extractors::Params(form): super::extractors::Params<DomainBlockForm>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:blocks")?;
    block_domain_for(&state, auth.account_id, &form.domain.to_lowercase()).await?;
    Ok(Json(serde_json::json!({})))
}

/// `Account#block_domain!` followed by `AfterAccountDomainBlockWorker`:
/// `account_id` blocks `domain`, given lowercased, and its follows and
/// notifications from there go. The home feed is left as it is, as
/// Mastodon leaves it: undoing the follows unmerges those accounts' posts,
/// and the fan-out keeps the domain's boosted posts out from then on.
pub async fn block_domain_for(state: &AppState, account_id: i64, domain: &str) -> AppResult<()> {
    sqlx::query!(
        r#"INSERT INTO account_domain_blocks (account_id, domain, created_at, updated_at) VALUES ($1, $2, now(), now())
           ON CONFLICT (account_id, domain) DO NOTHING"#,
        account_id,
        domain,
    )
    .execute(&state.db)
    .await?;

    after_block_domain(state, account_id, domain).await?;

    // Clear the blocker's notifications originating from that domain
    // (Mastodon clear_notifications!).
    let _ = sqlx::query!(
        r#"DELETE FROM notifications
           WHERE account_id = $1
             AND from_account_id IN (SELECT id FROM accounts WHERE domain = $2)"#,
        account_id,
        domain,
    )
    .execute(&state.db)
    .await;

    Ok(())
}

/// `AfterBlockDomainFromAccountService`'s follow work: the blocker's follows
/// of the domain are undone, its followers there and their pending requests
/// rejected, each told so, and the severed follows recorded and notified.
async fn after_block_domain(state: &AppState, account_id: i64, domain: &str) -> AppResult<()> {
    use crate::federation::{activity, delivery, tag};

    let me = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        account_id
    )
    .fetch_one(&state.db)
    .await?;
    let my_url = tag::account_uri_of(&state.instance.domain, &me);
    let key_id = tag::key_id_of(&state.instance.domain, &me);

    let severs = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM follows f JOIN accounts a ON a.id = f.target_account_id
             WHERE f.account_id = $1 AND a.domain = $2
             UNION ALL
             SELECT 1 FROM follows f JOIN accounts a ON a.id = f.account_id
             WHERE f.target_account_id = $1 AND a.domain = $2
           ) AS "e!""#,
        account_id,
        domain,
    )
    .fetch_one(&state.db)
    .await?;
    let event = if severs {
        let event = crate::moderation::severance::create(
            &state.db,
            crate::moderation::severance::kind::USER_DOMAIN_BLOCK,
            domain,
        )
        .await?;
        crate::moderation::severance::record_follows_with_domain(
            &state.db, event, account_id, domain,
        )
        .await?;
        Some(event)
    } else {
        None
    };

    // `remove_follows!`: `UnfollowService` for each, which unmerges the
    // followed account from the home feed and the lists that held it —
    // memberships that go with the follow, so they are read first.
    let mut lists_holding: std::collections::HashMap<i64, Vec<i64>> =
        std::collections::HashMap::new();
    for row in sqlx::query!(
        r#"SELECT la.list_id, la.account_id FROM list_accounts la
           JOIN lists l ON l.id = la.list_id
           JOIN accounts a ON a.id = la.account_id
           WHERE l.account_id = $1 AND a.domain = $2"#,
        account_id,
        domain,
    )
    .fetch_all(&state.db)
    .await?
    {
        lists_holding
            .entry(row.account_id)
            .or_default()
            .push(row.list_id);
    }
    let following = sqlx::query!(
        r#"DELETE FROM follows f USING accounts a
           WHERE f.target_account_id = a.id AND f.account_id = $1 AND a.domain = $2
           RETURNING f.id, f.uri, a.id AS target_id, a.uri AS target_uri, a.inbox_url, a.shared_inbox_url"#,
        account_id,
        domain,
    )
    .fetch_all(&state.db)
    .await?;
    for follow in following {
        // `AccountStat`'s `update_index('accounts', :account)`.
        crate::search::elasticsearch::indexing::accounts(state, &[account_id, follow.target_id])
            .await;
        crate::counters::on_follow_removed(state, account_id, follow.target_id).await?;
        crate::home_feed::unmerge_from_home_and_lists(
            state,
            follow.target_id,
            account_id,
            lists_holding.remove(&follow.target_id).unwrap_or_default(),
        )
        .await;
        let follow_uri = follow
            .uri
            .unwrap_or_else(|| format!("{my_url}#follows/{}", follow.id));
        let inbox = if follow.shared_inbox_url.is_empty() {
            follow.inbox_url
        } else {
            follow.shared_inbox_url
        };
        if let (Some(target_uri), false) = (follow.target_uri, inbox.is_empty()) {
            let undo = activity::undo_follow(
                &format!("{my_url}#follows/{}/undo", follow.id),
                &my_url,
                &follow_uri,
                &my_url,
                &target_uri,
            )?;
            delivery::deliver_to_inboxes(state, undo, vec![inbox], key_id.clone()).await?;
        }
    }

    // `reject_existing_followers!` and `reject_pending_follow_requests!`.
    let followers = sqlx::query!(
        r#"DELETE FROM follows f USING accounts a
           WHERE f.account_id = a.id AND f.target_account_id = $1 AND a.domain = $2
           RETURNING f.id, f.uri, a.id AS follower_id, a.uri AS follower_uri, a.inbox_url"#,
        account_id,
        domain,
    )
    .fetch_all(&state.db)
    .await?;
    let requests = sqlx::query!(
        r#"DELETE FROM follow_requests f USING accounts a
           WHERE f.account_id = a.id AND f.target_account_id = $1 AND a.domain = $2
           RETURNING f.id, f.uri, a.uri AS follower_uri, a.inbox_url"#,
        account_id,
        domain,
    )
    .fetch_all(&state.db)
    .await?;
    let mut rejects = vec![];
    for f in followers {
        // `AccountStat`'s `update_index('accounts', :account)`.
        crate::search::elasticsearch::indexing::accounts(state, &[f.follower_id, account_id]).await;
        crate::counters::on_follow_removed(state, f.follower_id, account_id).await?;
        rejects.push((f.id, f.uri, f.follower_uri, f.inbox_url));
    }
    for r in requests {
        rejects.push((r.id, r.uri, r.follower_uri, r.inbox_url));
    }
    for (id, uri, follower_uri, inbox) in rejects {
        let (Some(follower_uri), false) = (follower_uri, inbox.is_empty()) else {
            continue;
        };
        let follow_uri = uri.unwrap_or_else(|| format!("{follower_uri}#follows/{id}"));
        let reject = activity::reject_follow(
            &format!("{my_url}#rejects/follows/{id}"),
            &my_url,
            &follow_uri,
            &follower_uri,
            &my_url,
        )?;
        delivery::deliver_to_inboxes(state, reject, vec![inbox], key_id.clone()).await?;
    }

    // `clear_notification_permissions!`
    sqlx::query!(
        r#"DELETE FROM notification_permissions
           WHERE account_id = $1 AND from_account_id IN (SELECT id FROM accounts WHERE domain = $2)"#,
        account_id,
        domain,
    )
    .execute(&state.db)
    .await?;

    // `notify_of_severed_relationships!`
    if let Some(event) = event {
        crate::moderation::severance::notify_affected(state, event)
            .await
            .map_err(crate::error::AppError::Internal)?;
    }
    Ok(())
}

// ── GET /api/v1/domain_blocks/preview ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DomainPreviewQuery {
    pub domain: Option<String>,
}

pub async fn preview_domain_block(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(q): Query<DomainPreviewQuery>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:blocks")?;
    let domain = q.domain.as_deref().unwrap_or("").to_lowercase();

    let following_count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) FROM follows f
           JOIN accounts a ON a.id = f.target_account_id
           WHERE f.account_id = $1 AND a.domain = $2"#,
        auth.account_id,
        domain,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    let followers_count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) FROM follows f
           JOIN accounts a ON a.id = f.account_id
           WHERE f.target_account_id = $1 AND a.domain = $2"#,
        auth.account_id,
        domain,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);

    Ok(Json(serde_json::json!({
        "following_count": following_count,
        "followers_count": followers_count,
    })))
}

// ── DELETE /api/v1/domain_blocks ─────────────────────────────────────────

pub async fn unblock_domain(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    super::extractors::Params(form): super::extractors::Params<DomainBlockForm>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:blocks")?;
    sqlx::query!(
        "DELETE FROM account_domain_blocks WHERE account_id = $1 AND domain = $2",
        auth.account_id,
        form.domain.to_lowercase(),
    )
    .execute(&state.db)
    .await?;

    Ok(Json(serde_json::json!({})))
}
