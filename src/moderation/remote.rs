//! A remote account's own server suspending it, as `ActivityPub::
//! ProcessAccountService#set_suspension!` reads it from the actor's
//! `suspended` property.

use anyhow::Result;

use crate::db::models::Account;
use crate::delete_account::suspension_origin;
use crate::state::AppState;

/// `set_suspension!` and `after_suspension_change!`: follow the actor's
/// `suspended` flag, unless a moderator here suspended the account, which only
/// a moderator here undoes. Returns whether the account is suspended now.
pub async fn set_suspension(state: &AppState, account: &Account, suspended: bool) -> Result<bool> {
    let is_suspended = account.suspended_at.is_some();
    if is_suspended && account.suspension_origin != Some(suspension_origin::REMOTE) {
        return Ok(true);
    }
    if is_suspended && !suspended {
        crate::delete_account::unsuspend(state, account.id).await?;
        let state = state.clone();
        let id = account.id;
        crate::tenants::spawn(async move {
            if let Err(error) = super::suspension::unsuspend(&state, id).await {
                tracing::warn!(account_id = id, %error, "UnsuspendAccountService failed");
            }
        });
        Ok(false)
    } else if !is_suspended && suspended {
        crate::delete_account::suspend(state, account.id, suspension_origin::REMOTE, true).await?;
        let state = state.clone();
        let id = account.id;
        crate::tenants::spawn(async move {
            if let Err(error) = super::suspension::suspend(&state, id).await {
                tracing::warn!(account_id = id, %error, "SuspendAccountService failed");
            }
        });
        Ok(true)
    } else {
        Ok(is_suspended)
    }
}

/// `UnsuspendAccountService#refresh_remote_account!`: fetch the actor again and
/// take its suspension state from what its server says now.
///
/// Boxed, as a `Send` future: it can set off `UnsuspendAccountService`, which
/// calls it, and the compiler cannot see through that cycle.
pub fn refresh<'a>(
    state: &'a AppState,
    account: &'a Account,
) -> futures::future::BoxFuture<'a, Result<()>> {
    Box::pin(refresh_inner(state, account))
}

async fn refresh_inner(state: &AppState, account: &Account) -> Result<()> {
    let Some(uri) = account.stored_uri() else {
        return Ok(());
    };
    let actor = crate::federation::fetch::signed_get_json(state, uri).await?;
    if actor.get("id").and_then(|id| id.as_str()) != Some(uri) {
        return Ok(());
    }
    let suspended = actor
        .get("suspended")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    set_suspension(state, account, suspended).await?;
    Ok(())
}
