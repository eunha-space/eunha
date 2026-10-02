//! Remote account handles.
//!
//! Since Mastodon 4.7.0 an account is identified by its ActivityPub `id`, and
//! its handle is a mutable property of it: an account that renames itself is
//! renamed here too, rather than becoming a second account that has to be
//! merged back later. A handle is only ever taken on webfinger's word, because
//! an actor document can claim any `preferredUsername` and believing it would
//! let one account take another's; `ProcessAccountService`
//! (`crate::federation::process_account`) asks it, and renames.

use crate::error::AppResult;
use crate::state::AppState;

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
