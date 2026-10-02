//! Notification emails: what `NotifyService#send_email!` hands to
//! `NotificationMailer` when a member is told something while away, and the
//! one-click links in those mails that turn the type off again
//! (`UnsubscriptionsController` for a user).
//!
//! A mail goes out when the notification was delivered rather than filtered,
//! its type is one `NotificationMailer` writes, the member has
//! `notification_emails.<type>` on, and either nothing of theirs is
//! listening (no streaming connection to their own stream, no web push
//! subscription) or they asked for every mail (`always_send_emails`). It is
//! sent two minutes later from the job queue, as `deliver_later(wait:
//! 2.minutes)` does, and only if the notification, its post and the member's
//! standing still allow it then.

use std::time::Duration;

use crate::db::models::Account;
use crate::state::AppState;

/// `NotifyService::NON_EMAIL_TYPES`.
pub const NON_EMAIL_TYPES: &[&str] = &[
    "admin.report",
    "admin.sign_up",
    "update",
    "quoted_update",
    "poll",
    "status",
    "moderation_warning",
    "severed_relationships",
    "annual_report",
    "added_to_collection",
    "collection_update",
];

/// The types `NotificationMailer` has an action for, with
/// `UserSettings`' default for each `notification_emails.<type>`.
pub const MAILED_TYPES: &[(&str, bool)] = &[
    ("follow", true),
    ("reblog", false),
    ("favourite", false),
    ("mention", true),
    ("quote", true),
    ("follow_request", true),
];

/// The types a link in a mail can unsubscribe from:
/// `UnsubscriptionsController#email_type_from_param`, and `quote`, whose
/// mails Mastodon sends with a link it then refuses.
pub const UNSUBSCRIBABLE_TYPES: &[&str] = &[
    "follow",
    "reblog",
    "favourite",
    "mention",
    "follow_request",
    "quote",
];

/// `deliver_later(wait: 2.minutes)`.
pub const DELAY: Duration = Duration::from_secs(120);

/// `NotificationMailer.with(recipient:, notification:).public_send(type)
/// .deliver_later(wait: 2.minutes)`: the mail, rendered when the job runs.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct NotificationMailJob {
    pub notification_id: i64,
}

impl crate::jobs::Job for NotificationMailJob {
    const KIND: &'static str = "ActionMailer::MailDeliveryJob(NotificationMailer)";
    const OPTIONS: crate::jobs::Options =
        crate::jobs::Options::DEFAULT.queue(crate::jobs::Queue::Mailers);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        deliver(state, self.notification_id).await
    }
}

/// `notification_emails.<kind>` for a member, with its default.
pub fn wants(settings: Option<&str>, kind: &str) -> bool {
    let default = MAILED_TYPES
        .iter()
        .find(|(k, _)| *k == kind)
        .is_some_and(|(_, default)| *default);
    crate::accounts::user_setting_bool(settings, &format!("notification_emails.{kind}"), default)
}

/// `NotifyService#send_email! if email_needed?`, for a notification just
/// created and delivered (not filtered) to `recipient_id`.
pub async fn notification_delivered(
    state: &AppState,
    notification_id: i64,
    recipient_id: i64,
    kind: &'static str,
) {
    if NON_EMAIL_TYPES.contains(&kind) || !MAILED_TYPES.iter().any(|(k, _)| *k == kind) {
        return;
    }
    let Ok(Some(user)) = sqlx::query!(
        r#"SELECT id, settings,
                  EXISTS (SELECT 1 FROM web_push_subscriptions w WHERE w.user_id = users.id)
                    AS "push!"
           FROM users WHERE account_id = $1"#,
        recipient_id,
    )
    .fetch_optional(&state.db)
    .await
    else {
        return;
    };
    let settings = user.settings.as_deref();
    // `send_email_for_notification_type?`
    if !wants(settings, kind) {
        return;
    }
    // `(!recipient_online? || always_send_emails?)`
    let online = state.streaming.is_online(recipient_id) || user.push;
    if online && !crate::accounts::user_setting_bool(settings, "always_send_emails", false) {
        return;
    }
    crate::jobs::push_in(state, DELAY, NotificationMailJob { notification_id }).await;
}

/// `NotificationMailer.with(recipient:, notification:).public_send(type)`,
/// when it is performed: nothing if the notification or its post is gone
/// (`rescue_from ActiveRecord::RecordNotFound`) or the member is no longer
/// functional (`verify_functional_user`).
pub async fn deliver(state: &AppState, notification_id: i64) -> anyhow::Result<()> {
    let Some(n) = sqlx::query!(
        r#"SELECT account_id, from_account_id, "type" AS "kind?", activity_type, activity_id
           FROM notifications WHERE id = $1"#,
        notification_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let Some(kind) = n
        .kind
        .as_deref()
        .and_then(|k| MAILED_TYPES.iter().find(|(t, _)| *t == k))
        .map(|(t, _)| *t)
    else {
        return Ok(());
    };
    // `User#functional?`: confirmed, approved, not disabled, the account
    // neither suspended, deleted, a memorial nor moved, and no second factor
    // missing that the role asks for.
    let Some(user) = sqlx::query!(
        r#"SELECT u.email, u.locale, u.time_zone,
                  (u.confirmed_at IS NOT NULL AND u.approved AND NOT u.disabled
                   AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                   AND NOT a.memorial AND a.moved_to_account_id IS NULL
                   AND NOT (COALESCE(r.require_2fa, false) AND NOT u.otp_required_for_login
                            AND NOT EXISTS (SELECT 1 FROM webauthn_credentials w
                                            WHERE w.user_id = u.id))
                  ) AS "functional!"
           FROM users u
           JOIN accounts a ON a.id = u.account_id
           LEFT JOIN user_roles r ON r.id = COALESCE(u.role_id, -99)
           WHERE u.account_id = $1"#,
        n.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    if !user.functional {
        return Ok(());
    }
    let Some(me) = load_account(state, n.account_id).await? else {
        return Ok(());
    };
    let Some(from) = load_account(state, n.from_account_id).await? else {
        return Ok(());
    };
    let locale = user.locale.clone().unwrap_or_else(|| "en".into());
    let domain = &state.instance.domain;

    let mut status = None;
    let mut account = None;
    let mut conversation_message_id = None;
    let (actor_acct, button_url) = match kind {
        "mention" | "quote" | "favourite" | "reblog" => {
            // `set_status`: the notification's `target_status`.
            let Some(target) = target_status(state, &n.activity_type, n.activity_id, kind).await?
            else {
                return Ok(());
            };
            let Some(author) = load_account(state, target.account_id).await? else {
                return Ok(());
            };
            if let Some(conversation_id) = target.conversation_id {
                if let Some(created_at) = sqlx::query_scalar!(
                    "SELECT created_at FROM conversations WHERE id = $1",
                    conversation_id,
                )
                .fetch_optional(&state.db)
                .await?
                {
                    conversation_message_id = Some(format!(
                        "<conversation-{conversation_id}.{}@{domain}>",
                        created_at.date()
                    ));
                }
            }
            let url = format!("https://{domain}/@{}/{}", author.acct(), target.id);
            status = Some(
                crate::email_subscriptions::mailed_status(
                    state,
                    &target,
                    &author,
                    user.time_zone.as_deref(),
                    &locale,
                )
                .await,
            );
            // The subject names whoever posted for a mention or a quote,
            // and whoever acted for a favourite or a boost.
            let named = if matches!(kind, "mention" | "quote") {
                author.acct()
            } else {
                from.acct()
            };
            (named, url)
        }
        _ => {
            account = Some(crate::email::MailedAccount {
                name: crate::email_subscriptions::display_name(&from),
                acct: from.acct(),
                avatar_url: crate::api::mastodon::convert::account_avatar_url_for(
                    &state.urls,
                    &from,
                ),
            });
            let url = if kind == "follow" {
                format!("https://{domain}/@{}", from.acct())
            } else {
                format!("https://{domain}/follow_requests")
            };
            (from.acct(), url)
        }
    };

    let mail = crate::email::NotificationMail {
        to: format!("{} <{}>", me.username, user.email),
        locale,
        kind,
        actor_acct,
        status,
        account,
        button_url,
        preferences_url: format!("https://{domain}/settings"),
        unsubscribe_url: unsubscribe_url(state, n.account_id, kind).await,
        list_id: format!("<{kind}.{}.{domain}>", me.username),
        conversation_message_id,
    };
    state.email.send_notification_mail(&mail).await
}

async fn load_account(state: &AppState, id: i64) -> anyhow::Result<Option<Account>> {
    Ok(
        sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", id)
            .fetch_optional(&state.db)
            .await?,
    )
}

/// `Notification#target_status`: the post a notification is about, from
/// whichever activity it points at — the status itself as eunha records it,
/// or Mastodon's `Mention`, `Favourite` and `Quote` — and for a boost, the
/// post boosted.
async fn target_status(
    state: &AppState,
    activity_type: &str,
    activity_id: i64,
    kind: &str,
) -> anyhow::Result<Option<crate::db::models::Status>> {
    let status_id = match activity_type {
        "Status" => Some(activity_id),
        "Mention" => {
            sqlx::query_scalar!("SELECT status_id FROM mentions WHERE id = $1", activity_id)
                .fetch_optional(&state.db)
                .await?
        }
        "Favourite" => {
            sqlx::query_scalar!(
                "SELECT status_id FROM favourites WHERE id = $1",
                activity_id
            )
            .fetch_optional(&state.db)
            .await?
        }
        "Quote" => {
            sqlx::query_scalar!("SELECT status_id FROM quotes WHERE id = $1", activity_id)
                .fetch_optional(&state.db)
                .await?
        }
        _ => None,
    };
    let Some(status_id) = status_id else {
        return Ok(None);
    };
    let status = sqlx::query_as!(
        crate::db::models::Status,
        "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        status_id,
    )
    .fetch_optional(&state.db)
    .await?;
    // A `reblog` notification Mastodon wrote points at the boost; eunha's
    // points at the post boosted.
    match status {
        Some(s) if kind == "reblog" && s.reblog_of_id.is_some() => Ok(sqlx::query_as!(
            crate::db::models::Status,
            "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
            s.reblog_of_id,
        )
        .fetch_optional(&state.db)
        .await?),
        other => Ok(other),
    }
}

/// What a link to unsubscribe from `kind` carries for a user: the user, signed
/// for `unsubscribe`. With `secret_key_base` that is Mastodon's
/// `to_sgid(for: 'unsubscribe')`, good for a month; without it, `User/<id>`
/// signed with a key derived from the VAPID key, which does not expire.
fn user_token(state: &AppState, user_id: i64) -> String {
    match &state.instance.secret_key_base {
        Some(secret) => secret.signed_global_id("User", user_id, "unsubscribe", chrono::Utc::now()),
        None => crate::crypto::sign_message(
            &state.instance.vapid_private_key,
            b"unsubscribe",
            &format!("User/{user_id}"),
        ),
    }
}

/// The user a [`user_token`] names, or `None` for any other token. A token
/// signed with the VAPID key is read with or without `secret_key_base`, so the
/// links eunha mailed before it was configured keep working.
pub fn user_from_token(state: &AppState, token: &str) -> Option<i64> {
    if let Some(secret) = &state.instance.secret_key_base {
        if let Some((model, id)) = secret.locate_signed(token, "unsubscribe", chrono::Utc::now()) {
            return (model == "User").then_some(id);
        }
    }
    crate::crypto::verify_message(&state.instance.vapid_private_key, b"unsubscribe", token)?
        .strip_prefix("User/")?
        .parse()
        .ok()
}

/// `unsubscribe_url(token: @user.to_sgid(for: 'unsubscribe'), type:)`.
pub async fn unsubscribe_url(state: &AppState, account_id: i64, kind: &str) -> String {
    let user_id = sqlx::query_scalar!("SELECT id FROM users WHERE account_id = $1", account_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    format!(
        "https://{}/unsubscribe?token={}&type={kind}",
        state.instance.domain,
        urlencoding::encode(&user_token(state, user_id)),
    )
}

/// `UnsubscriptionsController#create` for a user: `settings[type] = false`.
/// Returns whether there was such a user.
pub async fn unsubscribe(state: &AppState, user_id: i64, kind: &str) -> anyhow::Result<bool> {
    let Some(raw) = sqlx::query_scalar!("SELECT settings FROM users WHERE id = $1", user_id)
        .fetch_optional(&state.db)
        .await?
    else {
        return Ok(false);
    };
    let mut settings = raw
        .as_deref()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    settings
        .as_object_mut()
        .expect("an object")
        .insert(format!("notification_emails.{kind}"), false.into());
    sqlx::query!(
        "UPDATE users SET settings = $1, updated_at = now() WHERE id = $2",
        settings.to_string(),
        user_id,
    )
    .execute(&state.db)
    .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::wants;

    #[test]
    fn defaults_follow_user_settings() {
        assert!(wants(None, "follow"));
        assert!(wants(None, "mention"));
        assert!(wants(None, "quote"));
        assert!(wants(None, "follow_request"));
        assert!(!wants(None, "favourite"));
        assert!(!wants(None, "reblog"));
        assert!(!wants(
            Some(r#"{"notification_emails.follow":false}"#),
            "follow"
        ));
        assert!(wants(
            Some(r#"{"notification_emails.reblog":true}"#),
            "reblog"
        ));
    }
}
