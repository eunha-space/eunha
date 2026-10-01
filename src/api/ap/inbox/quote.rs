//! Inbound `QuoteRequest` and `FeatureRequest` activities: the consent
//! handshakes by which a remote actor asks to quote a local status or feature a
//! local account, and our Accept/Reject responses.

use serde_json::Value;

use crate::{error::AppResult, state::AppState};

use super::{fetch_remote_status, json_uri, resolve_or_fetch_remote_account};

pub(super) async fn handle_quote_request(
    state: &AppState,
    instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let req_id = activity.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let object_uri = activity
        .get("object")
        .and_then(|o| o.as_str())
        .unwrap_or("");
    let instrument_uri = json_uri(activity.get("instrument"));

    if req_id.is_empty() || object_uri.is_empty() || actor_uri.is_empty() {
        return Ok(());
    }

    // The quoted status must be one of ours.
    let Some(status) = sqlx::query!(
        r#"SELECT s.id, s.account_id, s.quote_approval_policy, s.visibility, s.reblog_of_id,
                  a.username, a.id_scheme
           FROM statuses s JOIN accounts a ON a.id = s.account_id
           WHERE s.uri = $1 AND s.deleted_at IS NULL AND a.domain IS NULL"#,
        object_uri,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };

    let Ok(quoter_id) = resolve_or_fetch_remote_account(state, actor_uri).await else {
        return Ok(());
    };
    let quoter = sqlx::query!(
        "SELECT uri, inbox_url, shared_inbox_url FROM accounts WHERE id = $1",
        quoter_id,
    )
    .fetch_one(&state.db)
    .await?;
    let inbox = if !quoter.shared_inbox_url.is_empty() {
        quoter.shared_inbox_url
    } else {
        quoter.inbox_url
    };
    if !crate::federation::keypair::has_signing_key(state, status.account_id)
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }
    if inbox.is_empty() {
        return Ok(());
    }

    let domain = &instance.domain;
    // The quoted status is ours, so its author's actor id is derived rather
    // than read from `accounts.uri`, which local accounts leave unset.
    let actor_url = crate::federation::tag::account_uri(
        domain,
        status.account_id,
        status.id_scheme,
        &status.username,
    );
    let key_id = format!("{actor_url}#main-key");
    // A remote actor we just resolved always has a URI; fall back to the one it
    // was resolved from rather than signing an empty address.
    let quoter_uri = quoter.uri.unwrap_or_else(|| actor_uri.to_string());

    // `StatusPolicy#quote?`: the quoter may see the post, neither blocks the
    // other, and the post's policy does not deny it.
    let relation = sqlx::query!(
        r#"SELECT
             EXISTS (SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2) AS "follows_author!",
             EXISTS (SELECT 1 FROM follows WHERE account_id = $2 AND target_account_id = $1) AS "followed_by_author!",
             EXISTS (SELECT 1 FROM blocks
                     WHERE (account_id = $1 AND target_account_id = $2)
                        OR (account_id = $2 AND target_account_id = $1)) AS "blocked!""#,
        quoter_id,
        status.account_id,
    )
    .fetch_one(&state.db)
    .await?;
    use crate::db::models::{quote_policy, vis};
    let visible = match status.visibility {
        vis::PUBLIC | vis::UNLISTED => true,
        vis::PRIVATE => relation.follows_author,
        _ => false,
    };
    let allowed = visible
        && status.reblog_of_id.is_none()
        && !relation.blocked
        && quote_policy::for_account(
            status.quote_approval_policy,
            quoter_id == status.account_id,
            relation.follows_author,
            relation.followed_by_author,
        ) != quote_policy::ForAccount::Denied;
    if !allowed {
        let reject_id = format!(
            "{actor_url}#rejects/quote_requests/{}",
            crate::snowflake::next_id()
        );
        if let Ok(r) =
            crate::federation::consent::reject(&reject_id, &actor_url, &quoter_uri, req_id)
        {
            if let Err(e) =
                crate::federation::delivery::deliver_to_inboxes(state, r, vec![inbox], key_id).await
            {
                tracing::warn!(error = %e, "failed to enqueue quote Reject");
            }
        }
        return Ok(());
    }

    // Auto-accept: stamp a QuoteAuthorization. Fetch the quoting status on
    // demand if we don't already have it.
    let quoting_status_id = match sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
        instrument_uri,
    )
    .fetch_optional(&state.db)
    .await?
    {
        Some(id) => id,
        None => {
            match fetch_remote_status(state, instrument_uri).await? {
                Some(id) => id,
                None => {
                    tracing::debug!(actor_uri, instrument_uri, "QuoteRequest accepted but quoting status could not be fetched; skipping stamp");
                    return Ok(());
                }
            }
        }
    };

    // Upsert the quote and mark it accepted (one quote per quoting status).
    let quote_id = sqlx::query_scalar!(
        r#"INSERT INTO quotes
             (id, status_id, quoted_status_id, account_id, quoted_account_id, activity_uri, state, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, 1, now(), now())
           ON CONFLICT (status_id) DO UPDATE
             SET state = 1, activity_uri = EXCLUDED.activity_uri, updated_at = now()
           RETURNING id"#,
        crate::snowflake::next_id(),
        quoting_status_id,
        status.id,
        quoter_id,
        status.account_id,
        req_id,
    )
    .fetch_one(&state.db)
    .await?;

    // Mastodon's `Quote#increment_counter_caches!` is `return unless
    // accepted?`, so accepting is when the quoted post's count rises. The
    // upsert above marks it accepted; without this a post quoted from other
    // instances shows none of those quotes.
    if let Err(e) = sqlx::query!(
        r#"INSERT INTO status_stats (status_id, quotes_count, created_at, updated_at)
           VALUES ($1, 1, now(), now())
           ON CONFLICT (status_id) DO UPDATE
             SET quotes_count = status_stats.quotes_count + 1, updated_at = now()"#,
        status.id,
    )
    .execute(&state.db)
    .await
    {
        tracing::error!(status_id = status.id, error = %e, "failed to count a federated quote");
    }

    let authorization_uri = format!(
        "https://{domain}/users/{}/quote_authorizations/{quote_id}",
        status.username
    );
    sqlx::query!(
        "UPDATE quotes SET approval_uri = $2 WHERE id = $1",
        quote_id,
        authorization_uri,
    )
    .execute(&state.db)
    .await?;

    let accept_id = format!("{actor_url}#accepts/quote_requests/{quote_id}");
    if let Ok(accept) = crate::federation::consent::accept(
        &accept_id,
        &actor_url,
        &quoter_uri,
        req_id,
        &authorization_uri,
    ) {
        if let Err(e) =
            crate::federation::delivery::deliver_to_inboxes(state, accept, vec![inbox], key_id)
                .await
        {
            tracing::warn!(error = %e, "failed to enqueue quote Accept");
        }
    }

    Ok(())
}

/// Handle an incoming `FeatureRequest`: a remote collection wants to feature one
/// of our local accounts. We fetch/store the remote collection, record an
/// accepted item, and reply with an `Accept` whose `result` points at a
/// `FeatureAuthorization` we serve. (Rejection policy is intentionally simple:
/// suspended local accounts are skipped.)
pub(super) async fn handle_feature_request(
    state: &AppState,
    instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let req_id = activity.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let account_uri = activity
        .get("object")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let collection_uri = activity
        .get("instrument")
        .and_then(|v| {
            if v.is_string() {
                v.as_str()
            } else {
                v.get("id").and_then(|i| i.as_str())
            }
        })
        .unwrap_or("");
    if req_id.is_empty() || account_uri.is_empty() || collection_uri.is_empty() {
        return Ok(());
    }

    // The featured account must be local and active.
    let Some(local) = sqlx::query!(
        r#"SELECT id, username, suspended_at, requested_deletion_at, id_scheme
           FROM accounts WHERE uri = $1 AND domain IS NULL"#,
        account_uri,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    if local.suspended_at.is_some() || local.requested_deletion_at.is_some() {
        return Ok(());
    }

    // Fetch the remote FeaturedCollection to learn its owner and name.
    let coll: Value = match crate::federation::fetch::signed_get_json(state, collection_uri).await {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let owner_uri = coll
        .get("attributedTo")
        .and_then(|v| {
            if v.is_string() {
                v.as_str()
            } else {
                v.get("id").and_then(|i| i.as_str())
            }
        })
        .unwrap_or("");
    if owner_uri.is_empty() {
        return Ok(());
    }
    let Ok(owner_id) = resolve_or_fetch_remote_account(state, owner_uri).await else {
        return Ok(());
    };
    let name = coll
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Featured collection");
    let sensitive = coll
        .get("sensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let discoverable = coll
        .get("discoverable")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    // Upsert the remote collection (local = false).
    let collection_id = sqlx::query_scalar!(
        r#"INSERT INTO collections
             (account_id, name, discoverable, local, sensitive, item_count, uri, created_at, updated_at)
           VALUES ($1, $2, $3, false, $4, 0, $5, now(), now())
           ON CONFLICT (uri) WHERE uri IS NOT NULL
             DO UPDATE SET name = EXCLUDED.name, updated_at = now()
           RETURNING id"#,
        owner_id,
        name,
        discoverable,
        sensitive,
        collection_uri,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(collection_id) = collection_id else {
        return Ok(());
    };

    // Record the accepted item with our authorization URI.
    let item_id = sqlx::query_scalar!(
        r#"INSERT INTO collection_items
             (collection_id, account_id, state, activity_uri, position, created_at, updated_at)
           VALUES ($1, $2, 1, $3,
                   (SELECT COALESCE(MAX(position), 0) + 1 FROM collection_items WHERE collection_id = $1),
                   now(), now())
           ON CONFLICT (account_id, collection_id)
             DO UPDATE SET state = 1, activity_uri = EXCLUDED.activity_uri, updated_at = now()
           RETURNING id"#,
        collection_id,
        local.id,
        req_id,
    )
    .fetch_one(&state.db)
    .await?;

    let domain = &instance.domain;
    let authorization_uri = format!(
        "https://{domain}/users/{}/feature_authorizations/{item_id}",
        local.username
    );
    sqlx::query!(
        "UPDATE collection_items SET approval_uri = $2 WHERE id = $1",
        item_id,
        authorization_uri,
    )
    .execute(&state.db)
    .await?;

    // Reply with Accept(result = our FeatureAuthorization) to the collection owner.
    let owner = sqlx::query!(
        "SELECT uri, inbox_url, shared_inbox_url FROM accounts WHERE id = $1",
        owner_id,
    )
    .fetch_one(&state.db)
    .await?;
    if !crate::federation::keypair::has_signing_key(state, local.id)
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }

    let inbox = if !owner.shared_inbox_url.is_empty() {
        owner.shared_inbox_url
    } else {
        owner.inbox_url
    };
    if !inbox.is_empty() {
        let actor_url =
            crate::federation::tag::account_uri(domain, local.id, local.id_scheme, &local.username);
        let accept_id = format!("{actor_url}#accepts/feature_requests/{item_id}");
        let owner_uri = owner.uri.unwrap_or_default();
        if let Ok(accept) = crate::federation::consent::accept(
            &accept_id,
            &actor_url,
            &owner_uri,
            req_id,
            &authorization_uri,
        ) {
            let key_id = format!("{actor_url}#main-key");
            if let Err(e) =
                crate::federation::delivery::deliver_to_inboxes(state, accept, vec![inbox], key_id)
                    .await
            {
                tracing::warn!(error = %e, "failed to enqueue feature Accept");
            }
        }
    }

    Ok(())
}
