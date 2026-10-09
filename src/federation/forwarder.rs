//! `ActivityPub::Forwarder`: another server's activity about a status that
//! local accounts shared is passed on to their followers, who may have the
//! status only through them. Mastodon forwards a remote status's `Delete` and
//! edit (`Update`) when the activity carries a Linked Data Signature
//! (`forwardable?`), and a quote stamp's `Delete` always.

use serde_json::Value;

use crate::db::models::vis;
use crate::state::AppState;

/// `forwardable?`: the activity is signed in itself, so the followers it is
/// passed on to can tell who wrote it, and the status is public or unlisted.
pub async fn forwardable(state: &AppState, activity: &Value, status_id: i64) -> bool {
    if crate::api::ap::inbox::signed_as_sent(activity).is_none() {
        return false;
    }
    sqlx::query_scalar!("SELECT visibility FROM statuses WHERE id = $1", status_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .is_some_and(|v| matches!(v, vis::PUBLIC | vis::UNLISTED))
}

/// `forward!`: pass `activity`, sent by `sender_id`, on to the followers of
/// the local accounts that boosted or quoted the status `status_id`, and of
/// the local author it replies to; signed by that author, or else by the
/// first of those accounts. The sender's own inbox is left out. Suspended
/// followers are not: `Account.inboxes` does not ask.
pub async fn forward(state: &AppState, sender_id: i64, activity: &Value, status_id: i64) {
    if let Err(error) = try_forward(state, sender_id, activity, status_id).await {
        tracing::warn!(status_id, %error, "could not forward an activity");
    }
}

async fn try_forward(
    state: &AppState,
    sender_id: i64,
    activity: &Value,
    status_id: i64,
) -> anyhow::Result<()> {
    // `shared_by_account_ids`: local boosters, then local quoters.
    let mut shared_by: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT s.account_id FROM statuses s JOIN accounts a ON a.id = s.account_id
           WHERE s.reblog_of_id = $1 AND s.deleted_at IS NULL AND a.domain IS NULL
           ORDER BY s.id"#,
        status_id,
    )
    .fetch_all(&state.db)
    .await?;
    shared_by.extend(
        sqlx::query_scalar!(
            r#"SELECT q.account_id FROM quotes q JOIN accounts a ON a.id = q.account_id
               WHERE q.quoted_status_id = $1 AND a.domain IS NULL
               ORDER BY q.id"#,
            status_id,
        )
        .fetch_all(&state.db)
        .await?,
    );

    // `in_reply_to_local?`: the thread's author is local.
    let replied_to_local: Option<i64> = sqlx::query_scalar!(
        r#"SELECT t.account_id FROM statuses s
           JOIN statuses t ON t.id = s.in_reply_to_id
           JOIN accounts a ON a.id = t.account_id
           WHERE s.id = $1 AND a.domain IS NULL"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?;

    let Some(signer) = replied_to_local.or_else(|| shared_by.first().copied()) else {
        return Ok(());
    };

    let mut targets = shared_by;
    targets.extend(replied_to_local);
    let sender_inbox: Option<String> = sqlx::query_scalar!(
        r#"SELECT CASE WHEN shared_inbox_url <> '' THEN shared_inbox_url ELSE inbox_url END AS "inbox!"
           FROM accounts WHERE id = $1"#,
        sender_id,
    )
    .fetch_optional(&state.db)
    .await?;
    let inboxes: Vec<String> = sqlx::query_scalar!(
        r#"SELECT DISTINCT COALESCE(NULLIF(a.shared_inbox_url, ''), a.inbox_url) AS "inbox!"
           FROM accounts a
           WHERE a.domain IS NOT NULL AND a.protocol = 1
             AND a.id IN (SELECT account_id FROM follows WHERE target_account_id = ANY($1::bigint[]))"#,
        &targets,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .filter(|inbox| !inbox.is_empty() && Some(inbox) != sender_inbox.as_ref())
    .collect();
    if inboxes.is_empty() {
        return Ok(());
    }

    let account = sqlx::query!(
        "SELECT id, username, id_scheme FROM accounts WHERE id = $1",
        signer
    )
    .fetch_one(&state.db)
    .await?;
    let key_id = crate::federation::tag::key_id(
        &state.instance.domain,
        account.id,
        account.id_scheme,
        &account.username,
    );
    // As its sender signed it: without what eunha noted on it on arrival.
    let activity = crate::api::ap::inbox::as_sent(activity);
    crate::federation::delivery::forward_to_inboxes(state, activity, inboxes, key_id).await?;
    Ok(())
}
