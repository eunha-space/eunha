//! Mastodon's `Scheduler::AutoCloseRegistrationsScheduler`: once an hour, an
//! instance whose registrations are open but where no moderator has been
//! active for a week switches them to requiring approval, so that an
//! abandoned server is not left for anyone to sign up on unwatched, and tells
//! whoever can manage the settings (`AdminMailer#auto_close_registrations`).
//!
//! The instance configuration's
//! `disable_automatic_switching_to_approved_registrations` turns it off, as
//! `DISABLE_AUTOMATIC_SWITCHING_TO_APPROVED_REGISTRATIONS=true` does.

use crate::moderation::role::flag;
use crate::settings::RegistrationsMode;
use crate::state::AppState;

/// `OPEN_REGISTRATIONS_MODERATOR_THRESHOLD`: a week, and the day
/// `UserTrackingConcern::SIGN_IN_UPDATE_FREQUENCY` lets a sign-in time lag.
const MODERATOR_THRESHOLD_HOURS: i64 = 7 * 24 + 24;

/// `perform`: whether registrations were switched to approval.
pub async fn check(state: &AppState) -> anyhow::Result<bool> {
    if state
        .instance
        .disable_automatic_switching_to_approved_registrations
    {
        return Ok(false);
    }
    if crate::settings::registrations_mode(state).await != RegistrationsMode::Open {
        return Ok(false);
    }
    if active_moderators(state).await? {
        return Ok(false);
    }
    switch_to_approval_mode(state).await?;
    Ok(true)
}

/// `active_moderators?`: someone whose role can manage reports signed in
/// within the threshold, by `current_sign_in_at`, which every authenticated
/// request moves once a day (`UserTrackingConcern`).
async fn active_moderators(state: &AppState) -> anyhow::Result<bool> {
    let moderators = crate::push::accounts_who_can(state, &[flag::MANAGE_REPORTS]).await?;
    if moderators.is_empty() {
        return Ok(false);
    }
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM users u
             WHERE u.account_id = ANY($1)
               AND u.current_sign_in_at >= now() - make_interval(hours => $2)
           ) AS "e!""#,
        &moderators,
        MODERATOR_THRESHOLD_HOURS as i32,
    )
    .fetch_one(&state.db)
    .await?)
}

/// `switch_to_approval_mode!`: `Setting.registrations_mode = 'approved'`,
/// and a mail to each user who can manage the settings.
async fn switch_to_approval_mode(state: &AppState) -> anyhow::Result<()> {
    crate::settings::set(state, "registrations_mode", "approved".into()).await?;
    tracing::warn!("no moderator active for a week; registrations now require approval");
    let admins = crate::push::accounts_who_can(state, &[flag::MANAGE_SETTINGS]).await?;
    let recipients: Vec<String> = sqlx::query_scalar!(
        "SELECT email FROM users WHERE account_id = ANY($1)",
        &admins,
    )
    .fetch_all(&state.db)
    .await?;
    for to in recipients {
        let email = state.mailer();
        let domain = state.instance.domain.clone();
        {
            if let Err(error) = email.send_auto_close_registrations(&to, &domain).await {
                tracing::warn!(%error, "could not mail that registrations were closed");
            }
        }
    }
    Ok(())
}
