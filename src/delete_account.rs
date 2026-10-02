//! Account deletion — a port of Mastodon's `DeleteAccountService`
//! (`app/services/delete_account_service.rb`) together with the `Account#suspend!`
//! / `Account#unsuspend!` model methods it builds on.
//!
//! Suspend or remove an account and remove as much of its data as possible. If
//! it is a local account that has not been confirmed or never been approved,
//! side effects are skipped and both the user and account records are removed
//! fully. Otherwise, behaviour is controlled by [`Options`]:
//!
//! | caller                       | reserve_username | reserve_email |
//! |------------------------------|------------------|---------------|
//! | self-service delete          | yes              | no            |
//! | admin delete / 30-day purge  | yes              | yes           |
//! | admin reject (pending user)  | no               | no            |
//! | inbound `Delete(actor)`      | no               | (remote)      |
//!
//! Two deliberate departures from Mastodon, both noted at their call sites:
//!
//! * Invite lineage is snapshotted into `eunha.invite_lineage` before the user
//!   row (which carries the email and the rest of the PII) is destroyed, so
//!   eunha's invite tree survives a deletion that removes the personal data.
//! * Counter caches on *other* accounts' statuses (replies/reblogs/quotes) are
//!   decremented. Mastodon's batched removal path skips its callbacks and
//!   leaves them stale; eunha keeps them accurate, as its single-status delete
//!   already does.

use anyhow::Result;

use crate::db::models::Account;
use crate::state::AppState;

/// Mastodon's `AccountDeletionRequest::DELAY_TO_DELETION`: how long a suspended
/// account is kept before the scheduler purges it for good.
pub const DELAY_TO_DELETION: chrono::TimeDelta = chrono::TimeDelta::days(30);

/// `accounts.suspension_origin` enum (`Account::SUSPENSION_ORIGINS`).
pub mod suspension_origin {
    pub const LOCAL: i32 = 0;
    pub const REMOTE: i32 = 1;
}

/// How much of the account to keep. Mirrors `DeleteAccountService#call`'s
/// options hash; [`Options::default`] matches its
/// `{ reserve_username: true, reserve_email: true }`.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Options {
    /// Keep the `accounts` record (scrubbed and suspended), reserving the username.
    pub reserve_username: bool,
    /// Keep the `users` record. Only applicable for local accounts.
    pub reserve_email: bool,
    /// Skip ActivityPub payloads *and* streaming/feed updates.
    pub skip_side_effects: bool,
    /// Skip sending ActivityPub payloads. Implied by `skip_side_effects`.
    pub skip_activitypub: bool,
    /// `:suspended_at`: keep the account record suspended from this time,
    /// rather than marking it deleted. Only with `reserve_username`.
    pub suspended_at: Option<chrono::NaiveDateTime>,
    /// `:relationship_severance_event`: record the follows the deletion
    /// severs under this `relationship_severance_events` row.
    pub relationship_severance_event: Option<i64>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            reserve_username: true,
            reserve_email: true,
            skip_side_effects: false,
            skip_activitypub: false,
            suspended_at: None,
            relationship_severance_event: None,
        }
    }
}

impl Options {
    /// Mastodon's self-service deletion (`Settings::DeletesController` →
    /// `AccountDeletionWorker`): the username stays reserved, the user record
    /// (and with it the email) is destroyed.
    pub fn self_service() -> Self {
        Self {
            reserve_email: false,
            ..Self::default()
        }
    }

    /// A full purge: neither the account nor the user record survives.
    pub fn purge() -> Self {
        Self {
            reserve_username: false,
            reserve_email: false,
            ..Self::default()
        }
    }
}

// ── Account#suspend! / #unsuspend! ────────────────────────────────────────

/// Port of `Account#suspend!`. Records a deletion request (which is what makes
/// the suspension reversible, and what the 30-day scheduler later acts on),
/// marks the account suspended, and optionally blocks the user's email from
/// being reused. What follows a moderator's suspension, `SuspendAccountService`,
/// is [`crate::moderation::suspension::suspend`].
///
/// Nothing is deleted here: a suspended account's statuses stay in the database
/// and are hidden by the read paths (`StatusPolicy#show?`, and the
/// `suspended_at IS NULL` filters on the timelines), so [`unsuspend`] brings
/// everything back. Data is only removed when a deletion request comes due or
/// [`call`] is invoked directly.
pub async fn suspend(
    state: &AppState,
    account_id: i64,
    origin: i32,
    block_email: bool,
) -> Result<()> {
    // `Account`'s `update_index('accounts', :self)`: queued now, read once
    // the suspension is committed.
    crate::search::elasticsearch::indexing::account(state, account_id).await;
    let mut tx = state.db.begin().await?;
    create_deletion_request(&mut tx, account_id).await?;
    let local = sqlx::query_scalar!(
        r#"UPDATE accounts SET suspended_at = now(), suspension_origin = $2, updated_at = now()
           WHERE id = $1 RETURNING (domain IS NULL) AS "local!""#,
        account_id,
        origin,
    )
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    if block_email {
        // `Account#create_canonical_email_block!` — local accounts only, and a
        // duplicate is not an error.
        sqlx::query!(
            r#"INSERT INTO canonical_email_blocks (canonical_email_hash, reference_account_id, created_at, updated_at)
               SELECT encode(sha256(lower(btrim(u.email))::bytea), 'hex'), a.id, now(), now()
               FROM accounts a JOIN users u ON u.account_id = a.id
               WHERE a.id = $1 AND a.domain IS NULL AND u.email <> ''
               ON CONFLICT (canonical_email_hash) DO NOTHING"#,
            account_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    // Terminate the account's streaming connections (Mastodon publishes a
    // `kill` event on `timeline:system:{id}` for a local account).
    if local {
        state.streaming.kill_account(account_id).await;
    }
    Ok(())
}

/// Port of `Account#mark_deleted!`, which since 4.7.0 is how an account asks to
/// be deleted: a deletion request, `requested_deletion_at`, and its streaming
/// connections closed.
pub async fn mark_deleted(state: &AppState, account_id: i64) -> Result<()> {
    let mut tx = state.db.begin().await?;
    create_deletion_request(&mut tx, account_id).await?;
    let local = sqlx::query_scalar!(
        r#"UPDATE accounts SET requested_deletion_at = now(), updated_at = now()
           WHERE id = $1 RETURNING (domain IS NULL) AS "local!""#,
        account_id,
    )
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or(false);
    tx.commit().await?;
    if local {
        state.streaming.kill_account(account_id).await;
    }
    Ok(())
}

/// `create_deletion_request!`. There is no unique index on account_id, so
/// guard against a second request for an account that already has one.
async fn create_deletion_request(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    account_id: i64,
) -> Result<()> {
    sqlx::query!(
        r#"INSERT INTO account_deletion_requests (account_id, created_at, updated_at)
           SELECT $1, now(), now()
           WHERE NOT EXISTS (SELECT 1 FROM account_deletion_requests WHERE account_id = $1)"#,
        account_id,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The cache work of `SuspendAccountService` (`unmerge_from_home_timelines!`,
/// `unmerge_from_list_timelines!`, `remove_from_trends!`). The account's content
/// is *not* deleted — it is hidden for as long as the suspension lasts (see
/// `StatusPolicy#show?`) — so the work here is clearing it out of the caches
/// that would otherwise keep serving it.
pub async fn suspend_side_effects(state: &AppState, account_id: i64) -> Result<()> {
    // `unmerge_from_home_timelines!`
    let followers: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT f.account_id FROM follows f
           JOIN accounts a ON a.id = f.account_id
           WHERE f.target_account_id = $1 AND a.domain IS NULL"#,
        account_id,
    )
    .fetch_all(&state.db)
    .await?;
    let mut redis = state.redis.clone();
    for follower in followers {
        crate::feed::unmerge_from_home(
            &mut redis,
            &state.redis_keys,
            &state.db,
            account_id,
            follower,
        )
        .await;
    }

    // `unmerge_from_list_timelines!`. Dropping the cached feed is enough: it is
    // repopulated from the database on the next read, which skips suspended
    // authors.
    let lists: Vec<i64> = sqlx::query_scalar!(
        "SELECT list_id FROM list_accounts WHERE account_id = $1",
        account_id,
    )
    .fetch_all(&state.db)
    .await?;
    for list_id in lists {
        crate::feed::delete_list_feed(&mut redis, &state.redis_keys, list_id).await;
    }

    // `remove_from_trends!`
    sqlx::query!(
        "DELETE FROM status_trends WHERE account_id = $1",
        account_id,
    )
    .execute(&state.db)
    .await?;

    Ok(())
}

/// Port of `Account#unsuspend!`.
pub async fn unsuspend(state: &AppState, account_id: i64) -> Result<()> {
    crate::search::elasticsearch::indexing::account(state, account_id).await;
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "DELETE FROM account_deletion_requests WHERE account_id = $1",
        account_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE accounts SET suspended_at = NULL, suspension_origin = NULL, updated_at = now() WHERE id = $1",
        account_id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM canonical_email_blocks WHERE reference_account_id = $1",
        account_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

// ── DeleteAccountService#call ─────────────────────────────────────────────

/// Port of `DeleteAccountService#call`. Deleting an account that no longer
/// exists is a no-op (Mastodon's workers swallow `RecordNotFound`).
/// `AccountDeletionWorker.perform_async(account_id, options)`: the purge, from
/// the job queue, since an account can own a lot of content.
pub async fn call_later(state: &AppState, account_id: i64, options: Options) {
    crate::jobs::push(
        state,
        AccountDeletionWorker {
            account_id,
            options,
        },
    )
    .await;
}

/// `AccountDeletionWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct AccountDeletionWorker {
    pub account_id: i64,
    pub options: Options,
}

impl crate::jobs::Job for AccountDeletionWorker {
    const KIND: &'static str = "AccountDeletionWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Pull)
        .lock(crate::jobs::Lock::UntilExecuted(
            std::time::Duration::from_secs(7 * 24 * 3600),
        ));

    async fn perform(self, state: &AppState) -> Result<()> {
        call(state, self.account_id, self.options).await
    }
}

/// `Admin::AccountDeletionWorker`: a suspended account's data, deleted by a
/// moderator.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct AdminAccountDeletionWorker {
    pub account_id: i64,
}

impl crate::jobs::Job for AdminAccountDeletionWorker {
    const KIND: &'static str = "Admin::AccountDeletionWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Pull)
        .lock(crate::jobs::Lock::UntilExecuted(
            std::time::Duration::from_secs(7 * 24 * 3600),
        ));

    async fn perform(self, state: &AppState) -> Result<()> {
        call(state, self.account_id, Options::default()).await
    }
}

pub async fn call(state: &AppState, account_id: i64, options: Options) -> Result<()> {
    let Some(account) =
        sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", account_id,)
            .fetch_optional(&state.db)
            .await?
    else {
        return Ok(());
    };

    let mut options = options;
    if account.is_local() && user_unconfirmed_or_pending(state, account_id).await? {
        options.reserve_email = false;
        options.reserve_username = false;
        options.skip_side_effects = true;
    }
    if options.skip_side_effects {
        options.skip_activitypub = true;
    }

    tracing::info!(
        account_id,
        acct = %account.acct(),
        reserve_username = options.reserve_username,
        reserve_email = options.reserve_email,
        "deleting account",
    );

    if let Some(event_id) = options.relationship_severance_event {
        crate::moderation::severance::record_follows_of(&state.db, event_id, account.id).await?;
    }
    distribute_activities(state, &account, &options).await;
    purge_content(state, &account, &options).await?;
    fulfill_deletion_request(state, account_id).await?;
    Ok(())
}

/// `User#unconfirmed_or_pending?` — never confirmed, or awaiting approval.
/// An account with no user row at all is neither (it is a bare local actor).
async fn user_unconfirmed_or_pending(state: &AppState, account_id: i64) -> Result<bool> {
    let row = sqlx::query!(
        "SELECT confirmed_at, approved FROM users WHERE account_id = $1",
        account_id,
    )
    .fetch_optional(&state.db)
    .await?;
    Ok(row.is_some_and(|u| u.confirmed_at.is_none() || !u.approved))
}

// ── distribute_activities! ────────────────────────────────────────────────

async fn distribute_activities(state: &AppState, account: &Account, options: &Options) {
    if options.skip_activitypub {
        return;
    }
    let result = if account.is_local() {
        delete_actor(state, account).await
    } else {
        sever_remote_follows(state, account).await
    };
    if let Err(e) = result {
        tracing::warn!(account_id = account.id, error = %e, "failed to distribute account deletion activities");
    }
}

/// `delete_actor!` — announce the deletion to every inbox we know of. Mastodon
/// splits this into a normal-priority push to followers and relays plus a
/// low-priority push to everyone else; eunha's delivery queue has a single
/// priority, so both sets are enqueued together.
///
/// Only reached for local accounts, which always keep their `accounts` row
/// here: the queue loads the signing key from `accounts` at send time, so a
/// `reserve_username: false` local delete would leave the jobs unsignable. That
/// combination only arises for unconfirmed/pending users, where Mastodon (and
/// this port) skip side effects entirely.
async fn delete_actor(state: &AppState, account: &Account) -> Result<()> {
    if !crate::federation::keypair::has_signing_key(state, account.id)
        .await
        .unwrap_or(false)
    {
        return Ok(());
    }

    let domain = &state.instance.domain;
    let actor_url = crate::federation::tag::account_uri_of(domain, account);
    let key_id = crate::federation::tag::key_id_of(domain, account);
    let activity = crate::federation::activity::delete_actor(&actor_url);

    let inboxes = delete_actor_inboxes(state).await?;
    // `always_sign`: the actor's Linked Data Signature, whatever the mode.
    let enqueued = crate::federation::delivery::deliver_to_inboxes_signed(
        state,
        activity,
        inboxes,
        key_id,
        crate::federation::delivery::LinkedData::Always,
    )
    .await?;
    tracing::debug!(account_id = account.id, enqueued, "enqueued Delete(actor)");
    Ok(())
}

/// `delivery_inboxes + low_priority_delivery_inboxes`: every known remote inbox
/// (`Account.inboxes`) plus the enabled relays.
async fn delete_actor_inboxes(state: &AppState) -> Result<Vec<String>> {
    let rows: Vec<String> = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT inbox AS "inbox!" FROM (
            SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END AS inbox
            FROM accounts a
            WHERE a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.inbox_url <> ''
            UNION
            SELECT inbox_url FROM relays WHERE state = 2 AND inbox_url <> ''
        ) reach
        WHERE inbox <> ''
        "#,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows)
}

/// `RejectFollowSerializer` for each follow a remote account has of local
/// accounts, signed by the account followed and sent to the follower's inbox.
/// Returns the ids of the local accounts it was following.
pub(crate) async fn reject_follows_by(state: &AppState, account: &Account) -> Result<Vec<i64>> {
    let Some(remote_uri) = account.stored_uri().map(str::to_owned) else {
        return Ok(vec![]);
    };
    if account.inbox_url.is_empty() {
        return Ok(vec![]);
    }
    let domain = &state.instance.domain;
    let mut targets = vec![];
    // Follows *by* the remote account: Reject them, signed by the local target.
    let outgoing = sqlx::query!(
        r#"SELECT f.id, f.uri, a.id AS target_id, a.username, a.id_scheme
           FROM follows f JOIN accounts a ON a.id = f.target_account_id
           WHERE f.account_id = $1 AND a.domain IS NULL"#,
        account.id,
    )
    .fetch_all(&state.db)
    .await?;
    for follow in outgoing {
        targets.push(follow.target_id);
        let target_url = crate::federation::tag::account_uri(
            domain,
            follow.target_id,
            follow.id_scheme,
            &follow.username,
        );
        let key_id = format!("{target_url}#main-key");
        let follow_uri = follow
            .uri
            .unwrap_or_else(|| format!("{remote_uri}#follows/{}", follow.id));
        let activity = crate::federation::activity::reject_follow(
            &format!("{target_url}#rejects/follows/{}", follow.id),
            &target_url,
            &follow_uri,
            &remote_uri,
            &target_url,
        )?;
        crate::federation::delivery::deliver_to_inboxes(
            state,
            activity,
            vec![account.inbox_url.clone()],
            key_id,
        )
        .await?;
    }

    Ok(targets)
}

/// `reject_follows!` + `undo_follows!` — a deleted remote account keeps working
/// on its own server, so force the follow relationships apart in both
/// directions rather than leaving it able to receive our posts.
async fn sever_remote_follows(state: &AppState, account: &Account) -> Result<()> {
    let Some(remote_uri) = account.stored_uri().map(str::to_owned) else {
        return Ok(());
    };
    if account.inbox_url.is_empty() {
        return Ok(());
    }
    reject_follows_by(state, account).await?;
    let domain = &state.instance.domain;

    // Follows *of* the remote account: Undo them, signed by the local follower.
    let incoming = sqlx::query!(
        r#"SELECT f.id, f.uri, a.id AS follower_id, a.username, a.id_scheme
           FROM follows f JOIN accounts a ON a.id = f.account_id
           WHERE f.target_account_id = $1 AND a.domain IS NULL"#,
        account.id,
    )
    .fetch_all(&state.db)
    .await?;
    for follow in incoming {
        let follower_url = crate::federation::tag::account_uri(
            domain,
            follow.follower_id,
            follow.id_scheme,
            &follow.username,
        );
        let key_id = format!("{follower_url}#main-key");
        let follow_uri = follow
            .uri
            .unwrap_or_else(|| format!("{follower_url}#follows/{}", follow.id));
        let activity = crate::federation::activity::undo_follow(
            &format!("{follow_uri}#undo"),
            &follower_url,
            &follow_uri,
            &follower_url,
            &remote_uri,
        )?;
        crate::federation::delivery::deliver_to_inboxes(
            state,
            activity,
            vec![account.inbox_url.clone()],
            key_id,
        )
        .await?;
    }
    Ok(())
}

// ── purge_content! ────────────────────────────────────────────────────────

async fn purge_content(state: &AppState, account: &Account, options: &Options) -> Result<()> {
    let reported = reported_status_ids(state, account.id).await?;

    purge_user(state, account, options).await?;
    purge_profile(state, account, options).await?;
    purge_statuses(state, account, options, &reported).await?;
    purge_mentions(state, account.id, &reported).await?;
    purge_media_attachments(state, account, options, &reported).await?;
    purge_polls(state, account.id, &reported).await?;
    purge_generated_notifications(state, account.id).await?;
    purge_favourites(state, account.id).await?;
    purge_bookmarks(state, account.id).await?;
    purge_feeds(state, account, options).await?;
    purge_associations(state, account.id, options).await?;

    if !options.reserve_username {
        // Everything else hangs off `accounts` with an ON DELETE CASCADE (or
        // SET NULL) foreign key, so the row itself is the last thing to go.
        delete_avatar_and_header(state, account).await;
        crate::fasp::events::account_deleted(state, account.id).await;
        sqlx::query!("DELETE FROM accounts WHERE id = $1", account.id)
            .execute(&state.db)
            .await?;
    }
    Ok(())
}

/// `reported_status_ids` — statuses attached to an unresolved report about this
/// account are kept so moderators can still act on them.
async fn reported_status_ids(state: &AppState, account_id: i64) -> Result<Vec<i64>> {
    let ids: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT DISTINCT unnest(status_ids) AS "id!"
           FROM reports
           WHERE target_account_id = $1 AND action_taken_at IS NULL"#,
        account_id,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(ids)
}

/// `purge_user!`. Keeping the record disables the user and destroys its unused
/// invites; otherwise the row goes, taking the email, sessions and OAuth tokens
/// (all `ON DELETE CASCADE`) with it.
async fn purge_user(state: &AppState, account: &Account, options: &Options) -> Result<()> {
    if !account.is_local() {
        return Ok(());
    }

    if options.reserve_email {
        sqlx::query!(
            "UPDATE users SET disabled = true, updated_at = now() WHERE account_id = $1",
            account.id,
        )
        .execute(&state.db)
        .await?;
        sqlx::query!(
            r#"DELETE FROM invites
               WHERE uses = 0 AND user_id IN (SELECT id FROM users WHERE account_id = $1)"#,
            account.id,
        )
        .execute(&state.db)
        .await?;
    } else {
        snapshot_invite_lineage(state, account.id).await?;
        sqlx::query!("DELETE FROM users WHERE account_id = $1", account.id)
            .execute(&state.db)
            .await?;
    }

    // A disabled user's tokens are already rejected by the authentication
    // middleware, but revoke them so nothing can keep streaming on an open
    // connection. (Destroying the user cascades them away instead.)
    sqlx::query!(
        r#"UPDATE oauth_access_tokens t SET revoked_at = now()
           FROM users u
           WHERE u.id = t.resource_owner_id AND u.account_id = $1 AND t.revoked_at IS NULL"#,
        account.id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// eunha-specific: preserve "who invited whom" before the `users` row (and with
/// it this account's `invites`, and its invitees' `users.invite_id`) is
/// destroyed. Only account ids are copied — no email, no other user data — into
/// `eunha.invite_lineage`, which is what the invite tree falls back to.
async fn snapshot_invite_lineage(state: &AppState, account_id: i64) -> Result<()> {
    sqlx::query!(
        r#"INSERT INTO eunha.invite_lineage (account_id, inviter_account_id, invited_at)
           -- the accounts this one invited
           SELECT invitee.account_id, $1, invitee.created_at
           FROM users invitee
           JOIN invites i ON i.id = invitee.invite_id
           JOIN users inviter ON inviter.id = i.user_id
           WHERE inviter.account_id = $1
           UNION
           -- and who invited this one
           SELECT u.account_id, inviter.account_id, u.created_at
           FROM users u
           JOIN invites i ON i.id = u.invite_id
           JOIN users inviter ON inviter.id = i.user_id
           WHERE u.account_id = $1
           ON CONFLICT (account_id) DO UPDATE
             SET inviter_account_id = COALESCE(EXCLUDED.inviter_account_id, eunha.invite_lineage.inviter_account_id),
                 invited_at = COALESCE(EXCLUDED.invited_at, eunha.invite_lineage.invited_at)"#,
        account_id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `purge_profile!` — there is no point scrubbing an account record that is
/// about to be deleted.
///
/// Since Mastodon 4.7.0 this records `requested_deletion_at` rather than
/// suspending the account: a deletion the owner asked for and a suspension a
/// moderator imposed are now distinct states. An account that was suspended
/// first keeps its `suspended_at` — the two can hold at once. Both hide the
/// account, through the `suspended_at IS NULL AND requested_deletion_at IS
/// NULL` pairs that stand in for Mastodon's `without_suspended` scope.
async fn purge_profile(state: &AppState, account: &Account, options: &Options) -> Result<()> {
    if !options.reserve_username {
        return Ok(());
    }

    delete_avatar_and_header(state, account).await;
    let was_discoverable = account.discoverable.unwrap_or(false);
    sqlx::query!(
        r#"UPDATE accounts SET
             silenced_at = NULL,
             suspended_at = COALESCE($2, suspended_at),
             suspension_origin = CASE WHEN $2::timestamp IS NULL THEN suspension_origin ELSE 0 END,
             requested_deletion_at = CASE WHEN $2::timestamp IS NULL
                                          THEN COALESCE(requested_deletion_at, now())
                                          ELSE requested_deletion_at END,
             locked = false,
             memorial = false,
             discoverable = false,
             trendable = false,
             display_name = '',
             note = '',
             fields = '[]'::jsonb,
             moved_to_account_id = NULL,
             reviewed_at = NULL,
             requested_review_at = NULL,
             also_known_as = '{}',
             avatar_file_name = NULL,
             avatar_content_type = NULL,
             avatar_file_size = NULL,
             avatar_updated_at = NULL,
             avatar_remote_url = NULL,
             avatar_description = '',
             header_file_name = NULL,
             header_content_type = NULL,
             header_file_size = NULL,
             header_updated_at = NULL,
             header_remote_url = '',
             header_description = '',
             updated_at = now()
           WHERE id = $1"#,
        account.id,
        options.suspended_at,
    )
    .execute(&state.db)
    .await?;
    // Turning `discoverable` off is news to providers.
    crate::fasp::events::account_updated(state, account.id, was_discoverable).await;

    // statuses_count / followers_count / following_count live in account_stats.
    sqlx::query!(
        r#"UPDATE account_stats
           SET statuses_count = 0, followers_count = 0, following_count = 0, updated_at = now()
           WHERE account_id = $1"#,
        account.id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

async fn delete_avatar_and_header(state: &AppState, account: &Account) {
    for (file_name, kind) in [
        (account.avatar_file_name.as_deref(), "avatars"),
        (account.header_file_name.as_deref(), "headers"),
    ] {
        if let Some(name) = file_name.filter(|n| !n.is_empty()) {
            let key = format!(
                "accounts/{kind}/{}/original/{name}",
                crate::media::int_to_path(account.id),
            );
            let _ = state.storage.delete(&key).await;
        }
    }
}

/// `purge_statuses!` → `BatchedRemoveStatusService`. Statuses (and everyone
/// else's reblogs of them) are removed outright rather than tombstoned: the
/// point of deletion is that the content is gone.
async fn purge_statuses(
    state: &AppState,
    account: &Account,
    options: &Options,
    reported: &[i64],
) -> Result<()> {
    const BATCH: i64 = 200;

    loop {
        let ids: Vec<i64> = sqlx::query_scalar!(
            r#"SELECT id FROM statuses
               WHERE account_id = $1 AND NOT (id = ANY($2::bigint[]))
               ORDER BY id LIMIT $3"#,
            account.id,
            reported,
            BATCH,
        )
        .fetch_all(&state.db)
        .await?;
        if ids.is_empty() {
            break;
        }

        // Other accounts' reblogs of these statuses go too (`status.reblogs`).
        let reblogs = sqlx::query!(
            r#"SELECT id, account_id FROM statuses WHERE reblog_of_id = ANY($1::bigint[])"#,
            &ids,
        )
        .fetch_all(&state.db)
        .await?;

        if !options.skip_side_effects {
            for &id in &ids {
                remove_status_side_effects(state, account.id, id).await;
            }
            for r in &reblogs {
                remove_status_side_effects(state, r.account_id, r.id).await;
            }
        }

        // Mastodon's `delete_all` skips the counter-cache callbacks, leaving
        // interaction counts on other people's statuses stale. Keep them
        // accurate instead, mirroring eunha's single-status delete.
        decrement_interaction_counters(state, &ids).await?;

        // Rebloggers lose a status each.
        let reblogger_ids: Vec<i64> = reblogs.iter().map(|r| r.account_id).collect();
        if !reblogger_ids.is_empty() {
            sqlx::query!(
                r#"UPDATE account_stats s
                   SET statuses_count = GREATEST(0, s.statuses_count - c.n), updated_at = now()
                   FROM (SELECT account_id, count(*) AS n
                         FROM unnest($1::bigint[]) AS account_id GROUP BY account_id) c
                   WHERE s.account_id = c.account_id"#,
                &reblogger_ids,
            )
            .execute(&state.db)
            .await?;
        }

        let reblog_ids: Vec<i64> = reblogs.iter().map(|r| r.id).collect();
        sqlx::query!(
            "DELETE FROM statuses WHERE id = ANY($1::bigint[]) OR id = ANY($2::bigint[])",
            &ids,
            &reblog_ids,
        )
        .execute(&state.db)
        .await?;
        // `BatchedRemoveStatusService`: the callbacks were skipped, so the
        // statuses are de-indexed by hand.
        let removed: Vec<i64> = ids.iter().chain(&reblog_ids).copied().collect();
        crate::search::elasticsearch::indexing::enqueue(
            state,
            crate::search::elasticsearch::Index::Statuses,
            &removed,
        )
        .await;
        crate::search::elasticsearch::indexing::enqueue(
            state,
            crate::search::elasticsearch::Index::PublicStatuses,
            &removed,
        )
        .await;
    }

    // Recompute the featured-tag counters that pointed at the removed statuses.
    sqlx::query!(
        r#"UPDATE featured_tags ft SET
             statuses_count = (
               SELECT COUNT(*) FROM statuses_tags st JOIN statuses s ON s.id = st.status_id
               WHERE st.tag_id = ft.tag_id AND s.account_id = $1 AND s.deleted_at IS NULL),
             last_status_at = (
               SELECT MAX(s.created_at) FROM statuses_tags st JOIN statuses s ON s.id = st.status_id
               WHERE st.tag_id = ft.tag_id AND s.account_id = $1 AND s.deleted_at IS NULL)
           WHERE ft.account_id = $1"#,
        account.id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// Streaming delete + timeline unpush for one removed status
/// (`unpush_from_home_timelines` / `unpush_from_list_timelines` /
/// `unpush_from_public_timelines`).
async fn remove_status_side_effects(state: &AppState, author_id: i64, status_id: i64) {
    crate::streaming::fan_out::remove_batched(state, status_id).await;
    let mut redis = state.redis.clone();
    crate::feed::fanout_remove_status(
        &mut redis,
        &state.redis_keys,
        &state.db,
        author_id,
        status_id,
    )
    .await;
    crate::feed::fanout_remove_from_lists(
        &mut redis,
        &state.redis_keys,
        &state.db,
        author_id,
        status_id,
    )
    .await;
}

/// Give back the replies / reblogs / quotes these statuses took from other
/// people's counters before they disappear.
async fn decrement_interaction_counters(state: &AppState, ids: &[i64]) -> Result<()> {
    sqlx::query!(
        r#"UPDATE status_stats ss
           SET replies_count = GREATEST(0, ss.replies_count - c.n), updated_at = now()
           FROM (SELECT in_reply_to_id AS status_id, count(*) AS n FROM statuses
                 WHERE id = ANY($1::bigint[]) AND in_reply_to_id IS NOT NULL
                 GROUP BY in_reply_to_id) c
           WHERE ss.status_id = c.status_id"#,
        ids,
    )
    .execute(&state.db)
    .await?;
    sqlx::query!(
        r#"UPDATE status_stats ss
           SET reblogs_count = GREATEST(0, ss.reblogs_count - c.n), updated_at = now()
           FROM (SELECT reblog_of_id AS status_id, count(*) AS n FROM statuses
                 WHERE id = ANY($1::bigint[]) AND reblog_of_id IS NOT NULL
                 GROUP BY reblog_of_id) c
           WHERE ss.status_id = c.status_id"#,
        ids,
    )
    .execute(&state.db)
    .await?;
    sqlx::query!(
        r#"UPDATE status_stats ss
           SET quotes_count = GREATEST(0, ss.quotes_count - c.n), updated_at = now()
           FROM (SELECT quoted_status_id AS status_id, count(*) AS n FROM quotes
                 WHERE status_id = ANY($1::bigint[]) AND state = 1 AND quoted_status_id IS NOT NULL
                 GROUP BY quoted_status_id) c
           WHERE ss.status_id = c.status_id"#,
        ids,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `purge_mentions!` — mentions *of* this account in statuses that survive.
async fn purge_mentions(state: &AppState, account_id: i64, reported: &[i64]) -> Result<()> {
    sqlx::query!(
        "DELETE FROM mentions WHERE account_id = $1 AND NOT (status_id = ANY($2::bigint[]))",
        account_id,
        reported,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `purge_media_attachments!` — attachments on statuses kept for moderation are
/// kept too, as long as the account record survives to own them.
async fn purge_media_attachments(
    state: &AppState,
    account: &Account,
    options: &Options,
    reported: &[i64],
) -> Result<()> {
    let keep_reported = options.reserve_username;
    // `status_id` is nullable here (an upload not yet attached to a status), so
    // guard the comparison: `NULL = ANY(…)` is NULL, which would otherwise drop
    // every unattached attachment out of the purge.
    let rows = sqlx::query!(
        r#"SELECT id, file_file_name FROM media_attachments
           WHERE account_id = $1
             AND NOT COALESCE($2::bool AND status_id = ANY($3::bigint[]), false)"#,
        account.id,
        keep_reported,
        reported,
    )
    .fetch_all(&state.db)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }

    for row in &rows {
        if let Some(name) = row.file_file_name.as_deref().filter(|n| !n.is_empty()) {
            let key = format!(
                "media_attachments/files/{}/original/{name}",
                crate::media::int_to_path(row.id),
            );
            let _ = state.storage.delete(&key).await;
        }
    }
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    sqlx::query!(
        "DELETE FROM media_attachments WHERE id = ANY($1::bigint[])",
        &ids,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `purge_polls!`
async fn purge_polls(state: &AppState, account_id: i64, reported: &[i64]) -> Result<()> {
    sqlx::query!(
        "DELETE FROM polls WHERE account_id = $1 AND NOT (status_id = ANY($2::bigint[]))",
        account_id,
        reported,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `purge_generated_notifications!` — deleting statuses and polls without
/// callbacks leaves behind the notifications this account generated.
async fn purge_generated_notifications(state: &AppState, account_id: i64) -> Result<()> {
    sqlx::query!(
        "DELETE FROM notifications WHERE from_account_id = $1",
        account_id,
    )
    .execute(&state.db)
    .await?;
    sqlx::query!(
        "DELETE FROM notification_requests WHERE from_account_id = $1",
        account_id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `purge_favourites!` — hand back the favourite counts first.
async fn purge_favourites(state: &AppState, account_id: i64) -> Result<()> {
    sqlx::query!(
        r#"UPDATE status_stats ss
           SET favourites_count = GREATEST(0, ss.favourites_count - c.n), updated_at = now()
           FROM (SELECT status_id, count(*) AS n FROM favourites
                 WHERE account_id = $1 GROUP BY status_id) c
           WHERE ss.status_id = c.status_id"#,
        account_id,
    )
    .execute(&state.db)
    .await?;
    let unfavourited: Vec<i64> =
        sqlx::query_scalar("DELETE FROM favourites WHERE account_id = $1 RETURNING status_id")
            .bind(account_id)
            .fetch_all(&state.db)
            .await?;
    crate::search::elasticsearch::indexing::enqueue(
        state,
        crate::search::elasticsearch::Index::Statuses,
        &unfavourited,
    )
    .await;
    Ok(())
}

/// `purge_bookmarks!`
async fn purge_bookmarks(state: &AppState, account_id: i64) -> Result<()> {
    let unbookmarked: Vec<i64> =
        sqlx::query_scalar("DELETE FROM bookmarks WHERE account_id = $1 RETURNING status_id")
            .bind(account_id)
            .fetch_all(&state.db)
            .await?;
    crate::search::elasticsearch::indexing::enqueue(
        state,
        crate::search::elasticsearch::Index::Statuses,
        &unbookmarked,
    )
    .await;
    Ok(())
}

/// `purge_feeds!` — `FeedManager#clean_feeds!` for the home feed and every list
/// the account owns.
async fn purge_feeds(state: &AppState, account: &Account, options: &Options) -> Result<()> {
    if !account.is_local() || options.skip_side_effects {
        return Ok(());
    }
    let list_ids: Vec<i64> =
        sqlx::query_scalar!("SELECT id FROM lists WHERE account_id = $1", account.id,)
            .fetch_all(&state.db)
            .await?;

    let mut redis = state.redis.clone();
    crate::feed::delete_home_feed(&mut redis, &state.redis_keys, account.id).await;
    for list_id in list_ids {
        crate::feed::delete_list_feed(&mut redis, &state.redis_keys, list_id).await;
    }
    Ok(())
}

/// `purge_other_associations!` — `ASSOCIATIONS_ON_SUSPEND`, plus
/// `ASSOCIATIONS_ON_DESTROY` when the account record is not being kept.
async fn purge_associations(state: &AppState, account_id: i64, options: &Options) -> Result<()> {
    // Follows carry counter-cache callbacks: settle the other side's stats
    // before the rows go.
    sqlx::query!(
        r#"UPDATE account_stats s
           SET followers_count = GREATEST(0, s.followers_count - c.n), updated_at = now()
           FROM (SELECT target_account_id AS account_id, count(*) AS n FROM follows
                 WHERE account_id = $1 GROUP BY target_account_id) c
           WHERE s.account_id = c.account_id"#,
        account_id,
    )
    .execute(&state.db)
    .await?;
    sqlx::query!(
        r#"UPDATE account_stats s
           SET following_count = GREATEST(0, s.following_count - c.n), updated_at = now()
           FROM (SELECT account_id, count(*) AS n FROM follows
                 WHERE target_account_id = $1 GROUP BY account_id) c
           WHERE s.account_id = c.account_id"#,
        account_id,
    )
    .execute(&state.db)
    .await?;

    // Home feeds of the accounts that followed this one keep its statuses until
    // the next repopulate; drop them now (`FeedManager#unmerge_from_home`).
    if !options.skip_side_effects {
        let followers: Vec<i64> = sqlx::query_scalar!(
            r#"SELECT f.account_id FROM follows f
               JOIN accounts a ON a.id = f.account_id
               WHERE f.target_account_id = $1 AND a.domain IS NULL"#,
            account_id,
        )
        .fetch_all(&state.db)
        .await?;
        let mut redis = state.redis.clone();
        for follower in followers {
            crate::feed::unmerge_from_home(
                &mut redis,
                &state.redis_keys,
                &state.db,
                account_id,
                follower,
            )
            .await;
        }
    }

    // ASSOCIATIONS_ON_SUSPEND
    let statements: &[&str] = &[
        "DELETE FROM account_notes WHERE account_id = $1",
        "DELETE FROM account_pins WHERE account_id = $1",
        "DELETE FROM follows WHERE account_id = $1", // active_relationships
        "DELETE FROM account_aliases WHERE account_id = $1", // aliases
        "DELETE FROM blocks WHERE account_id = $1",  // block_relationships
        "DELETE FROM blocks WHERE target_account_id = $1", // blocked_by_relationships
        "DELETE FROM conversation_mutes WHERE account_id = $1",
        "DELETE FROM account_conversations WHERE account_id = $1",
        "DELETE FROM custom_filters WHERE account_id = $1",
        "DELETE FROM account_domain_blocks WHERE account_id = $1",
        "DELETE FROM featured_tags WHERE account_id = $1",
        "DELETE FROM follow_requests WHERE account_id = $1",
        "DELETE FROM list_accounts WHERE account_id = $1",
        "DELETE FROM account_migrations WHERE account_id = $1",
        "DELETE FROM mutes WHERE account_id = $1", // mute_relationships
        "DELETE FROM mutes WHERE target_account_id = $1", // muted_by_relationships
        "DELETE FROM notifications WHERE account_id = $1",
        "DELETE FROM lists WHERE account_id = $1", // owned_lists
        "DELETE FROM follows WHERE target_account_id = $1", // passive_relationships
        "DELETE FROM report_notes WHERE account_id = $1",
        "DELETE FROM scheduled_statuses WHERE account_id = $1",
        "DELETE FROM status_pins WHERE account_id = $1",
    ];
    // ASSOCIATIONS_ON_DESTROY
    let on_destroy: &[&str] = &[
        "DELETE FROM reports WHERE account_id = $1",
        "DELETE FROM account_moderation_notes WHERE target_account_id = $1",
        "DELETE FROM reports WHERE target_account_id = $1",
        "DELETE FROM severed_relationships WHERE local_account_id = $1",
        "DELETE FROM severed_relationships WHERE remote_account_id = $1",
    ];

    let also_on_destroy = if options.reserve_username {
        &on_destroy[..0]
    } else {
        on_destroy
    };
    for sql in statements.iter().chain(also_on_destroy) {
        sqlx::query(sql).bind(account_id).execute(&state.db).await?;
    }
    Ok(())
}

/// `fulfill_deletion_request!` — the account has now been dealt with, so the
/// suspension is no longer reversible.
async fn fulfill_deletion_request(state: &AppState, account_id: i64) -> Result<()> {
    sqlx::query!(
        "DELETE FROM account_deletion_requests WHERE account_id = $1",
        account_id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}
