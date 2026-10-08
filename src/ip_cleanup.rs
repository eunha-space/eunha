//! Mastodon's `Scheduler::IpCleanupScheduler`: once a day, what is kept of
//! the addresses people used is forgotten after a year.
//!
//!  -  web sessions not used for a year are signed out, their access tokens
//!     and web push subscriptions with them;
//!  -  the address of a session, the sign-up address of a user, and the last
//!     address an access token was used from are cleared a year on;
//!  -  sign-in attempts older than a year are deleted;
//!  -  IP blocks past their expiry are deleted.
//!
//! Mastodon reads both periods from `IP_RETENTION_PERIOD` and
//! `SESSION_RETENTION_PERIOD`, a year unless set; eunha keeps the default.

use std::time::Duration;

use crate::state::AppState;

/// The scheduler runs daily.
pub const EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// `IP_RETENTION_PERIOD`'s default, `1.year`, in seconds (365.2425 days).
pub const IP_RETENTION_SECS: i64 = 31_556_952;
/// `SESSION_RETENTION_PERIOD`'s default, also `1.year`.
pub const SESSION_RETENTION_SECS: i64 = 31_556_952;

/// How many rows one batch handles, as `in_batches` does.
const BATCH: i64 = 1000;

/// Run the cleanup daily for as long as the instance runs. The first pass
/// waits its day, as a newly started Sidekiq scheduler does.
pub async fn run(state: AppState) {
    loop {
        crate::background::rest(&state.stop, EVERY).await;
        if state.stop.is_cancelled() {
            break;
        }
        if let Err(error) = perform(&state).await {
            tracing::error!(%error, "IP cleanup failed");
        }
    }
}

/// `IpCleanupScheduler#perform`: `clean_ip_columns!`, then
/// `clean_expired_ip_blocks!`.
pub async fn perform(state: &AppState) -> anyhow::Result<()> {
    clean_ip_columns(state).await?;
    clean_expired_ip_blocks(&state.db).await?;
    Ok(())
}

async fn clean_ip_columns(state: &AppState) -> anyhow::Result<()> {
    // `SessionActivation.where(updated_at: ...SESSION_RETENTION_PERIOD.ago)
    // .in_batches.destroy_all`: each destroyed with its token, whose streams
    // close.
    loop {
        let ids: Vec<i64> = sqlx::query_scalar!(
            r#"SELECT id FROM session_activations
               WHERE updated_at < now() AT TIME ZONE 'UTC' - make_interval(secs => $1)
               ORDER BY id LIMIT $2"#,
            SESSION_RETENTION_SECS as f64,
            BATCH,
        )
        .fetch_all(&state.db)
        .await?;
        if ids.is_empty() {
            break;
        }
        let tokens = crate::sessions::destroy_where_ids(&state.db, &ids).await?;
        crate::sessions::kill_streams(state, tokens).await;
    }
    let ip = IP_RETENTION_SECS as f64;
    // Each `in_batches.update_all`, without callbacks or `updated_at`.
    let updates = [
        r#"UPDATE session_activations SET ip = NULL WHERE id IN (
             SELECT id FROM session_activations
             WHERE updated_at < now() AT TIME ZONE 'UTC' - make_interval(secs => $1)
               AND ip IS NOT NULL
             ORDER BY id LIMIT $2)"#,
        r#"UPDATE users SET sign_up_ip = NULL WHERE id IN (
             SELECT id FROM users
             WHERE current_sign_in_at < now() AT TIME ZONE 'UTC' - make_interval(secs => $1)
               AND sign_up_ip IS NOT NULL
             ORDER BY id LIMIT $2)"#,
        r#"DELETE FROM login_activities WHERE id IN (
             SELECT id FROM login_activities
             WHERE created_at < now() AT TIME ZONE 'UTC' - make_interval(secs => $1)
             ORDER BY id LIMIT $2)"#,
        r#"UPDATE oauth_access_tokens SET last_used_ip = NULL WHERE id IN (
             SELECT id FROM oauth_access_tokens
             WHERE last_used_at < now() AT TIME ZONE 'UTC' - make_interval(secs => $1)
               AND last_used_ip IS NOT NULL
             ORDER BY id LIMIT $2)"#,
    ];
    for sql in updates {
        loop {
            let n = sqlx::query(sql)
                .bind(ip)
                .bind(BATCH)
                .execute(&state.db)
                .await?
                .rows_affected();
            if n == 0 {
                break;
            }
        }
    }
    Ok(())
}

/// `IpBlock.expired.in_batches.destroy_all`. Its `reset_cache` needs nothing
/// here: eunha's `no_access` cache reads only unexpired blocks, and lapses
/// within seconds.
async fn clean_expired_ip_blocks(db: &sqlx::PgPool) -> anyhow::Result<()> {
    sqlx::query!(
        r#"DELETE FROM ip_blocks
           WHERE expires_at IS NOT NULL AND expires_at < now() AT TIME ZONE 'UTC'"#
    )
    .execute(db)
    .await?;
    Ok(())
}
