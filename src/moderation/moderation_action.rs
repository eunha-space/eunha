//! `Admin::ModerationAction`: acting on what a report cites rather than on
//! the account — removing the reported posts and collections, or marking
//! them sensitive — with the strike, audit log entries and notification that
//! come with it.

use crate::db::models::{Account, Status};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

use super::action_log::{self, Target};
use super::role::{self, authorize, flag};
use super::warning::action as warning_action;

/// `Admin::ModerationAction::TYPES`.
pub const TYPES: &[&str] = &["delete", "mark_as_sensitive"];

struct CollectionRow {
    id: i64,
    account_id: i64,
    uri: Option<String>,
    local: bool,
}

/// `Admin::ModerationAction#save!` on report `report_id`, by `actor_id`.
pub async fn save(
    state: &AppState,
    actor_id: i64,
    report_id: i64,
    kind: &str,
    text: Option<String>,
    send_email_notification: bool,
) -> AppResult<()> {
    // `validates :type, presence: true, inclusion: { in: TYPES }`.
    if kind.is_empty() {
        return Err(AppError::Unprocessable(
            "Validation failed: Type can't be blank".into(),
        ));
    }
    if !TYPES.contains(&kind) {
        return Err(AppError::Unprocessable(
            "Validation failed: Type is not included in the list".into(),
        ));
    }
    let report = sqlx::query!(
        "SELECT id, target_account_id, status_ids, category, rule_ids FROM reports WHERE id = $1",
        report_id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let target = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = $1",
        report.target_account_id
    )
    .fetch_one(&state.db)
    .await?;
    let acct = target.acct();
    // `Status.with_discarded.where(id: status_ids)` and `report.collections`.
    let statuses = sqlx::query_as!(
        Status,
        "SELECT * FROM statuses WHERE id = ANY($1) ORDER BY id",
        &report.status_ids
    )
    .fetch_all(&state.db)
    .await?;
    let collections = sqlx::query_as!(
        CollectionRow,
        r#"SELECT c.id, c.account_id, c.uri, c.local FROM collections c
           JOIN collection_reports cr ON cr.collection_id = c.id
           WHERE cr.report_id = $1 ORDER BY c.id"#,
        report_id
    )
    .fetch_all(&state.db)
    .await?;
    let collection_uri = |c: &CollectionRow| {
        c.uri.clone().or_else(|| {
            c.local.then(|| {
                crate::api::mastodon::collections::collection_uri(
                    &state.instance.domain,
                    c.account_id,
                    c.id,
                )
            })
        })
    };
    let status_uri = |s: &Status| {
        s.uri.clone().unwrap_or_else(|| {
            format!(
                "https://{}/users/{}/statuses/{}",
                state.instance.domain, target.username, s.id
            )
        })
    };
    let acting = role::acting(&state.db, actor_id).await?;
    // `Admin::StatusPolicy#destroy?` / `#update?` and the collection policy's:
    // `manage_reports`, asked once per post and collection.
    let may = acting.can(&[flag::MANAGE_REPORTS]);
    let text = text.unwrap_or_default();
    let status_ids: Vec<String> = report.status_ids.iter().map(i64::to_string).collect();

    let strike_action = if kind == "delete" {
        warning_action::DELETE_STATUSES
    } else {
        warning_action::MARK_STATUSES_AS_SENSITIVE
    };
    let mut to_remove: Vec<Status> = vec![];
    let mut to_update: Vec<Status> = vec![];
    // The collections `mark_collections_as_sensitive!` changed, which
    // `UpdateCollectionService` tells their members and reach of.
    let mut made_sensitive: Vec<i64> = vec![];
    // `Account.representative`, who edits a local post marked sensitive.
    let representative = if kind != "delete" && target.is_local() && !statuses.is_empty() {
        crate::federation::instance_actor::representative(state)
            .await
            .map_err(AppError::Internal)?
    } else {
        crate::federation::instance_actor::INSTANCE_ACTOR_ID
    };
    let mut tx = state.db.begin().await?;
    if kind == "delete" {
        if !statuses.is_empty() || !collections.is_empty() {
            authorize(may)?;
        }
        // `delete_statuses!`: each one, discarded or not, is discarded with
        // its boosts and logged, and `RemovalWorker` below removes it.
        for status in &statuses {
            crate::remove_status::discard_with_reblogs_in(&mut tx, status).await?;
            action_log::log(
                &mut *tx,
                actor_id,
                "destroy",
                &Target::status(status.id, &acct, status_uri(status)),
            )
            .await?;
            to_remove.push(status.clone());
        }
        // `delete_collections!`.
        for collection in &collections {
            sqlx::query!("DELETE FROM collections WHERE id = $1", collection.id)
                .execute(&mut *tx)
                .await?;
            crate::api::mastodon::collections::destroy_notifications(&mut *tx, collection.id)
                .await?;
            action_log::log(
                &mut *tx,
                actor_id,
                "destroy",
                &Target::collection(collection.id, &acct, collection_uri(collection)),
            )
            .await?;
        }
    } else {
        if !collections.is_empty() {
            authorize(may)?;
        }
        // `mark_statuses_as_sensitive!`: a post still there that carries
        // media or a preview card.
        for status in &statuses {
            if status.deleted_at.is_some() {
                continue;
            }
            let carries = sqlx::query_scalar!(
                r#"SELECT EXISTS (SELECT 1 FROM media_attachments m JOIN statuses s ON s.id = m.status_id
                                 WHERE m.status_id = $1
                                   AND (s.ordered_media_attachment_ids IS NULL
                                        OR m.id = ANY(s.ordered_media_attachment_ids)))
                       OR EXISTS (SELECT 1 FROM preview_cards_statuses WHERE status_id = $1)
                   AS "e!""#,
                status.id
            )
            .fetch_one(&mut *tx)
            .await?;
            if !carries {
                continue;
            }
            authorize(may)?;
            if target.is_local() {
                // `UpdateStatusService.new.call(status,
                // representative_account.id, sensitive: true)`: the original
                // into the history if it has none, then the sensitive
                // version, edited by the instance's representative. A post
                // already sensitive is not changed (`NoChangesSubmittedError`):
                // no edit, and nothing is sent.
                if !status.sensitive {
                    crate::status_snapshot::create_previous_edit(&mut tx, status.id).await?;
                    sqlx::query!(
                        "UPDATE statuses SET sensitive = true, edited_at = now(), updated_at = now() WHERE id = $1",
                        status.id
                    )
                    .execute(&mut *tx)
                    .await?;
                    crate::status_snapshot::create_edit(&mut tx, status.id, representative).await?;
                    to_update.push(status.clone());
                }
                to_update.push(status.clone());
            } else {
                sqlx::query!(
                    "UPDATE statuses SET sensitive = true, updated_at = now() WHERE id = $1",
                    status.id
                )
                .execute(&mut *tx)
                .await?;
            }
            action_log::log(
                &mut *tx,
                actor_id,
                "update",
                &Target::status(status.id, &acct, status_uri(status)),
            )
            .await?;
        }
        // `mark_collections_as_sensitive!`.
        for collection in &collections {
            let changed = sqlx::query!(
                "UPDATE collections SET sensitive = true, updated_at = now()
                 WHERE id = $1 AND NOT sensitive",
                collection.id
            )
            .execute(&mut *tx)
            .await?
            .rows_affected()
                > 0;
            if changed {
                made_sensitive.push(collection.id);
            }
            action_log::log(
                &mut *tx,
                actor_id,
                "update",
                &Target::collection(collection.id, &acct, collection_uri(collection)),
            )
            .await?;
        }
    }

    // `resolve_report!`.
    sqlx::query!(
        r#"UPDATE reports SET action_taken_at = now(), action_taken_by_account_id = $2,
                  updated_at = now()
           WHERE id = $1"#,
        report_id,
        actor_id
    )
    .execute(&mut *tx)
    .await?;
    action_log::log(&mut *tx, actor_id, "resolve", &Target::report(report_id)).await?;

    // `process_strike!`, citing the report's posts, behind
    // `AccountPolicy#warn?` since Mastodon 4.7.3 (#40646). Refused, nothing
    // done above is kept: eunha acts in one transaction, where upstream's
    // marking as sensitive is not in one and stays done.
    let target_role = role::of_account(&state.db, target.id).await?;
    authorize(super::account_action::warn_policy(
        &acting,
        target_role.as_ref(),
    ))?;
    let warning_id = sqlx::query_scalar!(
        r#"INSERT INTO account_warnings
             (account_id, target_account_id, report_id, action, text, status_ids,
              created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, now(), now())
           RETURNING id"#,
        actor_id,
        target.id,
        report_id,
        strike_action,
        text,
        &status_ids,
    )
    .fetch_one(&mut *tx)
    .await?;

    // `create_tombstones!`: a remote server re-sending what was removed is
    // not taken in again.
    if kind == "delete" && !target.is_local() {
        let mut uris: Vec<String> = vec![];
        for status in &statuses {
            uris.extend(status.uri.clone());
        }
        for collection in &collections {
            uris.extend(collection.uri.clone());
        }
        for uri in uris {
            sqlx::query!(
                r#"INSERT INTO tombstones (id, account_id, uri, by_moderator, created_at, updated_at)
                   SELECT $1, $2, $3::text, true, now(), now()
                   WHERE NOT EXISTS (
                     SELECT 1 FROM tombstones
                     WHERE uri = $3::text AND account_id = $2 AND by_moderator)"#,
                crate::snowflake::next_id(),
                target.id,
                uri,
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    // The discarded posts' `after_update_commit`s.
    for status in &to_remove {
        crate::remove_status::after_discard(state, status.id).await;
    }
    // A local post marked sensitive was saved by `UpdateStatusService`: its
    // `status.updated` webhook.
    for status in &to_update {
        super::webhooks::status_updated(state, status.id).await;
    }

    super::webhooks::trigger(
        state,
        "report.updated",
        super::webhooks::Object::Report(report_id),
    )
    .await;

    // `process_notification!`.
    if send_email_notification && target.is_local() {
        let user = sqlx::query_as!(
            super::account_action::UserRow,
            "SELECT id, email FROM users WHERE account_id = $1",
            target.id,
        )
        .fetch_optional(&state.db)
        .await?;
        super::account_action::notify(
            state,
            &target,
            user.as_ref(),
            warning_id,
            warning_action::to_str(strike_action),
            &text,
            Some((report.category, report.rule_ids.clone().unwrap_or_default())),
        )
        .await;
    }

    // `RemovalWorker` for each post, a local one kept for the strike
    // (`preserve`) and a remote one destroyed (`immediate`).
    let options = crate::remove_status::Options {
        preserve: target.is_local(),
        immediate: !target.is_local(),
        ..Default::default()
    };
    for status in &to_remove {
        crate::remove_status::call(state, status.id, options).await?;
    }
    for status in &to_update {
        let Some(updated) =
            sqlx::query_as!(Status, "SELECT * FROM statuses WHERE id = $1", status.id)
                .fetch_optional(&state.db)
                .await?
        else {
            continue;
        };
        if let Err(error) = crate::api::mastodon::statuses::federate_status_update(
            state, updated.id, &target, &updated,
        )
        .await
        {
            tracing::warn!(status_id = updated.id, %error, "could not federate a sensitive post");
        }
    }
    // A collection a moderator deletes is destroyed without
    // `DeleteCollectionService`, so no one is told of it.
    // `UpdateCollectionService` for one made sensitive: its local members
    // are told (`NotifyOfCollectionUpdateService`), and a local one's
    // `Update` goes to its reach.
    for collection in collections
        .iter()
        .filter(|c| made_sensitive.contains(&c.id))
    {
        crate::api::mastodon::collections::notify_of_collection_update(state, collection.id).await;
        if collection.local {
            crate::api::mastodon::collections::distribute_collection(
                state,
                &state.instance.domain,
                collection.id,
                false,
            )
            .await;
        }
    }
    Ok(())
}
