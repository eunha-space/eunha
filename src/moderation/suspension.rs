//! `SuspendAccountService` and `UnsuspendAccountService`: what follows
//! `Account#suspend!` and `#unsuspend!` (in [`crate::delete_account`]) when a
//! moderator acts, run where Mastodon runs them, on `Admin::SuspensionWorker`
//! and `Admin::UnsuspensionWorker`.
//!
//! Neither touches media permissions. Mastodon's
//! `UpdateMediaAttachmentsPermissionsService` only does anything when media is
//! on S3 with `S3_PERMISSION` set or on the filesystem; eunha's bucket serves
//! objects without per-object ACLs, which is the `S3_PERMISSION=''` case it
//! returns early for.

use anyhow::Result;

use crate::db::models::Account;
use crate::state::AppState;

async fn load(state: &AppState, account_id: i64) -> Result<Option<Account>> {
    Ok(
        sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", account_id)
            .fetch_optional(&state.db)
            .await?,
    )
}

/// `Admin::SuspensionWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SuspensionWorker {
    pub account_id: i64,
}

impl crate::jobs::Job for SuspensionWorker {
    const KIND: &'static str = "Admin::SuspensionWorker";
    const OPTIONS: crate::jobs::Options =
        crate::jobs::Options::DEFAULT.queue(crate::jobs::Queue::Pull);

    async fn perform(self, state: &AppState) -> Result<()> {
        suspend(state, self.account_id).await
    }
}

/// `Admin::UnsuspensionWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct UnsuspensionWorker {
    pub account_id: i64,
}

impl crate::jobs::Job for UnsuspensionWorker {
    const KIND: &'static str = "Admin::UnsuspensionWorker";
    const OPTIONS: crate::jobs::Options =
        crate::jobs::Options::DEFAULT.queue(crate::jobs::Queue::Pull);

    async fn perform(self, state: &AppState) -> Result<()> {
        unsuspend(state, self.account_id).await
    }
}

/// `Admin::SuspensionWorker.perform_async(account_id)`.
pub async fn suspend_later(state: &AppState, account_id: i64) {
    crate::jobs::push(state, SuspensionWorker { account_id }).await;
}

/// `Admin::UnsuspensionWorker.perform_async(account_id)`.
pub async fn unsuspend_later(state: &AppState, account_id: i64) {
    crate::jobs::push(state, UnsuspensionWorker { account_id }).await;
}

/// `SuspendAccountService#call`.
pub async fn suspend(state: &AppState, account_id: i64) -> Result<()> {
    let Some(account) = load(state, account_id).await? else {
        return Ok(());
    };
    if account.suspended_at.is_none() {
        return Ok(());
    }

    reject_remote_follows(state, &account).await?;
    distribute_update_actor(state, &account).await?;
    crate::delete_account::suspend_side_effects(state, account.id).await?;
    Ok(())
}

/// `UnsuspendAccountService#call`.
pub async fn unsuspend(state: &AppState, account_id: i64) -> Result<()> {
    let Some(mut account) = load(state, account_id).await? else {
        return Ok(());
    };

    // `refresh_remote_account!`: it may have been suspended, or deleted, at
    // its origin while it was suspended here.
    if !account.is_local() {
        if let Err(error) = super::remote::refresh(state, &account).await {
            tracing::warn!(account_id, %error, "could not refresh an unsuspended account");
        }
        match load(state, account_id).await? {
            Some(refreshed) => account = refreshed,
            None => return Ok(()),
        }
    }
    if account.suspended_at.is_some() {
        return Ok(());
    }

    merge_into_home_timelines(state, account.id).await?;
    merge_into_list_timelines(state, account.id).await?;
    distribute_update_actor(state, &account).await?;
    Ok(())
}

/// `reject_remote_follows!`. A remote account suspended here is not suspended
/// at its origin, so it would go on receiving what the local accounts it
/// follows post. It is made to unfollow them, which cannot be undone.
async fn reject_remote_follows(state: &AppState, account: &Account) -> Result<()> {
    if account.is_local()
        || !account.is_activitypub()
        || account.suspension_origin == Some(crate::delete_account::suspension_origin::REMOTE)
    {
        return Ok(());
    }
    let targets = crate::delete_account::reject_follows_by(state, account).await?;
    for target in targets {
        sqlx::query!(
            "DELETE FROM follows WHERE account_id = $1 AND target_account_id = $2",
            account.id,
            target,
        )
        .execute(&state.db)
        .await?;
        // `AccountStat`'s `update_index('accounts', :account)`.
        crate::search::elasticsearch::indexing::accounts(state, &[account.id, target]).await;
        crate::counters::on_follow_removed(state, account.id, target).await?;
    }
    Ok(())
}

/// `distribute_update_actor!`: tell the servers that know a local account that
/// it is suspended, or no longer is. The actor it carries says which.
async fn distribute_update_actor(state: &AppState, account: &Account) -> Result<()> {
    if !account.is_local() {
        return Ok(());
    }
    crate::accounts::distribute_profile(state, &state.instance.domain, account, None).await?;
    Ok(())
}

/// `merge_into_home_timelines!`: `FeedManager#merge_into_home` for each
/// follower who signed in recently.
async fn merge_into_home_timelines(state: &AppState, account_id: i64) -> Result<()> {
    let mut redis = state.redis.clone();
    for follower in crate::feed::followers_for_local_distribution(&state.db, account_id).await {
        crate::feed::merge_into_home(
            &mut redis,
            &state.redis_keys,
            &state.db,
            account_id,
            follower,
        )
        .await;
    }
    Ok(())
}

/// `merge_into_list_timelines!`: `FeedManager#merge_into_list` for each list
/// of `lists_for_local_distribution`.
async fn merge_into_list_timelines(state: &AppState, account_id: i64) -> Result<()> {
    let mut redis = state.redis.clone();
    for list_id in crate::feed::list_ids_for_local_distribution(&state.db, account_id).await {
        crate::feed::merge_into_list(
            &mut redis,
            &state.redis_keys,
            &state.db,
            account_id,
            list_id,
        )
        .await;
    }
    Ok(())
}
