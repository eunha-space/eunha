//! Inbound feature consent: a remote collection asking to feature a local
//! account (`ActivityPub::Activity::FeatureRequest`), and a remote account's
//! answer to a local collection's request (`Accept#accept_feature_request!`,
//! `Reject#reject_feature_request!`).

use serde_json::Value;

use crate::{error::AppResult, state::AppState};

use crate::federation::featured_collections as featured;
use crate::federation::json_ld::non_matching_uri_hosts;
use ojak_vocab::json_ld_helper::{first_of_value, unsupported_uri_scheme, value_or_id};

/// `CollectionItem` states.
const ACCEPTED: i32 = 1;
const REJECTED: i32 = 2;

/// The sender, as the inbox knows it.
struct Sender {
    id: i64,
    uri: String,
    inbox_url: String,
}

async fn sender(state: &AppState, actor_uri: &str) -> AppResult<Option<Sender>> {
    if actor_uri.is_empty() {
        return Ok(None);
    }
    Ok(sqlx::query!(
        r#"SELECT id, uri AS "uri!", inbox_url FROM accounts
           WHERE uri = $1 AND domain IS NOT NULL ORDER BY id LIMIT 1"#,
        actor_uri,
    )
    .fetch_optional(&state.db)
    .await?
    .map(|row| Sender {
        id: row.id,
        uri: row.uri,
        inbox_url: row.inbox_url,
    }))
}

/// `ActivityPub::Activity::FeatureRequest#perform`: the sender asks, for the
/// collection it names as `instrument` — one of its own, found or fetched —
/// to feature the local account it names as `object`. The request has to
/// come from the sender's own host. When the sender may feature the account
/// (`AccountPolicy#feature?`), an accepted item is recorded, the account
/// told, and an `Accept` whose `result` is the account's
/// `FeatureAuthorization` sent to the sender's inbox; otherwise a `Reject`.
pub(super) async fn handle_feature_request(state: &AppState, activity: &Value) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(value_or_id).unwrap_or("");
    let Some(sender) = sender(state, actor_uri).await? else {
        return Ok(());
    };
    let Some(request_uri) = activity.get("id").and_then(Value::as_str) else {
        return Ok(());
    };
    if non_matching_uri_hosts(&sender.uri, request_uri) {
        return Ok(());
    }

    // `find_or_fetch_collection`: the sender's own, or fetched and then
    // the sender's.
    let Some(collection_uri) = activity.get("instrument").and_then(value_or_id) else {
        return Ok(());
    };
    let known = sqlx::query_scalar!(
        "SELECT id FROM collections WHERE account_id = $1 AND uri = $2",
        sender.id,
        collection_uri,
    )
    .fetch_optional(&state.db)
    .await?;
    let collection_id = match known {
        Some(id) => Some(id),
        None => {
            let fetched = featured::fetch_remote_featured_collection(state, collection_uri, None)
                .await
                .map_err(crate::error::AppError::Internal)?;
            match fetched {
                Some(id) => {
                    sqlx::query_scalar!(
                        "SELECT id FROM collections WHERE id = $1 AND account_id = $2",
                        id,
                        sender.id,
                    )
                    .fetch_optional(&state.db)
                    .await?
                }
                None => None,
            }
        }
    };
    // `uris_to_local_accounts([value_or_id(@json['object'])]).first`: a
    // local account, by its address.
    let featured_account = match activity.get("object").and_then(value_or_id) {
        Some(uri) if crate::federation::local_uri::is_local(state, uri) => {
            match crate::federation::local_uri::account(state, uri).await {
                Some(id) => {
                    sqlx::query!(
                    "SELECT id, username, id_scheme FROM accounts WHERE id = $1 AND domain IS NULL",
                    id,
                )
                    .fetch_optional(&state.db)
                    .await?
                }
                None => None,
            }
        }
        _ => None,
    };
    let (Some(collection_id), Some(featured_account)) = (collection_id, featured_account) else {
        return Ok(());
    };
    let domain = &state.instance.domain;
    let actor = crate::federation::tag::account_uri(
        domain,
        featured_account.id,
        featured_account.id_scheme,
        &featured_account.username,
    );

    let answer = if featured::may_feature(&state.db, sender.id, featured_account.id).await? {
        // `accept_request!`: `collection_items.create!(account:,
        // activity_uri:, state: :accepted)`, which an item already featuring
        // the account refuses.
        let mut tx = state.db.begin().await?;
        let item_id = sqlx::query_scalar!(
            r#"INSERT INTO collection_items
                 (collection_id, account_id, state, activity_uri, position, created_at, updated_at)
               VALUES ($1, $2, $3, $4,
                       (SELECT COALESCE(MAX(position), 0) + 1
                        FROM collection_items WHERE collection_id = $1),
                       now(), now())
               ON CONFLICT (account_id, collection_id) DO NOTHING
               RETURNING id"#,
            collection_id,
            featured_account.id,
            ACCEPTED,
            request_uri,
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(item_id) = item_id else {
            return Ok(());
        };
        crate::api::mastodon::collections::update_item_count(&mut *tx, collection_id, 1).await?;
        tx.commit().await?;

        // `notify_local_user!`.
        crate::push::notify_collection(
            state,
            featured_account.id,
            "added_to_collection",
            ("CollectionItem", item_id),
            sender.id,
        )
        .await;
        // `ActivityPub::AcceptFeatureRequestSerializer`.
        crate::federation::consent::accept(
            &format!("{actor}#accepts/feature_requests/{item_id}"),
            &actor,
            &sender.uri,
            request_uri,
            &crate::api::ap::collections::feature_authorization_uri(
                domain,
                featured_account.id,
                item_id,
            ),
        )
    } else {
        // `reject_request!`: the item is only built, never saved, so the
        // `Reject` (`ActivityPub::RejectFeatureRequestSerializer`) names no
        // item id.
        crate::federation::consent::reject(
            &format!("{actor}#rejects/feature_requests/"),
            &actor,
            &sender.uri,
            request_uri,
        )
    };
    let answer = answer.map_err(crate::error::AppError::Internal)?;

    // `ActivityPub::DeliveryWorker.perform_async(json, @featured_account.id,
    // @account.inbox_url)`.
    if sender.inbox_url.is_empty()
        || !crate::federation::keypair::has_signing_key(state, featured_account.id)
            .await
            .unwrap_or(false)
    {
        return Ok(());
    }
    if let Err(error) = crate::federation::delivery::deliver_to_inboxes(
        state,
        answer,
        vec![sender.inbox_url],
        format!("{actor}#main-key"),
    )
    .await
    {
        tracing::warn!(%error, "failed to enqueue a feature request's answer");
    }
    Ok(())
}

/// `Accept#accept_feature_request!` and `Reject#reject_feature_request!`:
/// the sender answers the request `request_uri` a local collection sent it
/// (`feature_request_from_object`: the item of a local collection featuring
/// the sender, by its `activity_uri`). Accepted, with a `result` on the
/// sender's own host, the item is accepted under that authorization and its
/// `Add` sent to the collection's reach; rejected, it is rejected. Says
/// whether the activity answered such a request.
pub(super) async fn answer_feature_request(
    state: &AppState,
    activity: &Value,
    request_uri: &str,
    accept: bool,
) -> AppResult<bool> {
    let actor_uri = activity.get("actor").and_then(value_or_id).unwrap_or("");
    let Some(sender) = sender(state, actor_uri).await? else {
        return Ok(false);
    };
    let Some(item) = sqlx::query!(
        r#"SELECT ci.id, ci.collection_id FROM collection_items ci
           JOIN collections c ON c.id = ci.collection_id AND c.local
           WHERE ci.activity_uri = $1 AND ci.account_id = $2
           ORDER BY ci.id LIMIT 1"#,
        request_uri,
        sender.id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(false);
    };

    if !accept {
        sqlx::query!(
            "UPDATE collection_items SET state = $2, updated_at = now() WHERE id = $1",
            item.id,
            REJECTED,
        )
        .execute(&state.db)
        .await?;
        return Ok(true);
    }

    let approval_uri = activity
        .get("result")
        .and_then(first_of_value)
        .and_then(value_or_id);
    let Some(approval_uri) = approval_uri else {
        return Ok(true);
    };
    if unsupported_uri_scheme(Some(approval_uri))
        || non_matching_uri_hosts(approval_uri, &sender.uri)
    {
        return Ok(true);
    }
    let accepted = sqlx::query!(
        r#"UPDATE collection_items SET approval_uri = $2, state = $3, updated_at = now()
           WHERE id = $1"#,
        item.id,
        approval_uri,
        ACCEPTED,
    )
    .execute(&state.db)
    .await;
    match accepted {
        Ok(_) => {}
        // An authorization another item already holds: `update!` fails.
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            tracing::debug!(approval_uri, "feature authorization already held");
            return Ok(true);
        }
        Err(error) => return Err(error.into()),
    }
    // `CollectionRawDistributionWorker` with `AddFeaturedItemSerializer`.
    if let Some(add) = crate::api::ap::collections::add_featured_item_activity(
        state,
        &state.instance.domain,
        item.id,
    )
    .await?
    {
        crate::api::mastodon::collections::distribute_collection_raw(
            state,
            item.collection_id,
            add,
        )
        .await?;
    }
    Ok(true)
}
