//! `NotifyService`'s `DropCondition` and `FilterCondition` policy checks: what
//! a recipient's `NotificationPolicy` does with a notification from someone
//! it has not let through.

use sqlx::PgPool;

/// What becomes of a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Deliver,
    /// Kept, with `filtered`, out of the main list.
    Filter,
    /// Never written.
    Drop,
}

/// `NotificationPolicy`'s `{ accept: 0, filter: 1, drop: 2 }`.
const FILTER: i32 = 1;
const DROP: i32 = 2;

/// `NotifyService::BaseCondition::NEW_ACCOUNT_THRESHOLD` and
/// `NEW_FOLLOWER_THRESHOLD`, in days.
const NEW_ACCOUNT_DAYS: i32 = 30;
const NEW_FOLLOWER_DAYS: i32 = 3;

/// `Notification::PROPERTIES[type][:filterable]`.
pub fn filterable(notification_type: &str) -> bool {
    matches!(
        notification_type,
        "mention"
            | "reblog"
            | "follow"
            | "follow_request"
            | "favourite"
            | "quote"
            | "added_to_collection"
    )
}

struct Policy {
    for_not_following: i32,
    for_not_followers: i32,
    for_new_accounts: i32,
    for_private_mentions: i32,
    for_limited_accounts: i32,
    for_bots: i32,
}

/// `NotificationPolicy.find_or_initialize_by(account:)`: the stored policy, or
/// the column defaults (limited accounts and private mentions filtered).
async fn policy(db: &PgPool, account_id: i64) -> Policy {
    sqlx::query_as!(
        Policy,
        r#"SELECT for_not_following, for_not_followers, for_new_accounts,
                  for_private_mentions, for_limited_accounts, for_bots
           FROM notification_policies WHERE account_id = $1"#,
        account_id,
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .unwrap_or(Policy {
        for_not_following: 0,
        for_not_followers: 0,
        for_new_accounts: 0,
        for_private_mentions: FILTER,
        for_limited_accounts: FILTER,
        for_bots: 0,
    })
}

struct Facts {
    not_following: bool,
    not_follower: bool,
    new_account: bool,
    silenced: bool,
    bot: bool,
    override_for_sender: bool,
}

/// `private_mention_not_in_response?`: a direct mention that is not part of a
/// private conversation the recipient started with the sender, looked for up
/// to 100 posts up the thread.
async fn private_mention_not_in_response(
    db: &PgPool,
    recipient: i64,
    sender: i64,
    status_id: i64,
) -> bool {
    let status = sqlx::query!(
        "SELECT visibility, in_reply_to_id FROM statuses WHERE id = $1",
        status_id
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    let Some(status) = status else {
        return false;
    };
    if status.visibility != crate::db::models::vis::DIRECT {
        return false;
    }
    let Some(parent) = status.in_reply_to_id else {
        return true;
    };
    let in_response: i64 = sqlx::query_scalar!(
        r#"WITH RECURSIVE ancestors(id, in_reply_to_id, mention_id, path, depth) AS (
               SELECT s.id, s.in_reply_to_id, m.id, ARRAY[s.id], 0
               FROM statuses s
               LEFT JOIN mentions m ON m.silent = FALSE AND m.account_id = $3 AND m.status_id = s.id
               WHERE s.id = $1
             UNION ALL
               SELECT s.id, s.in_reply_to_id, m.id, ancestors.path || s.id, ancestors.depth + 1
               FROM ancestors
               JOIN statuses s ON s.id = ancestors.in_reply_to_id
               LEFT JOIN mentions m ON m.silent = FALSE AND m.account_id = $3 AND m.status_id = s.id AND s.account_id = $2
               WHERE ancestors.mention_id IS NULL AND NOT s.id = ANY(path) AND ancestors.depth < 100
           )
           SELECT COUNT(*) AS "n!"
           FROM ancestors
           JOIN statuses s ON s.id = ancestors.id
           WHERE ancestors.mention_id IS NOT NULL AND s.account_id = $2 AND s.visibility = 3"#,
        parent,
        recipient,
        sender,
    )
    .fetch_one(db)
    .await
    .unwrap_or(0);
    in_response == 0
}

/// `from_staff?`: a local sender whose role may bypass blocks of the
/// recipient's (`UserRole#bypass_block?`).
pub async fn from_staff(db: &PgPool, recipient: i64, sender: i64) -> bool {
    let Ok(Some(sender_role)) = super::role::of_account(db, sender).await else {
        return false;
    };
    let recipient_role = super::role::of_account(db, recipient).await.ok().flatten();
    sender_role.bypass_block(recipient_role.as_ref())
}

/// The policy part of `NotifyService#drop?` and `#filter?`, for a filterable
/// notification that got past blocks and mutes. `silenced` is the
/// `silenced:` option a caller passes for a sender limited in context.
pub async fn decide(
    db: &PgPool,
    recipient: i64,
    sender: i64,
    notification_type: &str,
    status_id: Option<i64>,
    silenced: bool,
) -> Decision {
    if !filterable(notification_type) {
        return Decision::Deliver;
    }
    let row = sqlx::query!(
        r#"SELECT
             NOT EXISTS (SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2) AS "not_following!",
             NOT EXISTS (SELECT 1 FROM follows WHERE account_id = $2 AND target_account_id = $1
                           AND created_at <= now() - make_interval(days => $3)) AS "not_follower!",
             (a.created_at > now() - make_interval(days => $4)) AS "new_account!",
             (a.silenced_at IS NOT NULL) AS "silenced!",
             (COALESCE(a.actor_type, '') IN ('Application', 'Service')) AS "bot!",
             EXISTS (SELECT 1 FROM notification_permissions
                     WHERE account_id = $1 AND from_account_id = $2) AS "permitted!"
           FROM accounts a WHERE a.id = $2"#,
        recipient,
        sender,
        NEW_FOLLOWER_DAYS,
        NEW_ACCOUNT_DAYS,
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    let Some(row) = row else {
        return Decision::Deliver;
    };
    let facts = Facts {
        not_following: row.not_following,
        not_follower: row.not_follower,
        new_account: row.new_account,
        silenced: silenced || row.silenced,
        bot: row.bot,
        override_for_sender: row.permitted,
    };
    // `return false if override_for_sender?`
    if facts.override_for_sender {
        return Decision::Deliver;
    }
    let message = notification_type == "mention";
    // `return blocked if message? && from_staff?` in `drop?`, and
    // `return false if message? && from_staff?` in `filter?`.
    if message && from_staff(db, recipient, sender).await {
        return Decision::Deliver;
    }

    let policy = policy(db, recipient).await;
    let private_unsolicited = match (message, status_id) {
        (true, Some(id)) if facts.not_following => {
            private_mention_not_in_response(db, recipient, sender, id).await
        }
        _ => false,
    };
    // Each policy and whether it applies to this sender.
    let checks = [
        (
            policy.for_limited_accounts,
            facts.silenced && facts.not_following,
        ),
        (policy.for_not_following, facts.not_following),
        (policy.for_not_followers, facts.not_follower),
        (
            policy.for_new_accounts,
            facts.new_account && facts.not_following,
        ),
        (
            policy.for_private_mentions,
            facts.not_following && private_unsolicited,
        ),
        (policy.for_bots, facts.bot && facts.not_following),
    ];
    if checks
        .iter()
        .any(|(setting, applies)| *setting == DROP && *applies)
    {
        Decision::Drop
    } else if checks
        .iter()
        .any(|(setting, applies)| *setting == FILTER && *applies)
    {
        Decision::Filter
    } else {
        Decision::Deliver
    }
}
