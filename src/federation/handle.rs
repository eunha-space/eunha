//! Remote account handles.
//!
//! Since Mastodon 4.7.0 an account is identified by its ActivityPub `id`, and
//! its handle is a mutable property of it: an account that renames itself is
//! renamed here too, rather than becoming a second account that has to be
//! merged back later. A handle is only ever taken on webfinger's word, because
//! an actor document can claim any `preferredUsername` and believing it would
//! let one account take another's.

use crate::error::AppResult;
use crate::state::AppState;

/// Adopt a remote account's new handle, if it can be verified.
///
/// Mastodon 4.7.0 treats an actor's `id` as the account's identity, so an
/// account whose `preferredUsername` changes is renamed in place instead of
/// turning into a second account that later has to be merged. The claimed
/// handle is only taken once webfinger resolves it back to this same actor:
/// anyone can put any `preferredUsername` in their actor document, and
/// believing it outright would let one account take over another's handle.
pub async fn rename_if_handle_changed(
    state: &AppState,
    actor_uri: &str,
    claimed_username: &str,
) -> AppResult<()> {
    if claimed_username.is_empty() {
        return Ok(());
    }

    let Some(account) = sqlx::query!(
        "SELECT id, username, domain FROM accounts WHERE uri = $1 AND domain IS NOT NULL",
        actor_uri,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let Some(domain) = account.domain else {
        return Ok(());
    };
    if account.username.eq_ignore_ascii_case(claimed_username) {
        return Ok(());
    }

    match crate::federation::webfinger::resolve(&state.fetcher, claimed_username, &domain).await {
        Ok(resolved) if resolved == actor_uri => {}
        Ok(resolved) => {
            tracing::warn!(
                actor_uri,
                claimed_username,
                resolved,
                "ignoring a handle change that webfinger maps to a different actor"
            );
            return Ok(());
        }
        Err(e) => {
            tracing::warn!(actor_uri, claimed_username, error = %e, "could not verify a handle change");
            return Ok(());
        }
    }

    // The handle may still be held by another account we know about. Webfinger
    // has just said it belongs to this one, so the other account's handle is
    // the wrong one; upstream invalidates it and retries the rename.
    if rename(state, account.id, claimed_username).await.is_err() {
        invalidate_conflicting_handle(state, account.id, claimed_username, &domain).await?;
        if let Err(e) = rename(state, account.id, claimed_username).await {
            tracing::warn!(
                actor_uri,
                claimed_username,
                error = %e,
                "could not adopt a verified handle change"
            );
            return Ok(());
        }
    }

    tracing::info!(
        actor_uri,
        from = %account.username,
        to = claimed_username,
        "remote account changed handle"
    );
    Ok(())
}

pub(crate) async fn rename(state: &AppState, account_id: i64, username: &str) -> sqlx::Result<()> {
    sqlx::query!(
        r#"UPDATE accounts
           SET username = $2, last_webfingered_at = now(), updated_at = now()
           WHERE id = $1"#,
        account_id,
        username,
    )
    .execute(&state.db)
    .await
    .map(|_| ())
}

/// Take the handle away from whichever other remote account is holding it.
///
/// Mastodon's `Account#invalidate_username!`: the account keeps its actor id
/// and everything hanging off it, but its handle becomes one no server could
/// ever issue, which is what `invalid_handle` reports to clients. A local
/// account is never touched — a remote handle cannot collide with one.
pub async fn invalidate_conflicting_handle(
    state: &AppState,
    account_id: i64,
    username: &str,
    domain: &str,
) -> AppResult<()> {
    let Some(conflicting) = sqlx::query_scalar!(
        r#"SELECT id FROM accounts
           WHERE lower(username) = lower($1)
             AND lower(domain) = lower($2)
             AND domain IS NOT NULL
             AND id <> $3"#,
        username,
        domain,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };

    sqlx::query!(
        r#"UPDATE accounts
           SET username = '! ' || id::text, updated_at = now()
           WHERE id = $1"#,
        conflicting,
    )
    .execute(&state.db)
    .await?;

    tracing::info!(
        account_id = conflicting,
        handle = format!("{username}@{domain}"),
        "handle reassigned to the actor webfinger points at; the old holder's handle is now invalid"
    );
    Ok(())
}
