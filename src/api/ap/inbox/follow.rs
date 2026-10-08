//! Inbound relationship-lifecycle activities: `Follow`, `Undo` (of
//! Follow/Like/Announce), and `Accept`/`Reject` of an outbound follow (and of
//! our quote/feature requests).

use serde_json::Value;

use crate::{error::AppResult, state::AppState};

use super::{
    delete_arrived_first, delete_later, refresh_collection_item_count,
    resolve_or_fetch_remote_account,
};

pub(super) async fn handle_follow(
    state: &AppState,
    instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let object_uri = activity
        .get("object")
        .and_then(|o| o.as_str())
        .unwrap_or("");
    let activity_uri = activity.get("id").and_then(|i| i.as_str()).unwrap_or("");

    // Skip a Follow whose Undo already arrived out of order: the Undo(Follow)
    // recorded a tombstone for this activity id (Mastodon's delete_arrived_first?).
    if delete_arrived_first(state, actor_uri, activity_uri).await {
        return Ok(());
    }

    // The instance actor cannot be followed (Mastodon rejects these): reply with
    // a Reject signed by the instance actor.
    if object_uri == crate::federation::instance_actor::actor_url(&instance.domain) {
        if let Ok(follower_id) = resolve_or_fetch_remote_account(state, actor_uri).await {
            let inbox =
                sqlx::query_scalar!("SELECT inbox_url FROM accounts WHERE id = $1", follower_id,)
                    .fetch_optional(&state.db)
                    .await?
                    .filter(|s| !s.is_empty());
            if let Some(inbox) = inbox {
                let reject_id = format!(
                    "https://{}/activities/{}",
                    instance.domain,
                    crate::snowflake::next_id()
                );
                let key_id = crate::federation::instance_actor::key_id(&instance.domain);
                if let Ok(reject) = crate::federation::activity::reject_follow(
                    &reject_id,
                    object_uri,
                    activity_uri,
                    actor_uri,
                    object_uri,
                ) {
                    if let Err(e) = crate::federation::delivery::deliver_to_inboxes(
                        state,
                        reject,
                        vec![inbox],
                        key_id,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "failed to enqueue instance-actor Reject(Follow)");
                    }
                }
            }
        }
        return Ok(());
    }

    // Resolve the local target account. Local accounts derive their actor URL
    // from id_scheme + username/id and leave the `uri` column empty, so the
    // Follow's `object` must be matched against the derived URL (as
    // resolve_or_fetch_remote_account does) rather than the `uri` column, which
    // would miss them and silently drop the follow.
    let target_id: Option<i64> = if let Ok(parsed) = url::Url::parse(object_uri) {
        let on_our_host = parsed
            .host_str()
            .is_some_and(|h| h.eq_ignore_ascii_case(&instance.domain));
        let segments: Vec<&str> = parsed
            .path_segments()
            .map(|s| s.collect())
            .unwrap_or_default();
        if on_our_host {
            match segments.as_slice() {
                // https://{domain}/users/{username}
                ["users", username] => {
                    sqlx::query_scalar!(
                    "SELECT id FROM accounts WHERE lower(username) = lower($1) AND domain IS NULL",
                    username,
                )
                    .fetch_optional(&state.db)
                    .await?
                }
                // https://{domain}/ap/users/{id}
                ["ap", "users", id] => match id.parse::<i64>() {
                    Ok(numeric) => {
                        sqlx::query_scalar!(
                            "SELECT id FROM accounts WHERE id = $1 AND domain IS NULL",
                            numeric,
                        )
                        .fetch_optional(&state.db)
                        .await?
                    }
                    Err(_) => None,
                },
                _ => None,
            }
        } else {
            None
        }
    } else {
        None
    };
    let Some(target_id) = target_id else {
        return Ok(());
    };
    let target = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        target_id,
    )
    .fetch_one(&state.db)
    .await?;

    let follower_id = resolve_or_fetch_remote_account(state, actor_uri).await?;

    // Fetch the follower's account for push notification details and to decide
    // whether a silenced follower must go through a request.
    let follower = sqlx::query!(
        "SELECT display_name, username, domain, avatar_remote_url, silenced_at FROM accounts WHERE id = $1",
        follower_id,
    )
    .fetch_optional(&state.db)
    .await?;
    let follower_silenced = follower.as_ref().is_some_and(|f| f.silenced_at.is_some());

    // Details for any Accept/Reject we sign as the (local) target and deliver
    // back to the follower. `object_uri` is the target's own actor URL.
    let key_id = format!("{object_uri}#main-key");
    let can_sign = crate::federation::keypair::has_signing_key(state, target.id)
        .await
        .unwrap_or(false);
    let follower_inbox =
        sqlx::query_scalar!("SELECT inbox_url FROM accounts WHERE id = $1", follower_id,)
            .fetch_optional(&state.db)
            .await?
            .filter(|s| !s.is_empty());

    // Reject the follow up front (Mastodon ActivityPub::Activity::Follow) when
    // the target blocks the follower — directly or by domain — or has moved.
    let follower_domain = follower.as_ref().and_then(|f| f.domain.clone());
    let should_reject = target.moved_to_account_id.is_some()
        || sqlx::query_scalar!(
            r#"SELECT EXISTS(
                 SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2
                 UNION ALL
                 SELECT 1 FROM account_domain_blocks WHERE account_id = $1 AND domain = $3
               ) AS "exists!""#,
            target.id,
            follower_id,
            follower_domain,
        )
        .fetch_one(&state.db)
        .await?;
    if should_reject {
        if can_sign {
            if let Some(ref inbox) = follower_inbox {
                let reject_id = format!(
                    "https://{}/activities/{}",
                    instance.domain,
                    crate::snowflake::next_id()
                );
                if let Ok(reject) = crate::federation::activity::reject_follow(
                    &reject_id,
                    object_uri,
                    activity_uri,
                    actor_uri,
                    object_uri,
                ) {
                    if let Err(e) = crate::federation::delivery::deliver_to_inboxes(
                        state,
                        reject,
                        vec![inbox.clone()],
                        key_id.clone(),
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "failed to enqueue Reject(Follow)");
                    }
                }
            }
        }
        return Ok(());
    }

    // Fast-forward a repeat Follow: if the follower already follows the target,
    // refresh the stored uri and re-send Accept rather than opening a new
    // request (matches Mastodon's existing-follow fast path).
    let already_follows = sqlx::query_scalar!(
        r#"SELECT EXISTS(
             SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2
           ) AS "exists!""#,
        follower_id,
        target.id,
    )
    .fetch_one(&state.db)
    .await?;
    if already_follows {
        sqlx::query!(
            "UPDATE follows SET uri = $3, updated_at = now() WHERE account_id = $1 AND target_account_id = $2",
            follower_id,
            target.id,
            activity_uri,
        )
        .execute(&state.db)
        .await?;
        if can_sign {
            if let Some(ref inbox) = follower_inbox {
                let accept_id = format!(
                    "https://{}/activities/{}",
                    instance.domain,
                    crate::snowflake::next_id()
                );
                if let Ok(accept) = crate::federation::activity::accept_follow(
                    &accept_id,
                    object_uri,
                    activity_uri,
                    actor_uri,
                    object_uri,
                ) {
                    if let Err(e) = crate::federation::delivery::deliver_to_inboxes(
                        state,
                        accept,
                        vec![inbox.clone()],
                        key_id.clone(),
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "failed to enqueue Accept(Follow)");
                    }
                }
            }
        }
        return Ok(());
    }

    // Decide what to do via eunha's follow policy (federation::follow); eunha executes
    // the returned Actions against Postgres + delivery.
    let (Ok(follow_id), Ok(actor_iri), Ok(object_iri)) = (
        crate::federation::portable::iri(activity_uri),
        crate::federation::portable::iri(actor_uri),
        crate::federation::portable::iri(object_uri),
    ) else {
        return Ok(());
    };
    let follow = ojak_vocab::Follow {
        id: Some(follow_id),
        actors: vec![ojak_vocab::AnyActor::Iri(actor_iri)],
        objects: vec![ojak_vocab::AnyObject::Iri(object_iri.clone())],
        ..Default::default()
    };
    let accept_id = format!(
        "https://{}/activities/{}",
        instance.domain,
        crate::snowflake::next_id()
    );
    let Ok(accept_iri) = accept_id.parse::<ojak_vocab::Iri>() else {
        return Ok(());
    };

    // A locked target, or a silenced follower, holds the follow as a request
    // (Mastodon: target.locked? || account.silenced?).
    let actions = crate::federation::follow::on_follow(
        follow,
        &object_iri,
        target.locked || follower_silenced,
        accept_iri,
    );

    for action in actions {
        match action {
            crate::federation::follow::Action::RecordFollowRequest => {
                sqlx::query!(
                    r#"INSERT INTO follow_requests (account_id, target_account_id, uri, created_at, updated_at)
                       VALUES ($1, $2, $3, now(), now())
                       ON CONFLICT (account_id, target_account_id) DO UPDATE SET uri = EXCLUDED.uri"#,
                    follower_id,
                    target.id,
                    activity_uri,
                )
                .execute(&state.db)
                .await?;

                if let Some(ref f) = follower {
                    let acct = match &f.domain {
                        Some(d) => format!("{}@{}", f.username, d),
                        None => f.username.clone(),
                    };
                    crate::push::create_and_push(
                        state,
                        target.id,
                        follower_id,
                        "follow_request",
                        None,
                        format!("{} wants to follow you", f.display_name),
                        acct,
                        f.avatar_remote_url.clone().unwrap_or_default(),
                    )
                    .await;
                }
            }
            crate::federation::follow::Action::RecordFollow => {
                // `RETURNING` distinguishes a new follow from a redelivery of
                // one already recorded: federation repeats, and counting on
                // every arrival would inflate the follower count.
                let created = sqlx::query_scalar!(
                    r#"INSERT INTO follows (account_id, target_account_id, uri, created_at, updated_at)
                       VALUES ($1, $2, $3, now(), now())
                       ON CONFLICT (account_id, target_account_id) DO UPDATE SET uri = EXCLUDED.uri
                       RETURNING (xmax = 0) AS "inserted!""#,
                    follower_id,
                    target.id,
                    activity_uri,
                )
                .fetch_one(&state.db)
                .await?;

                // Mastodon's Follow counter callbacks are unconditional, so a
                // follow from another instance moves the same two counts as one
                // made here.
                if created {
                    // `AccountStat`'s `update_index('accounts', :account)`.
                    crate::search::elasticsearch::indexing::accounts(
                        state,
                        &[follower_id, target.id],
                    )
                    .await;
                    if let Err(e) =
                        crate::counters::on_follow_created(&state.db, follower_id, target.id).await
                    {
                        tracing::error!(error = %e, "failed to count a federated follow");
                    }
                }

                if let Some(ref f) = follower {
                    let acct = match &f.domain {
                        Some(d) => format!("{}@{}", f.username, d),
                        None => f.username.clone(),
                    };
                    crate::push::create_and_push(
                        state,
                        target.id,
                        follower_id,
                        "follow",
                        None,
                        format!("{} followed you", f.display_name),
                        acct,
                        f.avatar_remote_url.clone().unwrap_or_default(),
                    )
                    .await;
                }
            }
            crate::federation::follow::Action::SendAccept(accept) => {
                if !crate::federation::keypair::has_signing_key(state, target.id)
                    .await
                    .unwrap_or(false)
                {
                    tracing::warn!(username = %target.username, "local account has no private key; cannot send Accept");
                    continue;
                }
                let follower_inbox = sqlx::query_scalar!(
                    "SELECT inbox_url FROM accounts WHERE id = $1",
                    follower_id,
                )
                .fetch_optional(&state.db)
                .await?
                .filter(|s| !s.is_empty());
                let Some(inbox) = follower_inbox else {
                    tracing::warn!(
                        actor_uri,
                        "cannot send Accept: remote actor has no inbox URL"
                    );
                    continue;
                };
                let activity = crate::federation::activity::document(&*accept);
                let actor_url = crate::federation::tag::account_uri(
                    &instance.domain,
                    target.id,
                    target.id_scheme,
                    &target.username,
                );
                let key_id = format!("{actor_url}#main-key");
                tracing::debug!(inbox, actor_uri, "enqueueing Accept");
                if let Err(e) = crate::federation::delivery::deliver_to_inboxes(
                    state,
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

    Ok(())
}

pub(super) async fn handle_undo(
    state: &AppState,
    _instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let object = activity.get("object");
    let object_type = object.and_then(|o| o.get("type")).and_then(|t| t.as_str());

    match object_type {
        Some("Follow") => {
            let follow_uri = object
                .and_then(|o| o.get("id"))
                .and_then(|i| i.as_str())
                .unwrap_or("");
            // Return who was following whom, so the two counts can come back
            // down: an unfollow that removed a row but left the counts is a
            // count that only ever rises.
            let undone_follow = sqlx::query!(
                "DELETE FROM follows WHERE uri = $1 RETURNING account_id, target_account_id",
                follow_uri
            )
            .fetch_optional(&state.db)
            .await?;
            if let Some(row) = &undone_follow {
                // `AccountStat`'s `update_index('accounts', :account)`.
                crate::search::elasticsearch::indexing::accounts(
                    state,
                    &[row.account_id, row.target_account_id],
                )
                .await;
                if let Err(e) = crate::counters::on_follow_removed(
                    &state.db,
                    row.account_id,
                    row.target_account_id,
                )
                .await
                {
                    tracing::error!(error = %e, "failed to uncount a federated unfollow");
                }
            }
            let undone_request = sqlx::query!(
                "DELETE FROM follow_requests WHERE uri = $1 RETURNING account_id, target_account_id",
                follow_uri
            )
            .fetch_optional(&state.db)
            .await?;
            if let Some(req) = &undone_request {
                // Mirror Mastodon's FollowRequest dependent: :destroy — clear the
                // recipient's follow_request notification for the withdrawn request.
                sqlx::query!(
                    "DELETE FROM notifications WHERE account_id = $1 AND from_account_id = $2 AND type = 'follow_request'",
                    req.target_account_id,
                    req.account_id,
                )
                .execute(&state.db)
                .await?;
            }
            // The Follow may not have been processed yet (out-of-order delivery);
            // remember this Undo so a late Follow with the same id is skipped
            // rather than resurrecting the follow.
            if undone_follow.is_none() && undone_request.is_none() {
                delete_later(state, actor_uri, follow_uri).await;
            }
        }
        Some("Like") => {
            let like_uri = object
                .and_then(|o| o.get("id"))
                .and_then(|i| i.as_str())
                .unwrap_or("");
            // object.object is the liked status URI
            let status_uri = object
                .and_then(|o| o.get("object"))
                .and_then(|v| {
                    if v.is_string() {
                        v.as_str()
                    } else {
                        v.get("id").and_then(|i| i.as_str())
                    }
                })
                .unwrap_or("");
            let status_id =
                sqlx::query_scalar!("SELECT id FROM statuses WHERE uri = $1", status_uri)
                    .fetch_optional(&state.db)
                    .await?;
            let account_id =
                sqlx::query_scalar!("SELECT id FROM accounts WHERE uri = $1", actor_uri)
                    .fetch_optional(&state.db)
                    .await?;
            let mut removed = false;
            if let (Some(sid), Some(aid)) = (status_id, account_id) {
                let deleted = sqlx::query!(
                    "DELETE FROM favourites WHERE account_id = $1 AND status_id = $2",
                    aid,
                    sid
                )
                .execute(&state.db)
                .await?;
                removed = deleted.rows_affected() > 0;
                crate::search::elasticsearch::indexing::status_interaction(state, sid).await;
                sqlx::query!(
                    r#"UPDATE status_stats SET favourites_count = (SELECT COUNT(*) FROM favourites WHERE status_id = $1), untrusted_favourites_count = CASE WHEN untrusted_favourites_count IS NULL THEN NULL ELSE LEAST(GREATEST(untrusted_favourites_count + (SELECT COUNT(*) FROM favourites WHERE status_id = $1) - favourites_count, 0), 100000000) END, updated_at = now() WHERE status_id = $1"#,
                    sid
                ).execute(&state.db).await?;
            }
            if !removed {
                delete_later(state, actor_uri, like_uri).await;
            }
        }
        Some("Announce") => {
            // `undo_announce`: the actor's status with the Announce's id, or
            // its `atomUri`, removed by `RemoveStatusService`; one not seen
            // yet is remembered, so that the Announce is skipped.
            let announce = object.filter(|o| o.is_object());
            let announce_uri = announce
                .and_then(|o| o.get("id"))
                .and_then(|i| i.as_str())
                .unwrap_or("");
            if !announce_uri.is_empty() {
                let atom_uri = announce
                    .and_then(|o| o.get("atomUri"))
                    .and_then(|u| u.as_str())
                    .filter(|u| !u.is_empty());
                let actor_id =
                    sqlx::query_scalar!("SELECT id FROM accounts WHERE uri = $1", actor_uri)
                        .fetch_optional(&state.db)
                        .await?;
                let mut boost_id = None;
                if let Some(actor_id) = actor_id {
                    for candidate in std::iter::once(announce_uri).chain(atom_uri) {
                        boost_id = sqlx::query_scalar!(
                            "SELECT id FROM statuses
                             WHERE uri = $1 AND account_id = $2 AND deleted_at IS NULL",
                            candidate,
                            actor_id,
                        )
                        .fetch_optional(&state.db)
                        .await?;
                        if boost_id.is_some() {
                            break;
                        }
                    }
                }
                match boost_id {
                    Some(boost_id) => {
                        crate::remove_status::call(
                            state,
                            boost_id,
                            crate::remove_status::Options::default(),
                        )
                        .await?;
                    }
                    None => delete_later(state, actor_uri, announce_uri).await,
                }
            }
        }
        Some("Block") => {
            let block_uri = object
                .and_then(|o| o.get("id"))
                .and_then(|i| i.as_str())
                .unwrap_or("");
            let block_object_uri = object
                .and_then(|o| o.get("object"))
                .and_then(|v| {
                    if v.is_string() {
                        v.as_str()
                    } else {
                        v.get("id").and_then(|i| i.as_str())
                    }
                })
                .unwrap_or("");
            let blocker_id =
                sqlx::query_scalar!("SELECT id FROM accounts WHERE uri = $1", actor_uri)
                    .fetch_optional(&state.db)
                    .await?;
            let blockee_id = sqlx::query_scalar!(
                "SELECT id FROM accounts WHERE uri = $1 AND domain IS NULL",
                block_object_uri
            )
            .fetch_optional(&state.db)
            .await?;
            let mut removed = false;
            if let (Some(bid), Some(eid)) = (blocker_id, blockee_id) {
                let deleted = sqlx::query!(
                    "DELETE FROM blocks WHERE account_id = $1 AND target_account_id = $2",
                    bid,
                    eid
                )
                .execute(&state.db)
                .await?;
                removed = deleted.rows_affected() > 0;
            }
            if !removed {
                delete_later(state, actor_uri, block_uri).await;
            }
        }
        _ => {}
    }

    Ok(())
}

pub(super) async fn handle_accept_reject(
    state: &AppState,
    _instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let activity_type = activity.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let object = activity.get("object");
    let follow_uri = object.and_then(|o| {
        if o.is_string() {
            o.as_str()
        } else {
            o.get("id").and_then(|i| i.as_str())
        }
    });

    // `return accept_follow_for_relay if relay_follow?`, and the same for a
    // Reject: the answer to a relay's Follow, found by the Follow's id.
    if let Some(uri) = follow_uri {
        if crate::relays::answered(state, uri, activity_type == "Accept")
            .await
            .map_err(crate::error::AppError::Internal)?
        {
            return Ok(());
        }
    }

    if let Some(uri) = follow_uri {
        if activity_type == "Accept" {
            // `FollowRequest#authorize!`: promote follow_request → follows
            // when remote accepts our Follow, moving the list memberships
            // that waited on the request over to the follow before the
            // request (and with it, by cascade, those memberships) goes.
            let promoted = sqlx::query!(
                "SELECT id, account_id, target_account_id, show_reblogs, notify, languages
                 FROM follow_requests WHERE uri = $1",
                uri
            )
            .fetch_optional(&state.db)
            .await?;
            if let Some(row) = promoted {
                let follow_id = sqlx::query_scalar!(
                    r#"INSERT INTO follows (account_id, target_account_id, show_reblogs, notify,
                                            languages, uri, created_at, updated_at)
                       VALUES ($1, $2, $3, $4, $5, $6, now(), now()) ON CONFLICT DO NOTHING
                       RETURNING id"#,
                    row.account_id,
                    row.target_account_id,
                    row.show_reblogs,
                    row.notify,
                    row.languages.as_deref(),
                    uri
                )
                .fetch_optional(&state.db)
                .await?;
                if let Some(follow_id) = follow_id {
                    sqlx::query!(
                        "UPDATE list_accounts SET follow_request_id = NULL, follow_id = $2
                         WHERE follow_request_id = $1",
                        row.id,
                        follow_id,
                    )
                    .execute(&state.db)
                    .await?;
                }
                sqlx::query!("DELETE FROM follow_requests WHERE id = $1", row.id)
                    .execute(&state.db)
                    .await?;
                // `AccountStat`'s `update_index('accounts', :account)`.
                crate::search::elasticsearch::indexing::accounts(
                    state,
                    &[row.account_id, row.target_account_id],
                )
                .await;

                // Update follower/following counts
                let _ = crate::counters::on_follow_created(
                    &state.db,
                    row.account_id,
                    row.target_account_id,
                )
                .await;
                // `MergeWorker` into the home feed of the local account that
                // asked, which also finishes the regeneration its first
                // follow started, and into its lists that hold the account.
                crate::home_feed::merge_into_home_and_lists(
                    state,
                    row.target_account_id,
                    row.account_id,
                )
                .await;
            }
        } else {
            sqlx::query!("DELETE FROM follow_requests WHERE uri = $1", uri)
                .execute(&state.db)
                .await?;
        }

        // Feature-request consent: the object may be one of our outstanding
        // FeatureRequests (matched by collection_items.activity_uri).
        let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
        let item = sqlx::query!(
            r#"SELECT ci.id, ci.collection_id, a.uri AS "account_uri?"
               FROM collection_items ci
               JOIN accounts a ON a.id = ci.account_id
               WHERE ci.activity_uri = $1 AND ci.state = 0"#,
            uri,
        )
        .fetch_optional(&state.db)
        .await?;
        if let Some(item) = item {
            // Only the featured account itself may answer the request.
            if item.account_uri.as_deref() == Some(actor_uri) {
                if activity_type == "Accept" {
                    let approval_uri = activity
                        .get("result")
                        .and_then(|r| {
                            if r.is_string() {
                                r.as_str()
                            } else {
                                r.get("id").and_then(|i| i.as_str())
                            }
                        })
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());
                    sqlx::query!(
                        r#"UPDATE collection_items
                           SET state = 1, approval_uri = $2,
                               approval_last_verified_at = now(), updated_at = now()
                           WHERE id = $1"#,
                        item.id,
                        approval_uri.as_deref(),
                    )
                    .execute(&state.db)
                    .await?;
                } else {
                    sqlx::query!(
                        "UPDATE collection_items SET state = 2, updated_at = now() WHERE id = $1",
                        item.id,
                    )
                    .execute(&state.db)
                    .await?;
                }
                refresh_collection_item_count(state, item.collection_id).await?;
            }
        }

        // Quote-request consent: the object may be one of our outstanding
        // QuoteRequests (`quote_request_from_object`).
        super::quote::handle_quote_answer(state, activity, activity_type == "Accept").await?;
    }

    Ok(())
}
