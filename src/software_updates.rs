//! Notices about the Mastodon release eunha implements.
//!
//! Mastodon polls an update server for newer releases and for the end of
//! support of the branch it is running, and records both in `software_updates`
//! and `software_deprecations`. Eunha asks the same server the same question,
//! about the Mastodon release *it implements* rather than about itself: eunha
//! builds Mastodon 4.7.0's schema and serves its API, so when that branch stops
//! receiving fixes, the schema and API eunha reproduces are the ones going out
//! of support, and the answer is the admin's to act on.
//!
//! What eunha does not do is claim to be Mastodon. The query carries the
//! Mastodon version being asked about, the request carries eunha's own
//! `User-Agent`, and the notices are recorded as being about that release. An
//! instance that would rather not talk to a third party at all can set
//! `software_update_url` to an empty string, which turns the check off.
//!
//! A process asks once however many instances it serves. The question is about
//! the release this binary implements, so it is the same question for all of
//! them, and a process of fifty instances asking it fifty times was fifty
//! requests for one answer. What stays each instance's own is the recording:
//! its `software_updates` and `software_deprecations` rows, and the mail to
//! whoever it is that should hear.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::Arc;

use crate::state::AppState;

/// How often to ask. Mastodon checks hourly; nothing here changes that fast,
/// and the answer is only read by a human.
const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// `software_deprecations.warning_issued`, from Mastodon's enum.
mod warning {
    pub const NONE: i32 = 0;
    pub const THREE_MONTHS: i32 = 1;
    pub const TWO_WEEKS: i32 = 2;
    pub const OUT_OF_SUPPORT: i32 = 3;
}

/// `software_updates.type`, from Mastodon's enum.
mod update_type {
    pub const PATCH: i32 = 0;
    pub const MINOR: i32 = 1;
    pub const MAJOR: i32 = 2;
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateCheck {
    #[serde(default)]
    updates_available: Vec<AvailableUpdate>,
    #[serde(default)]
    current_version: Option<CurrentVersion>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AvailableUpdate {
    version: String,
    #[serde(default)]
    release_notes: String,
    #[serde(default)]
    urgent: bool,
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    end_of_support: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CurrentVersion {
    #[serde(default)]
    end_of_support: Option<String>,
}

/// Poll for update notices for as long as the process runs, once for all the
/// instances it serves.
///
/// The instances are read afresh every time, so ones that came or went with a
/// reload are asked about, or are no longer.
pub async fn run_for_process(tenants: Arc<crate::tenants::Tenants>) {
    let mut interval = tokio::time::interval(CHECK_INTERVAL);
    loop {
        interval.tick().await;
        for (url, states) in by_url(tenants.states().await) {
            if let Err(e) = check_once_for(&states, &url).await {
                // A check that cannot run is not worth waking anyone for: the
                // instances keep serving, and the next tick tries again.
                tracing::warn!(error = %e, "software update check failed");
            }
        }
    }
}

/// The instances grouped by the update server each was told to ask, leaving out
/// those whose check is turned off. Instances in one process usually name the
/// same server, and then this is one group.
fn by_url(states: Vec<AppState>) -> Vec<(String, Vec<AppState>)> {
    let mut groups: Vec<(String, Vec<AppState>)> = Vec::new();
    for state in states {
        let Some(url) = state
            .config
            .software_update_url
            .clone()
            .filter(|url| !url.is_empty())
        else {
            continue;
        };
        match groups.iter_mut().find(|(asked, _)| *asked == url) {
            Some((_, group)) => group.push(state),
            None => groups.push((url, vec![state])),
        }
    }
    groups
}

/// Ask once, and record the answer in each of these instances' databases.
pub async fn check_once_for(states: &[AppState], url: &str) -> Result<()> {
    let Some(asking) = states.first() else {
        return Ok(());
    };
    let target = crate::version::MASTODON;
    let response = asking
        .fetch
        .get(format!("{url}?version={target}"))
        .header("Accept", "application/json")
        .send()
        .await
        .context("requesting update notices")?
        .error_for_status()
        .context("update server rejected the request")?;

    let check: UpdateCheck = response.json().await.context("parsing update notices")?;

    // One instance's database being unwritable should not keep the others from
    // hearing, so each is tried and the first failure is what comes back.
    let mut failed = None;
    for state in states {
        if let Err(e) = record(state, &check).await {
            tracing::warn!(
                tenant = %state.instance.domain,
                error = %format!("{e:#}"),
                "could not record update notices"
            );
            failed = failed.or(Some(e));
        }
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Ask once for a single instance.
pub async fn check_once(state: &AppState, url: &str) -> Result<()> {
    check_once_for(std::slice::from_ref(state), url).await
}

/// Record what the server answered where this instance's admins read it, and
/// mail whoever should hear about it.
async fn record(state: &AppState, check: &UpdateCheck) -> Result<()> {
    let new_updates = record_updates(state, &check.updates_available).await?;
    if !new_updates.is_empty() {
        notify_of_updates(state, &new_updates).await;
    }
    if let Some(end_of_support) = check
        .current_version
        .as_ref()
        .and_then(|current| current.end_of_support.as_deref())
    {
        record_deprecation(state, end_of_support).await?;
    }
    Ok(())
}

/// Replace the recorded updates with the ones the server still lists, and
/// report the ones that were not already recorded.
///
/// A release that has come and gone — because eunha has since adopted it, or
/// because upstream withdrew it — should not keep being advertised. Only
/// genuinely new ones are worth mailing about; the check runs every six hours
/// and nobody wants that in their inbox.
async fn record_updates<'a>(
    state: &AppState,
    updates: &'a [AvailableUpdate],
) -> Result<Vec<&'a AvailableUpdate>> {
    let versions: Vec<String> = updates.iter().map(|u| u.version.clone()).collect();
    sqlx::query!(
        "DELETE FROM public.software_updates WHERE NOT (version = ANY($1))",
        &versions,
    )
    .execute(&state.db)
    .await?;

    let known: std::collections::HashSet<String> = sqlx::query_scalar!(
        "SELECT version FROM public.software_updates WHERE version = ANY($1)",
        &versions,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    for update in updates {
        let kind = match update.kind.as_str() {
            "major" => update_type::MAJOR,
            "minor" => update_type::MINOR,
            _ => update_type::PATCH,
        };
        let end_of_support = update
            .end_of_support
            .as_deref()
            .and_then(|date| chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok());

        sqlx::query!(
            r#"INSERT INTO public.software_updates
                 (version, urgent, type, release_notes, end_of_support, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, now(), now())
               ON CONFLICT (version) DO UPDATE
                 SET urgent = EXCLUDED.urgent,
                     type = EXCLUDED.type,
                     release_notes = EXCLUDED.release_notes,
                     end_of_support = EXCLUDED.end_of_support,
                     updated_at = now()"#,
            update.version,
            update.urgent,
            kind,
            update.release_notes,
            end_of_support,
        )
        .execute(&state.db)
        .await?;
    }

    Ok(updates
        .iter()
        .filter(|u| !known.contains(&u.version))
        .collect())
}

/// Everyone who should hear about this: Mastodon mails the users whose role
/// carries `view_devops`, and an administrator carries every permission.
async fn devops_recipients(state: &AppState) -> Result<Vec<Recipient>> {
    const ADMINISTRATOR: i64 = 1 << 0;
    const VIEW_DEVOPS: i64 = 1 << 1;

    Ok(sqlx::query_as!(
        Recipient,
        r#"SELECT u.email, a.username, u.settings
           FROM users u
           JOIN accounts a ON a.id = u.account_id
           JOIN user_roles ur ON ur.id = u.role_id
           WHERE u.confirmed_at IS NOT NULL
             AND u.approved
             AND NOT u.disabled
             AND a.suspended_at IS NULL
             AND a.requested_deletion_at IS NULL
             AND (ur.permissions & $1 <> 0 OR ur.permissions & $2 <> 0)"#,
        ADMINISTRATOR,
        VIEW_DEVOPS,
    )
    .fetch_all(&state.db)
    .await?)
}

struct Recipient {
    email: String,
    username: String,
    settings: Option<String>,
}

impl Recipient {
    /// Read one of Mastodon's `notification_emails.*` settings.
    ///
    /// The column is a serialized blob whose format is Rails' business; rather
    /// than parse it, look for the setting's key and the value that follows.
    /// A setting that cannot be read falls back to Mastodon's default, which
    /// errs towards sending rather than towards silence.
    fn notification_setting(&self, key: &str) -> Option<String> {
        let settings = self.settings.as_deref()?;
        let at = settings.find(key)? + key.len();
        let rest = &settings[at..];
        let start = rest.find(|c: char| c.is_ascii_alphanumeric())?;
        let value: String = rest[start..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        (!value.is_empty()).then_some(value)
    }

    /// Mastodon's `should_notify_user?`, defaulting to `critical`.
    fn wants_update_email(&self, urgent: bool, patch: bool) -> bool {
        match self
            .notification_setting("software_updates")
            .as_deref()
            .unwrap_or("critical")
        {
            "none" => false,
            "patch" => urgent || patch,
            "all" => true,
            // "critical", and anything unrecognised.
            _ => urgent,
        }
    }

    /// Mastodon's `should_notify_about_end_of_support?`, defaulting to on.
    fn wants_end_of_support_email(&self) -> bool {
        self.notification_setting("end_of_support").as_deref() != Some("false")
    }
}

/// Mail the administrators about releases that were not already recorded.
async fn notify_of_updates(state: &AppState, updates: &[&AvailableUpdate]) {
    let urgent = updates.iter().any(|u| u.urgent);
    let patch = updates.iter().any(|u| u.kind == "patch");
    let versions: Vec<String> = updates.iter().map(|u| u.version.clone()).collect();

    let recipients = match devops_recipients(state).await {
        Ok(recipients) => recipients,
        Err(e) => {
            tracing::warn!(error = %e, "could not list administrators for update notices");
            return;
        }
    };

    for recipient in recipients {
        if !recipient.wants_update_email(urgent, patch) {
            continue;
        }
        if let Err(e) = state
            .mailer()
            .send_software_updates(
                &recipient.email,
                &recipient.username,
                &state.instance.domain,
                crate::version::MASTODON,
                &versions,
                urgent,
            )
            .await
        {
            tracing::warn!(error = %e, "could not send a software update notice");
        }
    }
}

/// Mail the administrators that support is ending, once per threshold crossed.
async fn notify_of_end_of_support(
    state: &AppState,
    branch: &str,
    end_of_support: chrono::NaiveDate,
) {
    let days_remaining = (end_of_support - chrono::Utc::now().date_naive()).num_days();
    let end_of_support = end_of_support.to_string();

    let recipients = match devops_recipients(state).await {
        Ok(recipients) => recipients,
        Err(e) => {
            tracing::warn!(error = %e, "could not list administrators for end-of-support notices");
            return;
        }
    };

    for recipient in recipients {
        if !recipient.wants_end_of_support_email() {
            continue;
        }
        if let Err(e) = state
            .mailer()
            .send_end_of_support(
                &recipient.email,
                &recipient.username,
                &state.instance.domain,
                branch,
                &end_of_support,
                days_remaining,
            )
            .await
        {
            tracing::warn!(error = %e, "could not send an end-of-support notice");
        }
    }
}

/// Record when the branch eunha implements stops being supported.
///
/// `warning_issued` tracks how loudly this has been said already, so that
/// crossing a threshold is noted once rather than every six hours.
async fn record_deprecation(state: &AppState, end_of_support: &str) -> Result<()> {
    let end_of_support = chrono::NaiveDate::parse_from_str(end_of_support, "%Y-%m-%d")
        .with_context(|| format!("unparseable endOfSupport {end_of_support:?}"))?;
    let branch = tracked_branch();

    // Only the branch eunha implements is of interest; anything else is a
    // leftover from a build that targeted a different release.
    sqlx::query!(
        "DELETE FROM public.software_deprecations WHERE branch <> $1",
        branch,
    )
    .execute(&state.db)
    .await?;

    let warning = warning_for(end_of_support, chrono::Utc::now().date_naive());
    let previous = sqlx::query_scalar!(
        "SELECT warning_issued FROM public.software_deprecations WHERE branch = $1",
        branch,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(warning::NONE);

    sqlx::query!(
        r#"INSERT INTO public.software_deprecations
             (branch, end_of_support, warning_issued, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())
           ON CONFLICT (branch) DO UPDATE
             SET end_of_support = EXCLUDED.end_of_support,
                 -- A warning already given is not withdrawn by a later date.
                 warning_issued = GREATEST(
                     software_deprecations.warning_issued,
                     EXCLUDED.warning_issued
                 ),
                 updated_at = now()"#,
        branch,
        end_of_support,
        warning,
    )
    .execute(&state.db)
    .await?;

    if warning > previous {
        notify_of_end_of_support(state, &branch, end_of_support).await;
        match warning {
            warning::OUT_OF_SUPPORT => tracing::error!(
                branch,
                %end_of_support,
                "Mastodon {branch} is out of support; the schema and API eunha implements \
                 no longer receive fixes"
            ),
            warning::TWO_WEEKS => tracing::warn!(
                branch,
                %end_of_support,
                "Mastodon {branch} loses support within two weeks"
            ),
            warning::THREE_MONTHS => tracing::warn!(
                branch,
                %end_of_support,
                "Mastodon {branch} loses support within three months"
            ),
            _ => {}
        }
    }
    Ok(())
}

/// The `major.minor` branch of the Mastodon release this build implements.
fn tracked_branch() -> String {
    crate::version::MASTODON
        .split('.')
        .take(2)
        .collect::<Vec<_>>()
        .join(".")
}

/// Which warning an end-of-support date has earned, from Mastodon's thresholds.
fn warning_for(end_of_support: chrono::NaiveDate, today: chrono::NaiveDate) -> i32 {
    if end_of_support <= today {
        warning::OUT_OF_SUPPORT
    } else if end_of_support < today + chrono::Duration::weeks(2) {
        warning::TWO_WEEKS
    } else if end_of_support < today + chrono::Duration::days(90) {
        warning::THREE_MONTHS
    } else {
        warning::NONE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(s: &str) -> chrono::NaiveDate {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn warnings_escalate_as_the_date_approaches() {
        let today = date("2026-08-23");
        assert_eq!(warning_for(date("2027-06-01"), today), warning::NONE);
        assert_eq!(
            warning_for(date("2026-10-01"), today),
            warning::THREE_MONTHS
        );
        assert_eq!(warning_for(date("2026-09-01"), today), warning::TWO_WEEKS);
        assert_eq!(
            warning_for(date("2026-08-23"), today),
            warning::OUT_OF_SUPPORT
        );
        assert_eq!(
            warning_for(date("2020-01-01"), today),
            warning::OUT_OF_SUPPORT
        );
    }

    #[test]
    fn the_branch_is_the_tracked_releases_major_minor() {
        let branch = tracked_branch();
        assert_eq!(
            branch.matches('.').count(),
            1,
            "expected major.minor, got {branch}"
        );
        assert!(crate::version::MASTODON.starts_with(&branch));
    }

    fn recipient(settings: Option<&str>) -> Recipient {
        Recipient {
            email: "admin@example.test".into(),
            username: "admin".into(),
            settings: settings.map(str::to_owned),
        }
    }

    /// Mastodon's default is `critical`: urgent releases are mailed about,
    /// ordinary ones are not.
    #[test]
    fn an_unset_preference_means_critical_only() {
        let r = recipient(None);
        assert!(r.wants_update_email(true, false));
        assert!(!r.wants_update_email(false, true));
        assert!(!r.wants_update_email(false, false));
        assert!(r.wants_end_of_support_email());
    }

    #[test]
    fn each_preference_selects_what_mastodon_selects() {
        let none = recipient(Some("notification_emails.software_updates: none"));
        assert!(!none.wants_update_email(true, true));

        let patch = recipient(Some("notification_emails.software_updates: patch"));
        assert!(patch.wants_update_email(false, true));
        assert!(patch.wants_update_email(true, false));
        assert!(!patch.wants_update_email(false, false));

        let all = recipient(Some("notification_emails.software_updates: all"));
        assert!(all.wants_update_email(false, false));

        let critical = recipient(Some("notification_emails.software_updates: critical"));
        assert!(critical.wants_update_email(true, false));
        assert!(!critical.wants_update_email(false, true));
    }

    #[test]
    fn end_of_support_email_can_be_turned_off() {
        assert!(
            !recipient(Some("notification_emails.end_of_support: false"))
                .wants_end_of_support_email()
        );
        assert!(recipient(Some("notification_emails.end_of_support: true"))
            .wants_end_of_support_email());
        // A blob that says nothing about it leaves it on, as Mastodon does.
        assert!(recipient(Some("something_else: 1")).wants_end_of_support_email());
    }

    /// The settings blob is Rails' own format; reading it must not mistake one
    /// setting's value for another's.
    #[test]
    fn reads_the_value_belonging_to_the_key() {
        let r = recipient(Some(
            "---\nnotification_emails.end_of_support: false\n\
             notification_emails.software_updates: all\n",
        ));
        assert_eq!(
            r.notification_setting("software_updates").as_deref(),
            Some("all")
        );
        assert_eq!(
            r.notification_setting("end_of_support").as_deref(),
            Some("false")
        );
        assert_eq!(r.notification_setting("not_present"), None);
        assert!(r.wants_update_email(false, false));
        assert!(!r.wants_end_of_support_email());
    }

    #[test]
    fn parses_what_the_update_server_returns() {
        // A real response from api.joinmastodon.org, shape and all.
        let body = r#"{
            "updatesAvailable": [
                {"version": "4.7.0", "releaseNotes": "https://example/4.7.0", "urgent": false, "type": "minor"},
                {"version": "4.6.6", "releaseNotes": "https://example/4.6.6", "urgent": true, "type": "patch"}
            ],
            "currentVersion": {"endOfSupport": null}
        }"#;
        let check: UpdateCheck = serde_json::from_str(body).unwrap();
        assert_eq!(check.updates_available.len(), 2);
        assert_eq!(check.updates_available[0].version, "4.7.0");
        assert!(check.updates_available[1].urgent);
        assert_eq!(check.updates_available[1].kind, "patch");
        assert!(check.current_version.unwrap().end_of_support.is_none());
    }

    #[test]
    fn tolerates_a_response_with_nothing_in_it() {
        let check: UpdateCheck = serde_json::from_str("{}").unwrap();
        assert!(check.updates_available.is_empty());
        assert!(check.current_version.is_none());
    }
}
