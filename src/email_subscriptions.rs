//! Mastodon 4.7's email subscriptions: a local account whose role carries
//! `manage_email_subscriptions`, and whose user has turned the feature on,
//! offers visitors a form to receive its public posts by email.
//!
//! The pieces, each named after what it ports:
//!
//!  -  `EmailSubscription` and `Api::V1::Accounts::EmailSubscriptionsController`:
//!     [`create`], with the model's validations;
//!  -  `EmailSubscriptionMailer#confirmation`: [`send_confirmation`];
//!  -  `EmailSubscriptions::ConfirmationsController`: [`confirm`];
//!  -  `UnsubscriptionsController`, for an email subscription: [`unsubscribe`];
//!  -  `PostStatusService#process_email_subscriptions!`: [`status_posted`];
//!  -  `EmailDistributionWorker` and `EmailSubscriptionMailer#notification`:
//!     [`distribute`];
//!  -  `Admin::EmailSubscriptionsPurgeWorker`: [`purge`];
//!  -  `Scheduler::UserCleanupScheduler#clean_unconfirmed_email_subscriptions!`:
//!     [`clean_unconfirmed`].
//!
//! Mastodon turns the whole feature off with `DISABLE_EMAIL_SUBSCRIPTIONS`;
//! eunha's per-instance `email_subscriptions` configuration flag is the same
//! switch, and the `email_subscriptions` site setting is what an administrator
//! turns on afterwards.

use std::time::Duration;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use crate::db::models::Account;
use crate::state::AppState;

/// `PostStatusService::EMAIL_DISTRIBUTION_DELAY`: posts made within this long
/// of each other go out in one email.
pub const DISTRIBUTION_DELAY: Duration = Duration::from_secs(5 * 60);
/// `PostStatusService::EMAIL_DISTRIBUTION_TTL`, in seconds.
pub const DISTRIBUTION_TTL: i64 = 60 * 60;
/// How long the lock that stands for Sidekiq's `lock: :until_executed` lives,
/// in seconds. Mastodon gives the job's lock a day; eunha's job lives in this
/// process, so a crash would otherwise hold every later batch back for that
/// long. Twice the delay is enough for the job to run.
const DISTRIBUTION_LOCK_TTL: u64 = 2 * 5 * 60 + 60;
/// `Scheduler::UserCleanupScheduler::UNCONFIRMED_ACCOUNTS_MAX_AGE_DAYS`.
pub const UNCONFIRMED_MAX_AGE_DAYS: i64 = 7;
/// How often the unconfirmed subscriptions are cleaned: the scheduler is daily.
pub const CLEANUP_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// The `validates :email, length: { maximum: 320 }`.
const MAX_EMAIL_LENGTH: usize = 320;

/// The site setting an administrator turns the feature on with.
pub const SETTING: &str = "email_subscriptions";
/// `Setting.email_footer_text`, the additional footer of these emails.
pub const FOOTER_SETTING: &str = "email_footer_text";
/// The user setting an account turns its own form on with.
pub const USER_SETTING: &str = "email_subscriptions";

// ── Who may use it ─────────────────────────────────────────────────────────

/// `Rails.application.config.x.email_subscriptions`: whoever runs the instance
/// has not turned the feature off.
pub fn available(state: &AppState) -> bool {
    state.instance.email_subscriptions
}

/// `config.x.email_subscriptions && Setting.email_subscriptions`.
pub async fn enabled(state: &AppState) -> bool {
    available(state) && crate::settings::boolean(state, SETTING).await
}

/// `Account#user_can?(:manage_email_subscriptions)`: the user's role, or the
/// everyone role, carries it; an account without a user never does.
pub async fn user_can(state: &AppState, account_id: i64) -> bool {
    use crate::moderation::role::{self, flag};
    role::of_account(&state.db, account_id)
        .await
        .ok()
        .flatten()
        .is_some_and(|r| r.can(&[flag::MANAGE_EMAIL_SUBSCRIPTIONS]))
}

/// `Account#user_email_subscriptions_enabled?`, off unless the user turned it
/// on.
pub async fn user_enabled(state: &AppState, account_id: i64) -> bool {
    let settings = sqlx::query_scalar!(
        "SELECT settings FROM users WHERE account_id = $1",
        account_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .flatten();
    crate::accounts::user_setting_bool(settings.as_deref(), USER_SETTING, false)
}

/// `user.settings['email_subscriptions'] = on; user.save!`. Returns whether
/// the account had a user to save.
pub async fn set_user_enabled(state: &AppState, account_id: i64, on: bool) -> anyhow::Result<bool> {
    let Some(raw) = sqlx::query_scalar!(
        "SELECT settings FROM users WHERE account_id = $1",
        account_id
    )
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
    settings[USER_SETTING] = serde_json::Value::Bool(on);
    sqlx::query!(
        "UPDATE users SET settings = $1, updated_at = now() WHERE account_id = $2",
        settings.to_string(),
        account_id,
    )
    .execute(&state.db)
    .await?;
    Ok(true)
}

/// Confirmed subscribers of an account: what its owner's privacy settings
/// count.
pub async fn confirmed_count(state: &AppState, account_id: i64) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM email_subscriptions
           WHERE account_id = $1 AND confirmed_at IS NOT NULL"#,
        account_id
    )
    .fetch_one(&state.db)
    .await?)
}

/// The `admin/email_subscriptions/_status` badge: `active`, `disabled` (the
/// role allows it but the user turned it off), `no_access` (turned on, but
/// the role no longer allows it) or `inactive`.
pub async fn admin_status(state: &AppState, account_id: i64) -> &'static str {
    match (
        user_can(state, account_id).await,
        user_enabled(state, account_id).await,
    ) {
        (true, true) => "active",
        (true, false) => "disabled",
        (false, true) => "no_access",
        (false, false) => "inactive",
    }
}

/// `user_can?(:manage_email_subscriptions) && user_email_subscriptions_enabled?`.
pub async fn offered_by(state: &AppState, account_id: i64) -> bool {
    offering(state, &[account_id]).await.contains(&account_id)
}

/// Which of `account_ids` [`offered_by`] says yes for, in one query.
pub async fn offering(state: &AppState, account_ids: &[i64]) -> std::collections::HashSet<i64> {
    use crate::moderation::role::{flag, EVERYONE_ROLE_ID};
    if account_ids.is_empty() {
        return Default::default();
    }
    let everyone: i64 = sqlx::query_scalar!(
        "SELECT permissions FROM user_roles WHERE id = $1",
        EVERYONE_ROLE_ID,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .unwrap_or(flag::DEFAULT);
    // `computed_permissions` of each user's role, as `accounts_who_can` reads
    // it, and the user setting read as `UserSettings` does.
    let rows = sqlx::query!(
        r#"SELECT u.account_id AS "account_id!", u.settings
           FROM users u
           LEFT JOIN user_roles ur ON ur.id = COALESCE(u.role_id, $1)
           WHERE u.account_id = ANY($2)
             AND (
               CASE
                 WHEN COALESCE(ur.id, $1) = $1 THEN COALESCE(ur.permissions, $3)
                 WHEN ur.permissions & 1 = 1 THEN $5
                 ELSE ur.permissions | $3
               END
             ) & $4 <> 0"#,
        EVERYONE_ROLE_ID,
        account_ids,
        everyone,
        flag::MANAGE_EMAIL_SUBSCRIPTIONS,
        flag::ALL,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    rows.into_iter()
        .filter(|r| crate::accounts::user_setting_bool(r.settings.as_deref(), USER_SETTING, false))
        .map(|r| r.account_id)
        .collect()
}

/// `REST::AccountSerializer#email_subscriptions`, which is only there while
/// the feature is enabled.
pub async fn serialized(state: &AppState, account: &Account) -> Option<bool> {
    if !enabled(state).await {
        return None;
    }
    Some(account.domain.is_none() && offered_by(state, account.id).await)
}

/// Mastodon's `display_name(account)` helper: the display name, or the
/// username when it is blank.
pub fn display_name(account: &Account) -> String {
    if account.display_name.trim().is_empty() {
        account.username.clone()
    } else {
        account.display_name.clone()
    }
}

// ── Validation ─────────────────────────────────────────────────────────────

/// What `ActiveRecord::RecordInvalid` carries, rendered as
/// `ValidationErrorFormatter` renders it.
#[derive(Debug, Default)]
pub struct ValidationErrors {
    /// `(attribute, error key, message)`, in the order the validations ran.
    errors: Vec<(&'static str, &'static str, &'static str)>,
}

impl ValidationErrors {
    pub fn add(&mut self, attribute: &'static str, key: &'static str, message: &'static str) {
        self.errors.push((attribute, key, message));
    }

    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }

    /// `errors.full_messages.to_sentence`'s `RecordInvalid` form.
    pub fn message(&self) -> String {
        let full: Vec<String> = self
            .errors
            .iter()
            .map(|(attribute, _, message)| format!("{} {message}", humanize(attribute)))
            .collect();
        format!("Validation failed: {}", full.join(", "))
    }
}

fn humanize(attribute: &str) -> String {
    let mut chars = attribute.replace('_', " ").chars().collect::<Vec<_>>();
    if let Some(first) = chars.first_mut() {
        *first = first.to_ascii_uppercase();
    }
    chars.into_iter().collect()
}

impl IntoResponse for ValidationErrors {
    fn into_response(self) -> Response {
        let mut details = serde_json::Map::new();
        for (attribute, key, message) in &self.errors {
            let entry = details
                .entry(attribute.to_string())
                .or_insert_with(|| serde_json::Value::Array(vec![]));
            if let serde_json::Value::Array(list) = entry {
                list.push(serde_json::json!({
                    "error": format!("ERR_{}", key.to_uppercase()),
                    "description": message,
                }));
            }
        }
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": self.message(), "details": details })),
        )
            .into_response()
    }
}

/// `normalizes :email, with: ->(str) { str.squish.downcase }`.
pub fn normalize_email(email: &str) -> String {
    email
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The model's validations of `email`, run in the order Mastodon declares them.
async fn validate_email(state: &AppState, account_id: i64, email: &str) -> ValidationErrors {
    let mut errors = ValidationErrors::default();
    // `presence: true`
    if email.is_empty() {
        errors.add("email", "blank", "can't be blank");
    }
    // `email_address: true`
    if !crate::accounts::valid_email(email) {
        errors.add("email", "invalid", "is invalid");
    }
    // `length: { maximum: 320 }`
    if email.chars().count() > MAX_EMAIL_LENGTH {
        errors.add(
            "email",
            "too_long",
            "is too long (maximum is 320 characters)",
        );
    }
    // `uniqueness: { scope: :account_id }`
    let taken = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM email_subscriptions
                          WHERE account_id = $1 AND email = $2) AS "e!""#,
        account_id,
        email,
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(false);
    if taken {
        errors.add("email", "taken", "has already been taken");
    }
    // `email_mx: true`, which Mastodon skips in development and test.
    if !email.is_empty() && !crate::moderation::signup::mx_check_skipped() {
        use crate::moderation::signup::{email_domain, email_domain_blocked, resolve_mx};
        match email_domain(email) {
            None => errors.add("email", "invalid", "is invalid"),
            Some(domain) => {
                let mx = resolve_mx(&domain).await;
                if mx.ips.is_empty() {
                    errors.add("email", "unreachable", "does not seem to exist");
                } else {
                    let mut domains = vec![domain];
                    domains.extend(mx.records);
                    if email_domain_blocked(state, &domains, false, None).await {
                        errors.add("email", "blocked", "uses a disallowed e-mail provider");
                    }
                }
            }
        }
    }
    errors
}

/// `Devise.friendly_token`: twenty URL-safe characters, without the ones that
/// read alike.
pub fn friendly_token() -> String {
    use base64::Engine;
    let bytes: [u8; 15] = rand::random();
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(bytes)
        .chars()
        .map(|c| match c {
            'l' => 's',
            'I' => 'x',
            'O' => 'y',
            '0' => 'z',
            c => c,
        })
        .collect()
}

// ── Subscribing ────────────────────────────────────────────────────────────

/// Why a subscription was not made.
#[derive(Debug)]
pub enum CreateError {
    Invalid(ValidationErrors),
    Database(sqlx::Error),
}

impl From<sqlx::Error> for CreateError {
    fn from(e: sqlx::Error) -> Self {
        Self::Database(e)
    }
}

/// `@account.email_subscriptions.create!(email:, locale:)`: validate, store
/// with a confirmation token, and once that is committed mail the address to
/// confirm it (`after_create_commit :send_confirmation_email`).
pub async fn create(
    state: &AppState,
    account_id: i64,
    email: &str,
    locale: &str,
) -> Result<i64, CreateError> {
    let email = normalize_email(email);
    let errors = validate_email(state, account_id, &email).await;
    if !errors.is_empty() {
        return Err(CreateError::Invalid(errors));
    }
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO email_subscriptions
             (account_id, email, locale, confirmation_token, created_at, updated_at)
           VALUES ($1, $2, $3, $4, now(), now())
           RETURNING id"#,
        account_id,
        email,
        locale,
        friendly_token(),
    )
    .fetch_one(&state.db)
    .await;
    let id = match inserted {
        Ok(id) => id,
        // The uniqueness validation lost a race to the index.
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            let mut errors = ValidationErrors::default();
            errors.add("email", "taken", "has already been taken");
            return Err(CreateError::Invalid(errors));
        }
        Err(e) => return Err(e.into()),
    };
    let state = state.clone();
    crate::tenants::spawn(async move {
        if let Err(error) = send_confirmation(&state, id).await {
            tracing::warn!(%error, "could not mail an email subscription's confirmation");
        }
    });
    Ok(id)
}

struct SubscriptionRow {
    id: i64,
    account_id: i64,
    email: String,
    locale: String,
    confirmation_token: Option<String>,
}

async fn load_subscription(state: &AppState, id: i64) -> anyhow::Result<Option<SubscriptionRow>> {
    Ok(sqlx::query_as!(
        SubscriptionRow,
        "SELECT id, account_id, email, locale, confirmation_token
         FROM email_subscriptions WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?)
}

/// Where a subscription's link to unsubscribe goes.
///
/// Mastodon signs a GlobalID with `secret_key_base`, which eunha does not
/// have. The confirmation token is as secret, reaches no one but the
/// subscriber, and Mastodon keeps it after confirming, so it is the key here.
pub fn unsubscribe_url(state: &AppState, token: &str) -> String {
    format!(
        "https://{}/unsubscribe?token={}",
        state.instance.domain,
        urlencoding::encode(token)
    )
}

/// `email_subscriptions_confirmation_url(confirmation_token:)`.
pub fn confirmation_url(state: &AppState, token: &str) -> String {
    format!(
        "https://{}/email_subscriptions/confirmation?confirmation_token={}",
        state.instance.domain,
        urlencoding::encode(token)
    )
}

/// What every one of these emails carries besides its body.
async fn envelope(
    state: &AppState,
    sub: &SubscriptionRow,
    account: &Account,
) -> crate::email::SubscriptionEnvelope {
    let token = sub.confirmation_token.clone().unwrap_or_default();
    let footer = crate::settings::string(state, FOOTER_SETTING).await;
    crate::email::SubscriptionEnvelope {
        to: sub.email.clone(),
        locale: sub.locale.clone(),
        name: display_name(account),
        domain: state.instance.domain.clone(),
        list_id: format!("<{}.{}>", account.username, state.instance.domain),
        unsubscribe_url: unsubscribe_url(state, &token),
        privacy_policy_url: format!("https://{}/about", state.instance.domain),
        footer_text: (!footer.trim().is_empty()).then_some(footer),
    }
}

async fn load_account(state: &AppState, account_id: i64) -> anyhow::Result<Option<Account>> {
    Ok(
        sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", account_id)
            .fetch_optional(&state.db)
            .await?,
    )
}

/// `EmailSubscriptionMailer#confirmation`.
pub async fn send_confirmation(state: &AppState, id: i64) -> anyhow::Result<()> {
    let Some(sub) = load_subscription(state, id).await? else {
        return Ok(());
    };
    let Some(account) = load_account(state, sub.account_id).await? else {
        return Ok(());
    };
    let token = sub.confirmation_token.clone().unwrap_or_default();
    let envelope = envelope(state, &sub, &account).await;
    let avatar = crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &account);
    state
        .email
        .send_subscription_confirmation(
            &envelope,
            &format!("{}@{}", account.username, state.instance.domain),
            &avatar,
            &confirmation_url(state, &token),
        )
        .await
}

/// `EmailSubscriptions::ConfirmationsController#show`: the subscription the
/// token belongs to, confirmed if it was not yet. `None` is a 404.
pub async fn confirm(state: &AppState, token: &str) -> anyhow::Result<Option<Account>> {
    let row = sqlx::query!(
        "SELECT id, account_id, confirmed_at FROM email_subscriptions
         WHERE confirmation_token = $1",
        token
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.confirmed_at.is_none() {
        // `confirm!`: `touch(:confirmed_at)`, which moves `updated_at` too.
        sqlx::query!(
            "UPDATE email_subscriptions SET confirmed_at = now(), updated_at = now()
             WHERE id = $1",
            row.id
        )
        .execute(&state.db)
        .await?;
    }
    load_account(state, row.account_id).await
}

/// The account a link to unsubscribe is for, without unsubscribing: what
/// `UnsubscriptionsController#show` asks about.
pub async fn subscription_account(
    state: &AppState,
    token: &str,
) -> anyhow::Result<Option<Account>> {
    let account_id = sqlx::query_scalar!(
        "SELECT account_id FROM email_subscriptions WHERE confirmation_token = $1",
        token
    )
    .fetch_optional(&state.db)
    .await?;
    match account_id {
        Some(id) => load_account(state, id).await,
        None => Ok(None),
    }
}

/// `UnsubscriptionsController#create` for an email subscription: destroy it.
/// Returns the account it was for, or `None` for a token that names nothing.
pub async fn unsubscribe(state: &AppState, token: &str) -> anyhow::Result<Option<Account>> {
    let account_id = sqlx::query_scalar!(
        "DELETE FROM email_subscriptions WHERE confirmation_token = $1 RETURNING account_id",
        token
    )
    .fetch_optional(&state.db)
    .await?;
    match account_id {
        Some(id) => load_account(state, id).await,
        None => Ok(None),
    }
}

// ── Distribution ───────────────────────────────────────────────────────────

/// `"email_subscriptions:#{account_id}:next_batch"`.
fn batch_key(state: &AppState, account_id: i64) -> String {
    state
        .redis_keys
        .key(format!("email_subscriptions:{account_id}:next_batch"))
}

/// What stands for the unique `EmailDistributionWorker` job of an account.
fn job_key(state: &AppState, account_id: i64) -> String {
    state
        .redis_keys
        .key(format!("email_subscriptions:{account_id}:distribution"))
}

/// What `PostStatusService#process_email_subscriptions!` asks of a post.
pub struct PostedStatus {
    pub id: i64,
    pub account_id: i64,
    pub visibility: String,
    pub in_reply_to_id: Option<i64>,
    pub in_reply_to_account_id: Option<i64>,
}

/// `PostStatusService#process_email_subscriptions!`: a public post that is not
/// a reply to someone else, by an account that offers subscriptions, joins
/// its next batch, and the batch goes out [`DISTRIBUTION_DELAY`] after the
/// first post in it.
pub async fn status_posted(state: &AppState, status: &PostedStatus) {
    let reply = status.in_reply_to_id.is_some();
    if status.visibility != "public"
        || (reply && status.in_reply_to_account_id != Some(status.account_id))
        || !enabled(state).await
        || !offered_by(state, status.account_id).await
    {
        return;
    }
    let mut redis = state.redis_coordination.clone();
    let key = batch_key(state, status.account_id);
    let added: redis::RedisResult<()> = redis::pipe()
        .cmd("SADD")
        .arg(&key)
        .arg(status.id)
        .ignore()
        .cmd("EXPIRE")
        .arg(&key)
        .arg(DISTRIBUTION_TTL)
        .ignore()
        .query_async(&mut redis)
        .await;
    if let Err(error) = added {
        tracing::warn!(%error, "could not add a post to its email batch");
        return;
    }
    schedule(state, status.account_id).await;
}

/// `EmailDistributionWorker.perform_in(EMAIL_DISTRIBUTION_DELAY, account_id)`,
/// unique per account until it has run, as `lock: :until_executed` makes it.
async fn schedule(state: &AppState, account_id: i64) {
    let mut redis = state.redis_coordination.clone();
    let key = job_key(state, account_id);
    let acquired: redis::RedisResult<Option<String>> = redis::cmd("SET")
        .arg(&key)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(DISTRIBUTION_LOCK_TTL)
        .query_async(&mut redis)
        .await;
    if !matches!(acquired, Ok(Some(_))) {
        return;
    }
    let state = state.clone();
    crate::tenants::spawn(async move {
        let stopped = tokio::select! {
            () = state.stop.cancelled() => true,
            () = tokio::time::sleep(DISTRIBUTION_DELAY) => false,
        };
        if !stopped {
            if let Err(error) = distribute(&state, account_id).await {
                tracing::warn!(account_id, %error, "email distribution failed");
            }
        }
        // A stopped instance leaves the batch where it is, for the next post
        // to send along with its own.
        let mut redis = state.redis_coordination.clone();
        let _: redis::RedisResult<()> = redis::cmd("DEL")
            .arg(job_key(&state, account_id))
            .query_async(&mut redis)
            .await;
    });
}

/// The posts waiting in an account's next batch, for tests and diagnosis.
pub async fn pending_batch(state: &AppState, account_id: i64) -> Vec<i64> {
    let mut redis = state.redis_coordination.clone();
    let mut ids: Vec<i64> = redis::cmd("SMEMBERS")
        .arg(batch_key(state, account_id))
        .query_async(&mut redis)
        .await
        .unwrap_or_default();
    ids.sort_unstable();
    ids
}

/// What one run of [`distribute`] sent.
#[derive(Debug, Default, PartialEq)]
pub struct Distribution {
    /// The posts mailed, newest first.
    pub statuses: Vec<i64>,
    /// The addresses they were mailed to.
    pub recipients: Vec<String>,
}

/// `EmailDistributionWorker#perform`: take the account's batch, and mail its
/// public posts that are neither replies to others nor boosts to every
/// confirmed subscriber.
pub async fn distribute(state: &AppState, account_id: i64) -> anyhow::Result<Distribution> {
    if !enabled(state).await {
        return Ok(Distribution::default());
    }
    let Some(account) = load_account(state, account_id).await? else {
        return Ok(Distribution::default());
    };
    if !offered_by(state, account_id).await {
        return Ok(Distribution::default());
    }
    let mut redis = state.redis_coordination.clone();
    let key = batch_key(state, account_id);
    let status_ids: Vec<i64> = redis::cmd("SMEMBERS")
        .arg(&key)
        .query_async(&mut redis)
        .await?;
    if !status_ids.is_empty() {
        let _: () = redis::cmd("SREM")
            .arg(&key)
            .arg(&status_ids)
            .query_async(&mut redis)
            .await?;
    }
    let subscribers = sqlx::query_as!(
        SubscriptionRow,
        "SELECT id, account_id, email, locale, confirmation_token
         FROM email_subscriptions
         WHERE account_id = $1 AND confirmed_at IS NOT NULL
         ORDER BY id",
        account_id
    )
    .fetch_all(&state.db)
    .await?;
    if subscribers.is_empty() || status_ids.is_empty() {
        return Ok(Distribution::default());
    }
    // `without_replies.without_reblogs.public_visibility`, under the default
    // scope's `recent.kept`.
    let statuses = sqlx::query_as!(
        crate::db::models::Status,
        "SELECT * FROM statuses
         WHERE id = ANY($1) AND deleted_at IS NULL
           AND (reply = false OR in_reply_to_account_id = account_id)
           AND reblog_of_id IS NULL
           AND visibility = 0
         ORDER BY id DESC",
        &status_ids
    )
    .fetch_all(&state.db)
    .await?;
    if statuses.is_empty() {
        return Ok(Distribution::default());
    }
    let mut mailed = vec![];
    for status in &statuses {
        mailed.push(mailed_status(state, status, &account).await);
    }
    let excerpt = truncate(&statuses[0].text, 17);
    let sign_up_url = if state.instance.registrations_open {
        format!("https://{}/auth/signup", state.instance.domain)
    } else {
        "https://joinmastodon.org/".to_string()
    };
    let mut recipients = vec![];
    for sub in &subscribers {
        let envelope = envelope(state, sub, &account).await;
        if let Err(error) = state
            .email
            .send_subscription_notification(
                &envelope,
                &account.display_name,
                &excerpt,
                &mailed,
                &sign_up_url,
            )
            .await
        {
            tracing::warn!(%error, subscription = sub.id, "could not mail an email subscriber");
        }
        recipients.push(sub.email.clone());
    }
    Ok(Distribution {
        statuses: statuses.iter().map(|s| s.id).collect(),
        recipients,
    })
}

/// `String#truncate(length)`: at most `length` characters, the last three
/// of them `...` when it had to cut.
fn truncate(text: &str, length: usize) -> String {
    if text.chars().count() <= length {
        return text.to_string();
    }
    let kept: String = text.chars().take(length.saturating_sub(3)).collect();
    format!("{kept}...")
}

/// The `notification_mailer/status` partial's inputs.
async fn mailed_status(
    state: &AppState,
    status: &crate::db::models::Status,
    account: &Account,
) -> crate::email::MailedStatus {
    use crate::api::mastodon::status_serialize::{build_status, fetch_status_media};
    let content = match fetch_status_media(state, status.id).await {
        Ok(media) => build_status(state, status, account, media, None, None)
            .await
            .map(|s| s.content)
            .unwrap_or_else(|_| crate::email::html_escape(&status.text)),
        Err(_) => crate::email::html_escape(&status.text),
    };
    crate::email::MailedStatus {
        name: display_name(account),
        acct: account.acct(),
        avatar_url: crate::api::mastodon::convert::account_avatar_url_for(&state.urls, account),
        spoiler_text: status.spoiler_text.clone(),
        content,
        url: format!(
            "https://{}/@{}/{}",
            state.instance.domain,
            account.acct(),
            status.id
        ),
        created_at: status.created_at.format("%b %d, %Y, %H:%M UTC").to_string(),
    }
}

// ── Housekeeping ───────────────────────────────────────────────────────────

/// `Admin::EmailSubscriptionsPurgeWorker`: every subscription, gone.
pub async fn purge(state: &AppState) -> anyhow::Result<u64> {
    Ok(sqlx::query!("DELETE FROM email_subscriptions")
        .execute(&state.db)
        .await?
        .rows_affected())
}

/// `clean_unconfirmed_email_subscriptions!`: subscriptions nobody confirmed
/// within [`UNCONFIRMED_MAX_AGE_DAYS`].
pub async fn clean_unconfirmed(state: &AppState) -> anyhow::Result<u64> {
    Ok(sqlx::query!(
        "DELETE FROM email_subscriptions
         WHERE confirmed_at IS NULL AND created_at <= now() - make_interval(days => $1)",
        UNCONFIRMED_MAX_AGE_DAYS as i32
    )
    .execute(&state.db)
    .await?
    .rows_affected())
}

/// The daily pass of [`clean_unconfirmed`].
pub async fn run_cleanup(state: AppState) {
    while !state.stop.is_cancelled() {
        if let Err(error) = clean_unconfirmed(&state).await {
            tracing::error!(%error, "unconfirmed email subscription cleanup failed");
        }
        crate::background::rest(&state.stop, CLEANUP_EVERY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_squish_and_downcase() {
        assert_eq!(normalize_email("  Foo@Example.COM \n"), "foo@example.com");
    }

    #[test]
    fn truncates_like_rails() {
        assert_eq!(truncate("short", 17), "short");
        assert_eq!(truncate("exactly seventeen", 17), "exactly seventeen");
        assert_eq!(
            truncate("a little longer than that", 17),
            "a little longe..."
        );
    }

    #[test]
    fn friendly_tokens_are_twenty_characters() {
        let token = friendly_token();
        assert_eq!(token.len(), 20);
        assert!(!token.contains(['l', 'I', 'O', '0']));
    }

    #[test]
    fn validation_messages_read_like_record_invalid() {
        let mut errors = ValidationErrors::default();
        errors.add("email", "blank", "can't be blank");
        errors.add("email", "invalid", "is invalid");
        assert_eq!(
            errors.message(),
            "Validation failed: Email can't be blank, Email is invalid"
        );
    }
}
