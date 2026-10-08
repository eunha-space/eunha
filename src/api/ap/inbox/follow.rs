//! Inbound relationship-lifecycle activities: `Follow`, `Undo` (of
//! Follow/Like/Announce), and `Accept`/`Reject` of an outbound follow (and of
//! our quote/feature requests).

use serde_json::Value;

use crate::{error::AppResult, state::AppState};

use super::{delete_arrived_first, delete_later, resolve_or_fetch_remote_account};

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
                let key_id = crate::federation::instance_actor::key_id(&instance.domain);
                // `reject_follow_request!`: a `Reject` of a
                // `FollowRequest.new`, which has no id of its own.
                if let Ok(reject) = crate::federation::relationships::reject_follow(
                    object_uri,
                    actor_uri,
                    None,
                    Some(activity_uri),
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

    // A request already pending only takes the Follow's id ("Update id of
    // already-existing follow requests"), before anything else is asked of
    // it: it is neither rejected nor notified of again.
    let pending = sqlx::query!(
        "UPDATE follow_requests SET uri = $3, updated_at = now()
         WHERE account_id = $1 AND target_account_id = $2",
        follower_id,
        target.id,
        Some(activity_uri).filter(|uri| !uri.is_empty()),
    )
    .execute(&state.db)
    .await?;
    if pending.rows_affected() > 0 {
        return Ok(());
    }

    let follower = crate::api::mastodon::accounts::fetch_account(state, follower_id).await?;

    // Reject the follow up front when the target blocks the follower —
    // directly or by domain — or has moved (`instance_actor?` was answered
    // above): a `Reject` of a `FollowRequest.new`, which has no id of its
    // own (`reject_follow_request!`).
    let should_reject = target.moved_to_account_id.is_some()
        || sqlx::query_scalar!(
            r#"SELECT EXISTS(
                 SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2
                 UNION ALL
                 SELECT 1 FROM account_domain_blocks WHERE account_id = $1 AND domain = $3
               ) AS "exists!""#,
            target.id,
            follower_id,
            follower.domain,
        )
        .fetch_one(&state.db)
        .await?;
    if should_reject {
        crate::api::mastodon::accounts::send_reject_follow(
            state,
            &target,
            &follower,
            None,
            Some(activity_uri),
        )
        .await;
        return Ok(());
    }

    // "Fast-forward repeat follow requests": a follow already there takes
    // the Follow's id and is accepted again (`AuthorizeFollowService` with
    // `skip_follow_request:`, an `Accept` of a `FollowRequest.new`).
    let refollowed = sqlx::query!(
        "UPDATE follows SET uri = $3, updated_at = now() WHERE account_id = $1 AND target_account_id = $2",
        follower_id,
        target.id,
        Some(activity_uri).filter(|uri| !uri.is_empty()),
    )
    .execute(&state.db)
    .await?;
    if refollowed.rows_affected() > 0 {
        crate::api::mastodon::accounts::send_accept_follow(
            state,
            &target,
            &follower,
            None,
            Some(activity_uri),
        )
        .await;
        return Ok(());
    }

    // `FollowRequest.create!(uri: @json['id'])`, whose `set_uri` names a
    // Follow that came without an id.
    let uri = Some(activity_uri)
        .filter(|uri| !uri.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| crate::federation::relationships::generate_uri(&state.instance.domain));
    let created = sqlx::query_scalar!(
        r#"INSERT INTO follow_requests (account_id, target_account_id, uri, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())
           ON CONFLICT (account_id, target_account_id) DO NOTHING
           RETURNING id"#,
        follower_id,
        target.id,
        uri,
    )
    .fetch_optional(&state.db)
    .await?;
    if created.is_none() {
        // Another delivery of the same Follow made it first.
        return Ok(());
    }

    let acct = follower.acct().to_string();
    let avatar = crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &follower);
    // A locked target, or a silenced follower, holds the follow as a request
    // (`target_account.locked? || @account.silenced?`); otherwise
    // `AuthorizeFollowService` makes it a follow and sends the `Accept`.
    if target.locked || follower.silenced_at.is_some() {
        crate::push::create_and_push(
            state,
            target.id,
            follower_id,
            "follow_request",
            None,
            format!("{} wants to follow you", follower.display_name),
            acct,
            avatar,
        )
        .await;
    } else {
        crate::api::mastodon::accounts::authorize_follow(state, follower_id, target.id).await?;
        crate::push::create_and_push(
            state,
            target.id,
            follower_id,
            "follow",
            None,
            format!("{} followed you", follower.display_name),
            acct,
            avatar,
        )
        .await;
    }

    Ok(())
}

/// `ActivityPub::Activity::Undo#perform`: what the sender takes back is
/// looked up as the sender's own, never by its id alone, so that no one can
/// undo what someone else did.
pub(super) async fn handle_undo(
    state: &AppState,
    _instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let object = activity.get("object");
    let object_uri = object
        .and_then(crate::federation::json_ld::value_or_id)
        .filter(|uri| !uri.is_empty());
    // `@account`, the sender, whom the inbox knows.
    let Some(account_id) = sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE uri = $1 AND domain IS NOT NULL ORDER BY id LIMIT 1",
        actor_uri,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let object_type = object
        .filter(|o| o.is_object())
        .and_then(|o| o.get("type"))
        .and_then(|t| t.as_str());
    // `target_uri`: `value_or_id(@object['object'])`.
    let target_uri = object
        .filter(|o| o.is_object())
        .and_then(|o| o.get("object"))
        .and_then(crate::federation::json_ld::value_or_id);

    match object_type {
        Some("Announce") => {
            // `undo_announce`: the sender's status with the Announce's id, or
            // its `atomUri`, removed by `RemoveStatusService`; one not seen
            // yet is remembered, so that the Announce is skipped.
            let Some(announce_uri) = object_uri else {
                return Ok(());
            };
            let atom_uri = object
                .and_then(|o| o.get("atomUri"))
                .and_then(|u| u.as_str())
                .filter(|u| !u.is_empty());
            let mut boost_id = None;
            for candidate in std::iter::once(announce_uri).chain(atom_uri) {
                boost_id = sqlx::query_scalar!(
                    "SELECT id FROM statuses
                     WHERE uri = $1 AND account_id = $2 AND deleted_at IS NULL",
                    candidate,
                    account_id,
                )
                .fetch_optional(&state.db)
                .await?;
                if boost_id.is_some() {
                    break;
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
        Some("Accept") => {
            // `undo_accept`: the sender's acceptance of a follow by one of
            // ours taken back, which leaves the follow a request again
            // (`Follow#revoke_request!`).
            if let Some(uri) = target_uri {
                revoke_follow_request(state, account_id, uri).await?;
            }
        }
        Some("Follow") => {
            // `undo_follow`: the sender's follow of, or request to follow, the
            // local account the Follow names.
            let Some(target_id) = local_target(state, target_uri).await else {
                return Ok(());
            };
            if !unfollow(state, account_id, target_id).await? {
                if let Some(uri) = object_uri {
                    delete_later(state, actor_uri, uri).await;
                }
            }
        }
        Some("Like") => {
            // `undo_like`: the sender's favourite of a local post.
            let Some(status_id) = (match target_uri {
                Some(uri) => super::kept_status(state, uri).await?,
                None => None,
            }) else {
                return Ok(());
            };
            let local_author = sqlx::query_scalar!(
                r#"SELECT (a.domain IS NULL) AS "local!" FROM statuses s
                   JOIN accounts a ON a.id = s.account_id WHERE s.id = $1"#,
                status_id,
            )
            .fetch_one(&state.db)
            .await?;
            if !local_author {
                return Ok(());
            }
            if !unfavourite(state, account_id, status_id).await? {
                if let Some(uri) = object_uri {
                    delete_later(state, actor_uri, uri).await;
                }
            }
        }
        Some("Block") => {
            // `undo_block`: the sender's block of a local account.
            let Some(target_id) = local_target(state, target_uri).await else {
                return Ok(());
            };
            if !unblock(state, account_id, target_id).await? {
                if let Some(uri) = object_uri {
                    delete_later(state, actor_uri, uri).await;
                }
            }
        }
        None => {
            // `handle_reference`: an object given by its id alone, guessed
            // at among the sender's boosts, follows and requests, and blocks
            // (`try_undo_announce || try_undo_follow || try_undo_block`), or
            // else remembered.
            let Some(uri) = object_uri else {
                return Ok(());
            };
            let boost_id = sqlx::query_scalar!(
                "SELECT id FROM statuses
                 WHERE uri = $1 AND account_id = $2 AND reblog_of_id IS NOT NULL AND deleted_at IS NULL",
                uri,
                account_id,
            )
            .fetch_optional(&state.db)
            .await?;
            if let Some(boost_id) = boost_id {
                crate::remove_status::call(
                    state,
                    boost_id,
                    crate::remove_status::Options::default(),
                )
                .await?;
                return Ok(());
            }
            let followed = sqlx::query_scalar!(
                r#"SELECT target_account_id AS "target_account_id?" FROM follow_requests WHERE account_id = $1 AND uri = $2
                   UNION ALL
                   SELECT target_account_id FROM follows WHERE account_id = $1 AND uri = $2
                   LIMIT 1"#,
                account_id,
                uri,
            )
            .fetch_optional(&state.db)
            .await?
            .flatten();
            if let Some(target_id) = followed {
                unfollow(state, account_id, target_id).await?;
                return Ok(());
            }
            let blocked = sqlx::query_scalar!(
                "SELECT target_account_id FROM blocks WHERE account_id = $1 AND uri = $2",
                account_id,
                uri,
            )
            .fetch_optional(&state.db)
            .await?;
            if let Some(target_id) = blocked {
                unblock(state, account_id, target_id).await?;
                return Ok(());
            }
            delete_later(state, actor_uri, uri).await;
        }
        Some(_) => {}
    }

    Ok(())
}

/// `account_from_uri(target_uri)`, when it is a local account.
async fn local_target(state: &AppState, uri: Option<&str>) -> Option<i64> {
    let id = crate::federation::local_uri::account(state, uri?).await?;
    sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE id = $1 AND domain IS NULL",
        id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
}

/// `@account.unfollow!(target_account)`, or the request destroyed when there
/// is no follow; each takes its notification with it (`has_one
/// :notification, dependent: :destroy`). Says whether there was either.
async fn unfollow(state: &AppState, account_id: i64, target_id: i64) -> AppResult<bool> {
    if let Some(follow_id) = sqlx::query_scalar!(
        "DELETE FROM follows WHERE account_id = $1 AND target_account_id = $2 RETURNING id",
        account_id,
        target_id,
    )
    .fetch_optional(&state.db)
    .await?
    {
        sqlx::query!(
            "DELETE FROM notifications WHERE activity_type = 'Follow' AND activity_id = $1",
            follow_id,
        )
        .execute(&state.db)
        .await?;
        // `AccountStat`'s `update_index('accounts', :account)`.
        crate::search::elasticsearch::indexing::accounts(state, &[account_id, target_id]).await;
        if let Err(e) = crate::counters::on_follow_removed(state, account_id, target_id).await {
            tracing::error!(error = %e, "failed to uncount a federated unfollow");
        }
        return Ok(true);
    }
    if sqlx::query!(
        "DELETE FROM follow_requests WHERE account_id = $1 AND target_account_id = $2 RETURNING id",
        account_id,
        target_id,
    )
    .fetch_optional(&state.db)
    .await?
    .is_some()
    {
        sqlx::query!(
            "DELETE FROM notifications WHERE account_id = $1 AND from_account_id = $2 AND type = 'follow_request'",
            target_id,
            account_id,
        )
        .execute(&state.db)
        .await?;
        return Ok(true);
    }
    Ok(false)
}

/// The favourite destroyed, with its notification and the status's count.
/// Says whether there was one.
async fn unfavourite(state: &AppState, account_id: i64, status_id: i64) -> AppResult<bool> {
    let removed = sqlx::query!(
        "DELETE FROM favourites WHERE account_id = $1 AND status_id = $2",
        account_id,
        status_id
    )
    .execute(&state.db)
    .await?
    .rows_affected()
        > 0;
    if !removed {
        return Ok(false);
    }
    sqlx::query!(
        r#"DELETE FROM notifications
           WHERE from_account_id = $1 AND "type" = 'favourite'
             AND activity_type = 'Status' AND activity_id = $2"#,
        account_id,
        status_id,
    )
    .execute(&state.db)
    .await?;
    crate::search::elasticsearch::indexing::status_interaction(state, status_id).await;
    sqlx::query!(
        r#"UPDATE status_stats SET favourites_count = (SELECT COUNT(*) FROM favourites WHERE status_id = $1), untrusted_favourites_count = CASE WHEN untrusted_favourites_count IS NULL THEN NULL ELSE LEAST(GREATEST(untrusted_favourites_count + (SELECT COUNT(*) FROM favourites WHERE status_id = $1) - favourites_count, 0), 100000000) END, updated_at = now() WHERE status_id = $1"#,
        status_id
    )
    .execute(&state.db)
    .await?;
    Ok(true)
}

/// `UnblockService` for a remote blocker: the block destroyed. Says whether
/// there was one.
async fn unblock(state: &AppState, account_id: i64, target_id: i64) -> AppResult<bool> {
    Ok(sqlx::query!(
        "DELETE FROM blocks WHERE account_id = $1 AND target_account_id = $2",
        account_id,
        target_id
    )
    .execute(&state.db)
    .await?
    .rows_affected()
        > 0)
}

/// `Follow#revoke_request!`: the follow of the sender with the id `uri`
/// becomes a request again, and is uncounted.
async fn revoke_follow_request(state: &AppState, target_id: i64, uri: &str) -> AppResult<()> {
    let Some(follow) = sqlx::query!(
        r#"SELECT id, account_id, show_reblogs, notify, languages FROM follows
           WHERE target_account_id = $1 AND uri = $2"#,
        target_id,
        uri,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        r#"INSERT INTO follow_requests (account_id, target_account_id, show_reblogs, notify,
                                        languages, uri, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, now(), now())
           ON CONFLICT DO NOTHING"#,
        follow.account_id,
        target_id,
        follow.show_reblogs,
        follow.notify,
        follow.languages.as_deref(),
        uri,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM follows WHERE id = $1", follow.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query!(
        "DELETE FROM notifications WHERE activity_type = 'Follow' AND activity_id = $1",
        follow.id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    crate::search::elasticsearch::indexing::accounts(state, &[follow.account_id, target_id]).await;
    if let Err(e) = crate::counters::on_follow_removed(state, follow.account_id, target_id).await {
        tracing::error!(error = %e, "failed to uncount a revoked follow");
    }
    Ok(())
}

/// `ActivityPub::Activity::Accept#perform` and `ActivityPub::Activity::Reject
/// #perform`: the sender answers something of ours, found by the object's id
/// among what was asked of the sender alone, so that no one can answer for
/// someone else. The first thing found is answered, and nothing after it.
pub(super) async fn handle_accept_reject(
    state: &AppState,
    _instance: &crate::config::InstanceConfig,
    activity: &Value,
) -> AppResult<()> {
    let accept = activity.get("type").and_then(|t| t.as_str()) == Some("Accept");
    let object = activity.get("object");
    // `object_uri`.
    let object_uri = object
        .and_then(crate::federation::json_ld::value_or_id)
        .filter(|uri| !uri.is_empty());

    // `return accept_follow_for_relay if relay_follow?`, and the same for a
    // Reject: the answer to a relay's Follow, found by the Follow's id.
    if let Some(uri) = object_uri {
        if crate::relays::answered(state, uri, accept)
            .await
            .map_err(crate::error::AppError::Internal)?
        {
            return Ok(());
        }
    }

    // `@account`, the sender, whom the inbox knows.
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let Some(account_id) = sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE uri = $1 AND domain IS NOT NULL ORDER BY id LIMIT 1",
        actor_uri,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };

    if let Some(uri) = object_uri {
        // `follow_request_from_object`: `FollowRequest.find_by(target_account:
        // @account, uri: object_uri)`.
        if let Some(request_id) = sqlx::query_scalar!(
            "SELECT id FROM follow_requests WHERE target_account_id = $1 AND uri = $2",
            account_id,
            uri,
        )
        .fetch_optional(&state.db)
        .await?
        {
            if accept {
                accept_follow(state, request_id, account_id).await?;
            } else {
                reject_follow_request(state, request_id).await?;
            }
            return Ok(());
        }
        // `follow_from_object`, for a Reject: the sender removes a follower
        // it had accepted (`UnfollowService`).
        if !accept {
            if let Some(follower_id) = sqlx::query_scalar!(
                "SELECT account_id FROM follows WHERE target_account_id = $1 AND uri = $2",
                account_id,
                uri,
            )
            .fetch_optional(&state.db)
            .await?
            {
                crate::api::mastodon::accounts::unfollow(state, follower_id, account_id, false)
                    .await?;
                return Ok(());
            }
        }
        // `quote_request_from_object`.
        if super::quote::handle_quote_answer(state, activity, accept).await? {
            return Ok(());
        }
        // `feature_request_from_object`.
        if super::feature::answer_feature_request(state, activity, uri, accept).await? {
            return Ok(());
        }
    }

    // `accept_embedded_follow` / `reject_embedded_follow`: a Follow whose id
    // is not known here, found by who asked whom.
    let embedded_follow = object
        .filter(|o| o.is_object())
        .filter(|o| o.get("type").and_then(|t| t.as_str()) == Some("Follow"));
    if let Some(follow) = embedded_follow {
        let target_uri = follow
            .get("actor")
            .and_then(crate::federation::json_ld::value_or_id);
        let Some(target_id) = local_target(state, target_uri).await else {
            return Ok(());
        };
        let request_id = sqlx::query_scalar!(
            "SELECT id FROM follow_requests WHERE account_id = $1 AND target_account_id = $2",
            target_id,
            account_id,
        )
        .fetch_optional(&state.db)
        .await?;
        if accept {
            if let Some(request_id) = request_id {
                accept_follow(state, request_id, account_id).await?;
            }
        } else {
            if let Some(request_id) = request_id {
                reject_follow_request(state, request_id).await?;
            }
            let following = sqlx::query_scalar!(
                r#"SELECT EXISTS (SELECT 1 FROM follows
                                  WHERE account_id = $1 AND target_account_id = $2) AS "e!""#,
                target_id,
                account_id,
            )
            .fetch_one(&state.db)
            .await?;
            if following {
                crate::api::mastodon::accounts::unfollow(state, target_id, account_id, false)
                    .await?;
            }
        }
    }

    Ok(())
}

/// `Accept#accept_follow!`: the request becomes a follow, and the account
/// that accepted it, `target_id`, is fetched again when this is its first
/// follower here (`RemoteAccountRefreshWorker`), for what it shows only to
/// followers.
async fn accept_follow(state: &AppState, request_id: i64, target_id: i64) -> AppResult<()> {
    // `!request.target_account.followers.local.exists?`.
    let is_first_follow = !sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM follows f JOIN accounts a ON a.id = f.account_id
                          WHERE f.target_account_id = $1 AND a.domain IS NULL) AS "e!""#,
        target_id,
    )
    .fetch_one(&state.db)
    .await?;
    authorize_follow_request(state, request_id).await?;
    if is_first_follow {
        crate::jobs::perform_async(
            state,
            crate::federation::process_account::RemoteAccountRefreshWorker {
                account_id: target_id,
            },
        )
        .await
        .map_err(crate::error::AppError::Internal)?;
    }
    Ok(())
}

/// `FollowRequest#reject!`, which is `destroy!`: the request goes, and its
/// notification with it (`has_one :notification, dependent: :destroy`).
async fn reject_follow_request(state: &AppState, request_id: i64) -> AppResult<()> {
    sqlx::query!("DELETE FROM follow_requests WHERE id = $1", request_id)
        .execute(&state.db)
        .await?;
    sqlx::query!(
        "DELETE FROM notifications WHERE activity_type = 'FollowRequest' AND activity_id = $1",
        request_id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `FollowRequest#authorize!` of the follow request `request_id`.
pub(crate) async fn authorize_follow_request(state: &AppState, request_id: i64) -> AppResult<()> {
    if let Some(request) = sqlx::query!(
        "SELECT account_id, target_account_id FROM follow_requests WHERE id = $1",
        request_id
    )
    .fetch_optional(&state.db)
    .await?
    {
        crate::api::mastodon::accounts::authorize(
            state,
            request.account_id,
            request.target_account_id,
        )
        .await?;
    }
    Ok(())
}
