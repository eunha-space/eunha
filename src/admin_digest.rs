//! Staff alert digests: at most one mail an hour to each staff member about
//! new reports, appeals and accounts awaiting review.
//!
//! Mastodon mails every staff member once per report (`AdminMailer#new_report`),
//! per appeal (`#new_appeal`) and per sign-up awaiting approval
//! (`#new_pending_account`). eunha sends the first alert of a quiet hour as
//! Mastodon would, at once, and opens an hour's window for that member. What
//! comes in while the window is open is held, and goes out as one summary when
//! it closes; a summary opens the next window, so a busy stretch makes one
//! mail an hour. An hour with nothing held ends quietly, and the next alert is
//! again mailed at once.
//!
//! Each kind's `notification_emails.*` opt-out is asked before an alert gets
//! here. The window is a `SET NX` key and the held alerts a set beside it, both
//! in the coordination pool, as email subscription batches are. Should Redis
//! fail, the alert is mailed at once: a report mailed twice is better than one
//! never mailed.
//!
//! Mastodon: none

use std::time::Duration;

use crate::state::AppState;

/// How long a window stays open after the mail that opened it.
pub const WINDOW: Duration = Duration::from_secs(60 * 60);

/// How long after a window opens its summary is looked for: a little after it
/// closes, so that everything held while it was open is there to be taken.
pub const FLUSH_AFTER: Duration = Duration::from_secs(61 * 60);

/// How long held alerts are kept if their summary never runs.
const HELD_TTL: u64 = 24 * 60 * 60;

/// One alert for a staff member.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Alert {
    /// `AdminMailer#new_report`. A local reporter is named; a remote one is
    /// known by its server.
    Report {
        report_id: i64,
        reporter: Option<String>,
        reporter_domain: Option<String>,
        target: String,
    },
    /// `AdminMailer#new_appeal`. `action` is the strike's
    /// `AccountWarning#action` key.
    Appeal {
        strike_id: i64,
        target: String,
        action_taken_by: String,
        strike_created_at: chrono::NaiveDateTime,
        action: String,
        text: String,
    },
    /// `AdminMailer#new_pending_account`.
    PendingAccount {
        account_id: i64,
        username: String,
        invite_request: Option<String>,
    },
}

/// An alert as it is held, with when it came in, which also keeps two
/// otherwise equal alerts apart in the set.
#[derive(serde::Serialize, serde::Deserialize)]
struct Held {
    at: chrono::DateTime<chrono::Utc>,
    #[serde(flatten)]
    alert: Alert,
}

fn window_key(state: &AppState, account_id: i64) -> String {
    state
        .redis_keys
        .key(format!("admin_digest:{account_id}:window"))
}

fn held_key(state: &AppState, account_id: i64) -> String {
    state
        .redis_keys
        .key(format!("admin_digest:{account_id}:held"))
}

/// Alert the staff member `account_id`, at `to`: mail it now if no window of
/// theirs is open, and open one; otherwise hold it for the window's summary.
pub async fn alert(state: &AppState, account_id: i64, to: &str, alert: Alert) {
    match hold(state, account_id, &alert).await {
        Ok(true) => {}
        Ok(false) => {
            send_now(state, to, &alert).await;
            crate::jobs::push_in(state, FLUSH_AFTER, AdminDigestJob { account_id }).await;
        }
        Err(error) => {
            tracing::warn!(%error, "could not hold a staff alert; mailing it now");
            send_now(state, to, &alert).await;
        }
    }
}

/// Open a window for `account_id`, or, if one is already open, hold `alert`
/// in it. `true` when it was held.
async fn hold(state: &AppState, account_id: i64, alert: &Alert) -> anyhow::Result<bool> {
    let mut redis = state.redis_coordination.clone();
    let opened: Option<String> = redis::cmd("SET")
        .arg(window_key(state, account_id))
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(WINDOW.as_secs())
        .query_async(&mut redis)
        .await?;
    if opened.is_some() {
        return Ok(false);
    }
    let held = serde_json::to_string(&Held {
        at: chrono::Utc::now(),
        alert: alert.clone(),
    })?;
    let key = held_key(state, account_id);
    let (): () = redis::pipe()
        .cmd("SADD")
        .arg(&key)
        .arg(held)
        .ignore()
        .cmd("EXPIRE")
        .arg(&key)
        .arg(HELD_TTL)
        .ignore()
        .query_async(&mut redis)
        .await?;
    Ok(true)
}

/// The alert as Mastodon mails it.
async fn send_now(state: &AppState, to: &str, alert: &Alert) {
    let mailer = state.mailer();
    let instance = &state.instance;
    let result = match alert {
        Alert::Report {
            report_id,
            reporter,
            reporter_domain,
            target,
        } => {
            mailer
                .send_new_report(
                    to,
                    instance,
                    *report_id,
                    reporter.as_deref(),
                    reporter_domain.as_deref(),
                    target,
                )
                .await
        }
        Alert::Appeal {
            strike_id,
            target,
            action_taken_by,
            strike_created_at,
            action,
            text,
        } => {
            mailer
                .send_new_appeal(
                    to,
                    instance,
                    target,
                    action_taken_by,
                    *strike_created_at,
                    action,
                    text,
                    *strike_id,
                )
                .await
        }
        Alert::PendingAccount {
            account_id,
            username,
            invite_request,
        } => {
            mailer
                .send_new_pending_account(
                    to,
                    instance,
                    *account_id,
                    username,
                    invite_request.as_deref(),
                )
                .await
        }
    };
    if let Err(error) = result {
        tracing::warn!(%error, "could not mail a staff alert");
    }
}

/// The alerts held for `account_id`, oldest first, for tests and diagnosis.
pub async fn held(state: &AppState, account_id: i64) -> Vec<Alert> {
    read_held(state, account_id)
        .await
        .map(|held| held.into_iter().map(|(_, alert)| alert).collect())
        .unwrap_or_default()
}

/// The held alerts with the members they are stored as, oldest first. Members
/// that do not read are dropped from the set.
async fn read_held(state: &AppState, account_id: i64) -> anyhow::Result<Vec<(String, Alert)>> {
    let mut redis = state.redis_coordination.clone();
    let key = held_key(state, account_id);
    let members: Vec<String> = redis::cmd("SMEMBERS")
        .arg(&key)
        .query_async(&mut redis)
        .await?;
    let mut held = Vec::with_capacity(members.len());
    for member in members {
        match serde_json::from_str::<Held>(&member) {
            Ok(entry) => held.push((entry.at, member, entry.alert)),
            Err(_) => {
                let _: redis::RedisResult<()> = redis::cmd("SREM")
                    .arg(&key)
                    .arg(&member)
                    .query_async(&mut redis)
                    .await;
            }
        }
    }
    held.sort_by_key(|(at, _, _)| *at);
    Ok(held
        .into_iter()
        .map(|(_, member, alert)| (member, alert))
        .collect())
}

/// Mail `account_id` the summary of what their window held, and open the next
/// window if there was any. Returns how many alerts it carried.
pub async fn flush(state: &AppState, account_id: i64) -> anyhow::Result<usize> {
    let held = read_held(state, account_id).await?;
    if held.is_empty() {
        return Ok(0);
    }
    let key = held_key(state, account_id);
    let mut redis = state.redis_coordination.clone();
    let to = sqlx::query_scalar!("SELECT email FROM users WHERE account_id = $1", account_id)
        .fetch_optional(&state.db)
        .await?;
    if let Some(to) = to {
        let alerts: Vec<Alert> = held.iter().map(|(_, alert)| alert.clone()).collect();
        state
            .mailer()
            .send_admin_digest(&to, &state.instance, &alerts)
            .await?;
    }
    // Only what was taken: an alert held since stays for the next summary.
    let (): () = redis::cmd("SREM")
        .arg(&key)
        .arg(held.iter().map(|(member, _)| member).collect::<Vec<_>>())
        .query_async(&mut redis)
        .await?;
    // The summary counts as this hour's mail. If an alert has already opened
    // a window since the last one closed, that window has its own summary.
    let opened: Option<String> = redis::cmd("SET")
        .arg(window_key(state, account_id))
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(WINDOW.as_secs())
        .query_async(&mut redis)
        .await?;
    if opened.is_some() {
        crate::jobs::push_in(state, FLUSH_AFTER, AdminDigestJob { account_id }).await;
    }
    Ok(held.len())
}

/// The summary at the end of a staff member's window.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct AdminDigestJob {
    pub account_id: i64,
}

impl crate::jobs::Job for AdminDigestJob {
    const KIND: &'static str = "Eunha::AdminDigestWorker";
    const OPTIONS: crate::jobs::Options =
        crate::jobs::Options::DEFAULT.queue(crate::jobs::Queue::Mailers);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        flush(state, self.account_id).await.map(drop)
    }
}
