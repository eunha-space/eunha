use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use p256::{
    ecdsa::SigningKey,
    pkcs8::{EncodePrivateKey, LineEnding},
};

use crate::state::AppState;

// ── VAPID key generation ───────────────────────────────────────────────────

/// Generates a P-256 VAPID keypair.
/// Returns (pkcs8_pem, public_key_base64url).
/// The PEM is stored in the DB; the base64url key is returned to clients.
pub fn generate_vapid_keypair() -> anyhow::Result<(String, String)> {
    use p256::elliptic_curve::rand_core::OsRng;
    let signing_key = SigningKey::random(&mut OsRng);
    let pem = signing_key
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| anyhow::anyhow!("pkcs8 encode: {e}"))?;

    let pub_point = signing_key.verifying_key().to_encoded_point(false); // uncompressed 65-byte point
    let pub_b64 = URL_SAFE_NO_PAD.encode(pub_point.as_bytes());

    Ok((pem.to_string(), pub_b64))
}

/// Returns the VAPID private key from instance config.
/// In single-tenant mode, keys are sourced from config rather than the DB.
pub fn get_vapid_private_key(state: &AppState) -> &str {
    &state.instance.vapid_private_key
}

pub fn get_vapid_public_key(state: &AppState) -> &str {
    &state.instance.vapid_public_key
}

// ── Push delivery ──────────────────────────────────────────────────────────

/// `Notification::TYPES`, in Mastodon's order: what a subscription may
/// have an alert for.
pub const NOTIFICATION_TYPES: &[&str] = &[
    "mention",
    "status",
    "reblog",
    "follow",
    "follow_request",
    "favourite",
    "poll",
    "update",
    "severed_relationships",
    "moderation_warning",
    "annual_report",
    "admin.sign_up",
    "admin.report",
    "quote",
    "quoted_update",
    "added_to_collection",
    "collection_update",
];

/// `ActiveModel::Type::Boolean#cast`: `nil` and `""` are nil; `false`, `0`
/// and the strings `0`, `f`, `false` and `off` (lower or upper case) are
/// false; anything else is true.
pub fn cast_boolean(value: &serde_json::Value) -> Option<bool> {
    use serde_json::Value;
    match value {
        Value::Null => None,
        Value::Bool(b) => Some(*b),
        Value::Number(n) => Some(n.as_f64() != Some(0.0)),
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(!matches!(
            s.as_str(),
            "0" | "f" | "F" | "false" | "FALSE" | "off" | "OFF"
        )),
        Value::Array(_) | Value::Object(_) => Some(true),
    }
}

/// `Web::PushNotificationWorker::TTL`: 48 hours, the push's `TTL` and how
/// long its unsubscribe token lasts.
pub const PUSH_TTL_SECONDS: i64 = 48 * 3600;

/// `Web::PushNotificationWorker::URGENCY`.
const URGENCY: &str = "normal";

/// What signs an unsubscribe token without `secret_key_base`.
const UNSUBSCRIBE_PURPOSE: &[u8] = b"Web::PushSubscription unsubscribe";

/// `subscription.generate_token_for(:unsubscribe)`: Mastodon's token with
/// `secret_key_base`, and otherwise one keyed from the VAPID key that carries
/// its expiry.
pub fn unsubscribe_token(state: &AppState, id: i64, now: chrono::DateTime<chrono::Utc>) -> String {
    match &state.instance.secret_key_base {
        Some(secret) => secret.push_unsubscribe_token(id, now),
        None => crate::crypto::sign_message(
            &state.instance.vapid_private_key,
            UNSUBSCRIBE_PURPOSE,
            &format!("{id}:{}", now.timestamp() + PUSH_TTL_SECONDS),
        ),
    }
}

/// `Web::PushSubscription.find_by_token_for(:unsubscribe, token)` up to the
/// find: the subscription a genuine, unexpired token names. A token keyed from
/// the VAPID key is read either way.
pub fn verify_unsubscribe_token(
    state: &AppState,
    token: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<i64> {
    if let Some(id) = state
        .instance
        .secret_key_base
        .as_ref()
        .and_then(|secret| secret.verify_push_unsubscribe_token(token, now))
    {
        return Some(id);
    }
    let message = crate::crypto::verify_message(
        &state.instance.vapid_private_key,
        UNSUBSCRIBE_PURPOSE,
        token,
    )?;
    let (id, expires) = message.split_once(':')?;
    (expires.parse::<i64>().ok()? > now.timestamp())
        .then(|| id.parse().ok())
        .flatten()
}

/// `subscription_url`, `api_web_push_subscription_url(id: token)`: the route
/// helper escapes the `/` a base64 token may hold, and nothing else in it.
fn unsubscribe_url(state: &AppState, id: i64) -> String {
    let token = unsubscribe_token(state, id, chrono::Utc::now()).replace('/', "%2F");
    format!(
        "https://{}/api/web/push_subscriptions/{token}",
        state.instance.domain
    )
}

/// `NotifyService#push_to_web_push_subscriptions!`: a
/// [`PushNotificationWorker`] for each of the recipient's subscriptions that
/// is `pushable?` for the notification. Failures are logged and swallowed;
/// push is best-effort.
pub async fn deliver(state: &AppState, notification_id: i64) {
    if let Err(e) = try_deliver(state, notification_id).await {
        tracing::warn!(error = %e, "push delivery error");
    }
}

async fn try_deliver(state: &AppState, notification_id: i64) -> anyhow::Result<()> {
    let Some(notification) = load_notification(state, notification_id).await? else {
        return Ok(());
    };
    // The recipient's user's subscriptions.
    let subscriptions = sqlx::query!(
        r#"SELECT wps.id, wps.data as "data: serde_json::Value"
           FROM web_push_subscriptions wps
           JOIN users u ON u.id = wps.user_id
           WHERE u.account_id = $1
           ORDER BY wps.id"#,
        notification.account_id,
    )
    .fetch_all(&state.db)
    .await?;
    let mut jobs = vec![];
    for subscription in subscriptions {
        if pushable(state, subscription.data.as_ref(), &notification).await? {
            jobs.push(PushNotificationWorker {
                web_push_subscription_id: subscription.id,
                notification_id: Some(notification_id),
            });
        }
    }
    // `Web::PushNotificationWorker.push_bulk(...) { [subscription.id,
    // notification.id] }`: the payload is rendered when each is sent.
    crate::jobs::perform_bulk(state, jobs).await?;
    Ok(())
}

/// A notification as a push reads it.
struct Pushed {
    id: i64,
    notification_type: String,
    account_id: i64,
    from_account_id: i64,
    activity_type: String,
    activity_id: i64,
    updated_at: chrono::NaiveDateTime,
}

/// `Notification.find`, its type read as `Notification#type` reads it: the
/// column, or for a notification from before types were recorded, the one
/// `LEGACY_TYPE_CLASS_MAP` gives its activity.
async fn load_notification(state: &AppState, id: i64) -> anyhow::Result<Option<Pushed>> {
    let Some(row) = sqlx::query!(
        r#"SELECT id, "type", account_id, from_account_id, activity_type, activity_id, updated_at
           FROM notifications WHERE id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(None);
    };
    let notification_type = match row.r#type {
        Some(t) => t,
        None => match row.activity_type.as_str() {
            "Mention" => "mention",
            "Status" => "reblog",
            "Follow" => "follow",
            "FollowRequest" => "follow_request",
            "Favourite" => "favourite",
            "Poll" => "poll",
            "Quote" => "quote",
            _ => "",
        }
        .to_owned(),
    };
    Ok(Some(Pushed {
        id: row.id,
        notification_type,
        account_id: row.account_id,
        from_account_id: row.from_account_id,
        activity_type: row.activity_type,
        activity_id: row.activity_id,
        updated_at: row.updated_at,
    }))
}

/// `Web::NotificationSerializer`, rendered for a subscription: what a push
/// carries, in the subscriber's locale.
#[derive(Debug, serde::Serialize)]
pub struct PushPayload {
    /// The subscription's access token, `associated_access_token`.
    pub access_token: String,
    /// The subscriber's locale, or the default.
    pub preferred_locale: String,
    pub notification_id: i64,
    pub notification_type: String,
    /// The sender's `avatar_static_url`.
    pub icon: String,
    /// `notification_mailer.<type>.subject`, naming the sender.
    pub title: String,
    /// The post's content warning or text, or else the sender's bio,
    /// without its tags and cut to 140 characters.
    pub body: String,
}

/// Render [`PushPayload`] for `subscription_id` and `notification_id`, as
/// `Web::PushNotificationWorker#push_notification_json` does when it sends.
/// `None` when the subscription, its token, the notification or its sender
/// is gone.
pub async fn payload(
    state: &AppState,
    subscription_id: i64,
    notification_id: i64,
) -> anyhow::Result<Option<PushPayload>> {
    let Some(notification) = load_notification(state, notification_id).await? else {
        return Ok(None);
    };
    render(state, subscription_id, &notification).await
}

async fn render(
    state: &AppState,
    subscription_id: i64,
    notification: &Pushed,
) -> anyhow::Result<Option<PushPayload>> {
    let Some(subscriber) = sqlx::query!(
        r#"SELECT t.token AS "token?", u.locale
           FROM web_push_subscriptions wps
           JOIN users u ON u.id = wps.user_id
           LEFT JOIN oauth_access_tokens t ON t.id = wps.access_token_id
           WHERE wps.id = $1"#,
        subscription_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(None);
    };
    let Some(access_token) = subscriber.token else {
        return Ok(None);
    };
    let Some(from) = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        notification.from_account_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(None);
    };
    // `I18n.with_locale(@subscription.locale.presence || I18n.default_locale)`.
    let locale = subscriber
        .locale
        .filter(|l| !l.trim().is_empty())
        .unwrap_or_else(|| crate::api::mastodon::DEFAULT_LOCALE.to_owned());
    // `display_name.presence || username`.
    let name = if from.display_name.trim().is_empty() {
        from.username.clone()
    } else {
        from.display_name.clone()
    };
    // `target_status&.spoiler_text.presence || target_status&.text ||
    // from_account.note`.
    let target = match notification.notification_type.as_str() {
        "status" | "update" | "quoted_update" | "reblog" | "favourite" | "mention" | "quote"
        | "poll" => {
            crate::notification_mail::target_status(
                state,
                &notification.activity_type,
                notification.activity_id,
                &notification.notification_type,
            )
            .await?
        }
        _ => None,
    };
    let source = match target {
        Some(status) if !status.spoiler_text.trim().is_empty() => status.spoiler_text,
        Some(status) => status.text,
        None => from.note.clone(),
    };
    Ok(Some(PushPayload {
        access_token,
        preferred_locale: locale.clone(),
        notification_id: notification.id,
        notification_type: notification.notification_type.clone(),
        icon: crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &from),
        title: push_title(&locale, &notification.notification_type, &name),
        body: push_body(&source),
    }))
}

/// `I18n.t("notification_mailer.#{type}.subject", name:)` in `locale`. A
/// type with no subject reads as Rails reads a missing translation, but for
/// the collection types (divergences.toml,
/// `collection-notification-push-titles`).
fn push_title(locale: &str, notification_type: &str, name: &str) -> String {
    if matches!(
        notification_type,
        "added_to_collection" | "collection_update"
    ) {
        return collection_push_title(notification_type, name);
    }
    let key = format!("notification_mailer.{notification_type}.subject");
    let lang = if locale == "ko" {
        crate::locale::Locale::Ko
    } else {
        crate::locale::Locale::En
    };
    match lang.t(&key) {
        "" => format!("Translation missing: {locale}.{key}"),
        subject => subject.replace("%{name}", name),
    }
}

/// `Web::PushSubscription#pushable?`: the subscription's policy allows the
/// notification, and its alert for the notification's type is on, as
/// `ActiveModel::Type::Boolean` reads it. Any type may be pushed; an alert
/// that is not stored is off.
async fn pushable(
    state: &AppState,
    data: Option<&serde_json::Value>,
    notification: &Pushed,
) -> anyhow::Result<bool> {
    use serde_json::Value;
    let policy_allows = match data.and_then(|d| d.get("policy")) {
        None | Some(Value::Null) => true,
        Some(Value::String(policy)) if policy == "all" => true,
        // `notification.account.following?(notification.from_account)`.
        Some(Value::String(policy)) if policy == "followed" => {
            following(state, notification.account_id, notification.from_account_id).await?
        }
        // `notification.from_account.following?(notification.account)`.
        Some(Value::String(policy)) if policy == "follower" => {
            following(state, notification.from_account_id, notification.account_id).await?
        }
        // `none`, and any policy Mastodon does not know.
        Some(_) => false,
    };
    Ok(policy_allows
        && data
            .and_then(|d| d.get("alerts"))
            .and_then(|a| a.get(&notification.notification_type))
            .and_then(cast_boolean)
            .unwrap_or(false))
}

/// `Account#following?`.
async fn following(
    state: &AppState,
    account_id: i64,
    target_account_id: i64,
) -> anyhow::Result<bool> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2
           ) AS "e!""#,
        account_id,
        target_account_id,
    )
    .fetch_one(&state.db)
    .await?)
}

/// `Web::PushNotificationWorker`: one notification, to one subscription.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PushNotificationWorker {
    pub web_push_subscription_id: i64,
    /// `None` for a job an earlier eunha queued with its payload already
    /// rendered, which is dropped.
    #[serde(default)]
    pub notification_id: Option<i64>,
}

impl crate::jobs::Job for PushNotificationWorker {
    const KIND: &'static str = "Web::PushNotificationWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Push)
        .retry(5);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        let Some(notification_id) = self.notification_id else {
            return Ok(());
        };
        // `Web::PushSubscription.find` and `Notification.find`, else
        // nothing to do.
        let Some(sub) = sqlx::query!(
            r#"SELECT wps.endpoint, wps.key_p256dh, wps.key_auth, wps.standard,
                      wps.data AS "data: serde_json::Value",
                      EXISTS (SELECT 1 FROM users u WHERE u.id = wps.user_id)
                        AND EXISTS (SELECT 1 FROM oauth_access_tokens t
                                    WHERE t.id = wps.access_token_id) AS "owned!"
               FROM web_push_subscriptions wps WHERE wps.id = $1"#,
            self.web_push_subscription_id
        )
        .fetch_optional(&state.db)
        .await?
        else {
            return Ok(());
        };
        let Some(notification) = load_notification(state, notification_id).await? else {
            return Ok(());
        };

        // `return if @notification.updated_at < TTL.ago`.
        if notification.updated_at
            < chrono::Utc::now().naive_utc() - chrono::Duration::seconds(PUSH_TTL_SECONDS)
        {
            return Ok(());
        }

        // `@subscription.destroy! unless @subscription.valid?`: one made
        // before its endpoint and keys were checked, or whose user or token
        // is gone.
        if !sub.owned
            || !subscription_errors(&sub.endpoint, &sub.key_p256dh, &sub.key_auth).is_empty()
        {
            sqlx::query!(
                "DELETE FROM web_push_subscriptions WHERE id = $1",
                self.web_push_subscription_id
            )
            .execute(&state.db)
            .await?;
            return Ok(());
        }

        // `return unless @notification.activity.present? &&
        // @subscription.pushable?(@notification)`: the activity may have
        // been deleted since, and the subscription changed.
        if !activity_present(state, &notification.activity_type, notification.activity_id).await?
            || !pushable(state, sub.data.as_ref(), &notification).await?
        {
            return Ok(());
        }

        // `push_notification_json`, rendered now.
        let Some(payload) = render(state, self.web_push_subscription_id, &notification).await?
        else {
            return Ok(());
        };
        let status = send_one(
            state,
            &sub.endpoint,
            &sub.key_p256dh,
            &sub.key_auth,
            sub.standard,
            &unsubscribe_url(state, self.web_push_subscription_id),
            &serde_json::to_string(&payload)?,
        )
        .await?;
        // `#send`: a 4xx other than a timeout or rate limit means the
        // subscription is gone or was never valid, so it is destroyed;
        // anything else that is not a success is tried again.
        let code = status.as_u16();
        if (400..500).contains(&code) && code != 408 && code != 429 {
            sqlx::query!(
                "DELETE FROM web_push_subscriptions WHERE id = $1",
                self.web_push_subscription_id
            )
            .execute(&state.db)
            .await?;
            return Ok(());
        }
        if !status.is_success() {
            anyhow::bail!("push endpoint answered {status}");
        }
        Ok(())
    }
}

/// `Notification#activity.present?`: the polymorphic activity still exists,
/// a status only while it is not deleted (`Status`'s default scope).
async fn activity_present(
    state: &AppState,
    activity_type: &str,
    activity_id: i64,
) -> anyhow::Result<bool> {
    let table = match activity_type {
        "Status" => "statuses",
        "Mention" => "mentions",
        "Favourite" => "favourites",
        "Follow" => "follows",
        "FollowRequest" => "follow_requests",
        "Poll" => "polls",
        "Report" => "reports",
        "AccountRelationshipSeveranceEvent" => "account_relationship_severance_events",
        "AccountWarning" => "account_warnings",
        "GeneratedAnnualReport" => "generated_annual_reports",
        "Quote" => "quotes",
        "CollectionItem" => "collection_items",
        "Collection" => "collections",
        "Account" => "accounts",
        _ => return Ok(false),
    };
    let kept = if table == "statuses" {
        " AND deleted_at IS NULL"
    } else {
        ""
    };
    Ok(sqlx::query_scalar::<_, bool>(&format!(
        "SELECT EXISTS (SELECT 1 FROM {table} WHERE id = $1{kept})"
    ))
    .bind(activity_id)
    .fetch_one(&state.db)
    .await?)
}

/// `Web::PushSubscription`'s validations, as full messages: the endpoint
/// present and an `http` or `https` URL with a host (`URLValidator`), both
/// keys present, and `WebPushKeyValidator`, which has them encrypt a test
/// message.
pub fn subscription_errors(endpoint: &str, p256dh: &str, auth: &str) -> Vec<String> {
    let mut errors = vec![];
    if endpoint.trim().is_empty() {
        errors.push("Endpoint can't be blank".to_owned());
    }
    let url_ok = url::Url::parse(endpoint).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some_and(|h| !h.is_empty())
    });
    if !url_ok {
        errors.push("Endpoint is invalid".to_owned());
    }
    if p256dh.trim().is_empty() {
        errors.push("Key p256dh can't be blank".to_owned());
    }
    if auth.trim().is_empty() {
        errors.push("Key auth can't be blank".to_owned());
    }
    let info = web_push::SubscriptionInfo {
        endpoint: "https://push.invalid/".to_owned(),
        keys: web_push::SubscriptionKeys {
            p256dh: p256dh.to_owned(),
            auth: auth.to_owned(),
        },
    };
    let mut builder = web_push::WebPushMessageBuilder::new(&info);
    builder.set_payload(web_push::ContentEncoding::AesGcm, b"validation_test");
    if p256dh.is_empty() || auth.is_empty() || builder.build().is_err() {
        errors.push("is not a valid Ed25519 or Curve25519 key".to_owned());
    }
    errors
}

/// Encrypt and send one push: `perform_standard_request` (`aes128gcm`, RFC
/// 8291, with RFC 8292 VAPID) for a `standard` subscription, else
/// `perform_legacy_request` (`aesgcm`). Either way with Mastodon's `TTL`,
/// `Urgency` and `Unsubscribe-URL`. Returns what the endpoint answered.
async fn send_one(
    state: &AppState,
    endpoint: &str,
    p256dh: &str,
    auth: &str,
    standard: bool,
    unsubscribe_url: &str,
    payload: &str,
) -> anyhow::Result<reqwest::StatusCode> {
    use web_push::{
        ContentEncoding, SubscriptionInfo, SubscriptionKeys, VapidSignatureBuilder,
        WebPushMessageBuilder,
    };

    let sub_info = SubscriptionInfo {
        endpoint: endpoint.to_string(),
        keys: SubscriptionKeys {
            auth: auth.to_string(),
            p256dh: p256dh.to_string(),
        },
    };

    let mut builder = WebPushMessageBuilder::new(&sub_info);
    let encoding = if standard {
        ContentEncoding::Aes128Gcm
    } else {
        ContentEncoding::AesGcm
    };
    builder.set_payload(encoding, payload.as_bytes());
    builder.set_ttl(PUSH_TTL_SECONDS as u32);

    let sig_builder =
        VapidSignatureBuilder::from_pem(state.instance.vapid_private_key.as_bytes(), &sub_info)?;
    builder.set_vapid_signature(sig_builder.build()?);

    let message = builder.build()?;

    send_with_reqwest(&state.http, message, unsubscribe_url).await
}

async fn send_with_reqwest(
    http: &reqwest::Client,
    message: web_push::WebPushMessage,
    unsubscribe_url: &str,
) -> anyhow::Result<reqwest::StatusCode> {
    let endpoint = message.endpoint.to_string();
    let ttl = message.ttl;
    let mut req = http
        .post(endpoint.as_str())
        .header("TTL", ttl.to_string())
        .header("Urgency", URGENCY)
        .header("Unsubscribe-URL", unsubscribe_url);

    if let Some(payload) = message.payload {
        req = req
            .header("Content-Encoding", payload.content_encoding.to_str())
            .header("Content-Type", "application/octet-stream");

        for (k, v) in &payload.crypto_headers {
            req = req.header(*k, v.as_str());
        }

        req = req.body(payload.content);
    }

    let resp = req.send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!(status = %status, body = %body, endpoint = %endpoint, "push relay rejected message");
    }

    Ok(status)
}

// ── Notification creation helper ───────────────────────────────────────────

/// Insert a notification record and fire push delivery in a background task.
/// The group a new notification belongs to, or `None` for a type that does not
/// group.
///
/// Mastodon's `Notification::Groups#set_group_key!`. The key is a prefix — what
/// is being grouped — and an hour bucket, and the bucket is where the behaviour
/// is: a new notification joins the previous group rather than starting its own,
/// *unless* that group already reaches back more than `MAXIMUM_GROUP_SPAN_HOURS`.
/// So a post favourited twice in an afternoon reads as one group, and favourited
/// again the next day reads as two.
///
/// The running bucket lives in Redis under the same key Mastodon uses, with the
/// same expiry, so the window slides rather than being a fixed clock division.
async fn notification_group_key(
    redis: &mut redis::aio::ConnectionManager,
    redis_keys: &crate::redis_keys::RedisKeyspace,
    recipient_id: i64,
    notification_type: &str,
    status_id: Option<i64>,
    activity_created_at: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    use crate::api::mastodon::notifications::{group_type_prefix, MAXIMUM_GROUP_SPAN_HOURS};

    let prefix = group_type_prefix(notification_type, status_id)?;
    let redis_key = redis_keys.key(format!("notif-group/{recipient_id}/{prefix}"));
    let hour = 3600;
    // `activity.created_at.utc.to_i / 1.hour.to_i`
    let mut bucket = activity_created_at.timestamp() / hour;

    let previous: Option<i64> = redis::cmd("GET")
        .arg(&redis_key)
        .query_async(redis)
        .await
        .ok()
        .flatten();
    if let Some(previous) = previous {
        if bucket < previous + MAXIMUM_GROUP_SPAN_HOURS {
            bucket = previous;
        }
    }
    let _: redis::RedisResult<()> = redis::cmd("SET")
        .arg(&redis_key)
        .arg(bucket)
        .arg("EX")
        .arg(MAXIMUM_GROUP_SPAN_HOURS * hour)
        .query_async(redis)
        .await;

    Some(format!("{prefix}-{bucket}"))
}

/// Resolve a notification's polymorphic activity (`activity_type`, `activity_id`).
/// Both columns are NOT NULL in the schema, so a notification without a
/// resolvable activity is dropped rather than inserted with NULLs.
async fn notification_activity(
    db: &sqlx::PgPool,
    notification_type: &str,
    recipient_id: i64,
    from_account_id: i64,
    status_id: Option<i64>,
) -> Option<(&'static str, i64)> {
    // A `quote` is about the Quote, as Mastodon's `LocalNotificationWorker`
    // is given it; `status_id` is the quoting status.
    if notification_type == "quote" {
        let sid = status_id?;
        return sqlx::query_scalar!("SELECT id FROM quotes WHERE status_id = $1", sid)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .map(|id| ("Quote", id));
    }
    // A `mention` is about the recipient's `Mention`, as
    // `notify_mentioned_accounts!` hands it to `LocalNotificationWorker`;
    // without one there is nothing to notify of.
    if notification_type == "mention" {
        let sid = status_id?;
        return sqlx::query_scalar!(
            "SELECT id FROM mentions WHERE status_id = $1 AND account_id = $2",
            sid,
            recipient_id,
        )
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|id| ("Mention", id));
    }
    if let Some(sid) = status_id {
        return Some(("Status", sid));
    }
    match notification_type {
        // The Follow links follower→followed; the direction differs for a fresh
        // follow vs. an accepted follow request, so match either orientation.
        "follow" => sqlx::query_scalar!(
            r#"SELECT id FROM follows
               WHERE (account_id = $1 AND target_account_id = $2)
                  OR (account_id = $2 AND target_account_id = $1)
               LIMIT 1"#,
            from_account_id,
            recipient_id,
        )
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|id| ("Follow", id)),
        "follow_request" => sqlx::query_scalar!(
            "SELECT id FROM follow_requests WHERE account_id = $1 AND target_account_id = $2 LIMIT 1",
            from_account_id,
            recipient_id,
        )
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|id| ("FollowRequest", id)),
        _ => None,
    }
}

pub async fn create_and_push(
    state: &AppState,
    recipient_id: i64,
    from_account_id: i64,
    notification_type: &'static str,
    status_id: Option<i64>,
) {
    create_and_push_with(
        state,
        recipient_id,
        from_account_id,
        notification_type,
        status_id,
        false,
    )
    .await;
}

/// [`create_and_push`] with `NotifyService`'s `silenced:` option: the
/// recipient's policy treats the sender as a limited account, as for a
/// mention of someone outside the status's audience.
pub async fn create_and_push_with(
    state: &AppState,
    recipient_id: i64,
    from_account_id: i64,
    notification_type: &'static str,
    status_id: Option<i64>,
    silenced: bool,
) {
    Box::pin(notify(
        state,
        recipient_id,
        from_account_id,
        notification_type,
        status_id,
        None,
        silenced,
    ))
    .await;
}

/// `LocalNotificationWorker` for `added_to_collection` (about a
/// `CollectionItem`) and `collection_update` (about a `Collection`), from the
/// collection's owner (`Notification#set_from_account`). Neither is about a
/// post; both go through `NotifyService`'s checks, and only
/// `added_to_collection` is filterable. Neither is mailed.
pub async fn notify_collection(
    state: &AppState,
    recipient_id: i64,
    notification_type: &'static str,
    activity: (&'static str, i64),
    from_account_id: i64,
) {
    Box::pin(notify(
        state,
        recipient_id,
        from_account_id,
        notification_type,
        None,
        Some(activity),
        false,
    ))
    .await;
}

/// A collection notification's push title. Mastodon's
/// `I18n.t("notification_mailer.#{type}.subject")` has no such key for
/// either type, so it pushes `Translation missing: …`; eunha says what its
/// notification fallback says (divergences.toml,
/// `collection-notification-push-titles`).
fn collection_push_title(notification_type: &str, name: &str) -> String {
    match notification_type {
        "added_to_collection" => format!("{name} added you to a collection"),
        _ => format!("{name} updated a collection you are in"),
    }
}

/// `truncate(HTMLEntities.new.decode(strip_tags(text)), length: 140)`:
/// the text without its tags, entities decoded, and if longer than 140
/// characters, its first 137 and `...`.
fn push_body(html: &str) -> String {
    let text: String = scraper::Html::parse_fragment(html)
        .root_element()
        .text()
        .collect();
    if text.chars().count() <= 140 {
        return text;
    }
    let mut truncated: String = text.chars().take(137).collect();
    truncated.push_str("...");
    truncated
}

/// `NotifyService`, for a notification about `status_id`, or about
/// `activity` when given.
#[allow(clippy::too_many_arguments)]
async fn notify(
    state: &AppState,
    recipient_id: i64,
    from_account_id: i64,
    notification_type: &'static str,
    status_id: Option<i64>,
    activity: Option<(&'static str, i64)>,
    silenced: bool,
) {
    let db = state.db.clone();

    // Don't notify yourself — except for the types exempt from it.
    if recipient_id == from_account_id && !SELF_NOTIFIABLE_TYPES.contains(&notification_type) {
        return;
    }

    // Local-only: Mastodon's LocalNotificationWorker only notifies local
    // accounts, so never create a notification row for a remote recipient
    // (e.g. favouriting or boosting a remote author's post).
    // `return if recipient.user.nil?`, and `DropCondition`'s
    // `@recipient.unavailable?`.
    let recipient_local = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM accounts a JOIN users u ON u.account_id = a.id
             WHERE a.id = $1 AND a.domain IS NULL
               AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
           ) AS "e!""#,
        recipient_id,
    )
    .fetch_one(&db)
    .await
    .unwrap_or(false);
    if !recipient_local {
        return;
    }

    // `return blocked if message? && from_staff?`: a mention from staff whose
    // role may bypass the recipient's is held to none of the recipient's
    // blocks, mutes or filters below.
    let staff_message = notification_type == "mention"
        && crate::moderation::notification_policy::from_staff(&db, recipient_id, from_account_id)
            .await;

    // `@recipient.blocking?(@sender)`: the recipient's block, and only
    // theirs; a staff mention passes it.
    let is_blocked = !staff_message
        && sqlx::query_scalar!(
            "SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2",
            recipient_id,
            from_account_id,
        )
        .fetch_optional(&db)
        .await
        .ok()
        .flatten()
        .is_some();
    if is_blocked {
        return;
    }

    // Mastodon's `domain_blocking?`: blocking a domain says you want nothing
    // from it, and a notification is the most direct way it would reach you.
    // Following someone there is the exception — a deliberate choice to keep
    // hearing from that account despite the domain block.
    let domain_blocked = sqlx::query_scalar!(
        r#"SELECT 1 FROM account_domain_blocks adb
           JOIN accounts sender ON sender.id = $2
           WHERE adb.account_id = $1
             AND sender.domain IS NOT NULL
             AND adb.domain = sender.domain
             AND NOT EXISTS (
               SELECT 1 FROM follows
               WHERE account_id = $1 AND target_account_id = $2
             )"#,
        recipient_id,
        from_account_id,
    )
    .fetch_optional(&db)
    .await
    .ok()
    .flatten()
    .is_some();
    if domain_blocked && !staff_message {
        return;
    }

    // `@recipient.muting_notifications?(@sender)`: a mute that hides
    // notifications drops every one from the muted account, a mention of the
    // recipient and a favourite of their post among them.
    let notifications_hidden = sqlx::query_scalar!(
        r#"SELECT 1 FROM mutes
           WHERE account_id = $1 AND target_account_id = $2 AND hide_notifications = true"#,
        recipient_id,
        from_account_id,
    )
    .fetch_optional(&db)
    .await
    .ok()
    .flatten()
    .is_some();
    if notifications_hidden && !staff_message {
        return;
    }

    // Mastodon's `blocked_mention?` — `FeedManager#filter_from_mentions?`. A
    // mention is dropped when the status mentions, or replies to, an account the
    // recipient blocks, or mutes with its notifications hidden
    // (`blocks_or_mutes?` in the `:mentions` context), even though the sender
    // is neither.
    if notification_type == "mention" && !staff_message {
        if let Some(sid) = status_id {
            let drags_in_blocked = sqlx::query_scalar!(
                r#"SELECT 1 FROM statuses s
                   WHERE s.id = $2
                     AND EXISTS (
                       SELECT 1 FROM (
                         SELECT m.account_id FROM mentions m
                         WHERE m.status_id = s.id AND NOT m.silent
                         UNION
                         SELECT s.in_reply_to_account_id WHERE s.in_reply_to_account_id IS NOT NULL
                       ) AS involved(account_id)
                       WHERE involved.account_id <> $1
                         AND (EXISTS (
                           SELECT 1 FROM blocks b
                           WHERE b.account_id = $1 AND b.target_account_id = involved.account_id
                         ) OR EXISTS (
                           SELECT 1 FROM mutes mu
                           WHERE mu.account_id = $1 AND mu.target_account_id = involved.account_id
                             AND mu.hide_notifications
                         ))
                     )"#,
                recipient_id,
                sid,
            )
            .fetch_optional(&db)
            .await
            .ok()
            .flatten()
            .is_some();
            if drags_in_blocked {
                return;
            }
        }
    }

    // Don't notify if the recipient has muted the conversation
    if let Some(sid) = status_id.filter(|_| !staff_message) {
        let conversation_muted = sqlx::query_scalar!(
            "SELECT 1 FROM conversation_mutes cm JOIN statuses s ON s.id = $2 WHERE cm.account_id = $1 AND cm.conversation_id = s.conversation_id LIMIT 1",
            recipient_id, sid,
        )
        .fetch_optional(&db)
        .await
        .ok()
        .flatten()
        .is_some();
        if conversation_muted {
            return;
        }
    }

    // `NotifyService`: the recipient's notification policy drops it, files
    // it as filtered, or lets it through.
    let filtered = match crate::moderation::notification_policy::decide(
        &db,
        recipient_id,
        from_account_id,
        notification_type,
        status_id,
        silenced,
    )
    .await
    {
        crate::moderation::notification_policy::Decision::Drop => return,
        crate::moderation::notification_policy::Decision::Filter => true,
        crate::moderation::notification_policy::Decision::Deliver => false,
    };

    // Resolve the polymorphic activity (activity_type/activity_id are NOT NULL).
    // Status-bearing notifications point at the Status; follow(_request)s point
    // at the Follow/FollowRequest row.
    let activity = match activity {
        Some(activity) => Some(activity),
        None => {
            notification_activity(
                &db,
                notification_type,
                recipient_id,
                from_account_id,
                status_id,
            )
            .await
        }
    };
    let Some((activity_type_val, activity_id_val)) = activity else {
        tracing::warn!(
            notification_type,
            "no activity found for notification; skipping"
        );
        return;
    };

    // `LocalNotificationWorker`: an `update` or `quoted_update` replaces the
    // earlier ones about the same status, so the newest edit is what is said.
    if matches!(
        notification_type,
        "update" | "quoted_update" | "collection_update"
    ) {
        if let Err(e) = sqlx::query!(
            r#"DELETE FROM notifications
               WHERE account_id = $1 AND "type" = $2
                 AND activity_type = $3 AND activity_id = $4"#,
            recipient_id,
            notification_type,
            activity_type_val,
            activity_id_val,
        )
        .execute(&db)
        .await
        {
            tracing::warn!(error = %e, "could not replace an earlier update notification");
        }
    }

    // Dedup: don't insert the same (account, from, type, activity) twice
    let existing = sqlx::query_scalar!(
        r#"SELECT 1 FROM notifications
           WHERE account_id = $1 AND from_account_id = $2
             AND "type" = $3 AND activity_id = $4
           LIMIT 1"#,
        recipient_id,
        from_account_id,
        notification_type,
        activity_id_val,
    )
    .fetch_optional(&db)
    .await;

    if matches!(existing, Ok(Some(_))) {
        return;
    }

    // Mastodon decides a notification's group when it is created, not when it is
    // read, because the decision depends on when the previous one arrived.
    // `set_group_key!` returns early for a filtered notification: it keeps no
    // key, even once it is unfiltered, and leaves the running bucket alone.
    // The groupable types' activities — the favourite, the boost, the follow —
    // are made just before they are notified of, so now is their
    // `created_at`.
    let group_key = if filtered {
        None
    } else {
        notification_group_key(
            &mut state.redis_coordination.clone(),
            &state.redis_keys,
            recipient_id,
            notification_type,
            status_id,
            chrono::Utc::now(),
        )
        .await
    };

    let row = sqlx::query!(
        r#"INSERT INTO notifications (account_id, from_account_id, "type", activity_type, activity_id, group_key, filtered, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, now(), now())
           RETURNING id"#,
        recipient_id,
        from_account_id,
        notification_type,
        activity_type_val,
        activity_id_val,
        group_key,
        filtered,
    )
    .fetch_one(&db)
    .await;

    let notification_id = match row {
        Ok(r) => r.id,
        Err(e) => {
            tracing::warn!(error = %e, "failed to create notification");
            return;
        }
    };

    // `update_notification_request!`: a filtered mention or quote is counted
    // in the sender's notification request; nothing filtered is pushed.
    if filtered {
        if matches!(notification_type, "mention" | "quote") {
            update_notification_request(&db, recipient_id, from_account_id, status_id).await;
        }
        return;
    }

    // `push_to_streaming_api! if subscribed_to_streaming_api?`.
    crate::streaming::fan_out::notification(state, recipient_id, notification_id).await;

    // `push_to_conversation! if direct_message?`: a mention in a direct
    // message, delivered and not filtered, adds it to the recipient's
    // conversations.
    if let (Some(sid), "mention") = (status_id, notification_type) {
        let direct = sqlx::query_scalar!(
            r#"SELECT (visibility = $2) AS "direct!" FROM statuses WHERE id = $1"#,
            sid,
            crate::db::models::vis::DIRECT,
        )
        .fetch_optional(&db)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);
        if direct {
            crate::api::mastodon::conversations::add_status(state, recipient_id, sid).await;
        }
    }

    deliver(state, notification_id).await;

    // `send_email! if email_needed?`
    crate::notification_mail::notification_delivered(
        state,
        notification_id,
        recipient_id,
        notification_type,
    )
    .await;
}

/// The types Mastodon exempts from its self-notification block
/// (`NotifyService::DropCondition#drop?`), notably `poll`, so that the poll
/// owner is still told when their own poll ends.
const SELF_NOTIFIABLE_TYPES: &[&str] = &[
    "poll",
    "severed_relationships",
    "moderation_warning",
    "annual_report",
];

/// `LocalNotificationWorker` and `NotifyService` for the notification types
/// that are about the recipient's own standing or staff work rather than
/// someone's post — `admin.report`, `admin.sign_up`, `moderation_warning` —
/// none of which is filterable, so none passes through the notification
/// policy; the recipient's blocks, domain blocks and mutes still drop them.
///
/// `activity_type`/`activity_id` are the polymorphic activity; `from_account_id`
/// is what `Notification#set_from_account` derives from it. A recipient
/// without a user, or one already notified of this activity, gets nothing.
pub async fn notify_local(
    state: &AppState,
    recipient_id: i64,
    notification_type: &'static str,
    activity_type: &'static str,
    activity_id: i64,
    from_account_id: i64,
) {
    // `DropCondition#drop?`: nobody is told of their own doing — of their own
    // sign-up, say, when they may manage users — but for the types exempt.
    if recipient_id == from_account_id && !SELF_NOTIFIABLE_TYPES.contains(&notification_type) {
        return;
    }
    let result: anyhow::Result<()> = async {
        // `return if recipient.user.nil?`
        let has_user = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM users WHERE account_id = $1) AS "e!""#,
            recipient_id,
        )
        .fetch_one(&state.db)
        .await?;
        if !has_user {
            return Ok(());
        }
        // The rest of `DropCondition#drop?` that applies to a type about no
        // post: the recipient unavailable, the sender's domain blocked by a
        // recipient not following them, the sender blocked, or muted with
        // their notifications. None of these types is filterable.
        let dropped = sqlx::query_scalar!(
            r#"SELECT
                 NOT EXISTS (
                   SELECT 1 FROM accounts
                   WHERE id = $1 AND suspended_at IS NULL AND requested_deletion_at IS NULL
                 )
                 OR EXISTS (
                   SELECT 1 FROM account_domain_blocks adb
                   JOIN accounts sender ON sender.id = $2
                   WHERE adb.account_id = $1 AND adb.domain = sender.domain
                     AND NOT EXISTS (
                       SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2
                     )
                 )
                 OR EXISTS (
                   SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2
                 )
                 -- `muting_notifications?` asks only whether the mute is
                 -- there: an expired one still mutes until it is removed.
                 OR EXISTS (
                   SELECT 1 FROM mutes
                   WHERE account_id = $1 AND target_account_id = $2 AND hide_notifications
                 ) AS "dropped!""#,
            recipient_id,
            from_account_id,
        )
        .fetch_one(&state.db)
        .await?;
        if dropped {
            return Ok(());
        }
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS (
                 SELECT 1 FROM notifications
                 WHERE account_id = $1 AND activity_type = $2 AND activity_id = $3 AND "type" = $4
               ) AS "e!""#,
            recipient_id,
            activity_type,
            activity_id,
            notification_type,
        )
        .fetch_one(&state.db)
        .await?;
        if exists {
            return Ok(());
        }
        // `set_group_key!`: of these types only `admin.sign_up` groups, by
        // the hour its account was made in.
        let group_key = if activity_type == "Account" {
            let created_at = sqlx::query_scalar!(
                "SELECT created_at FROM accounts WHERE id = $1",
                activity_id,
            )
            .fetch_optional(&state.db)
            .await?
            .map(|t| t.and_utc())
            .unwrap_or_else(chrono::Utc::now);
            notification_group_key(
                &mut state.redis_coordination.clone(),
                &state.redis_keys,
                recipient_id,
                notification_type,
                None,
                created_at,
            )
            .await
        } else {
            None
        };
        let notification_id = sqlx::query_scalar!(
            r#"INSERT INTO notifications (account_id, from_account_id, "type", activity_type, activity_id, group_key, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, now(), now())
               RETURNING id"#,
            recipient_id,
            from_account_id,
            notification_type,
            activity_type,
            activity_id,
            group_key,
        )
        .fetch_one(&state.db)
        .await?;

        crate::streaming::fan_out::notification(state, recipient_id, notification_id).await;

        deliver(state, notification_id).await;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, notification_type, "could not create a notification");
    }
}

/// The local accounts whose role carries any of `flags`: `User.those_who_can`,
/// which is the users of `UserRole.that_can`, the everyone role's included.
pub async fn accounts_who_can(state: &AppState, flags: &[i64]) -> anyhow::Result<Vec<i64>> {
    use crate::moderation::role::{flag, EVERYONE_ROLE_ID};
    let any: i64 = flags.iter().fold(0, |acc, f| acc | f);
    let everyone: i64 = sqlx::query_scalar!(
        "SELECT permissions FROM user_roles WHERE id = $1",
        EVERYONE_ROLE_ID,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(flag::DEFAULT);
    // `computed_permissions` per role, then any one of the flags.
    Ok(sqlx::query_scalar!(
        r#"SELECT u.account_id FROM users u
           JOIN accounts a ON a.id = u.account_id
           LEFT JOIN user_roles ur ON ur.id = COALESCE(u.role_id, $1)
           WHERE a.domain IS NULL
             AND (
               CASE
                 WHEN COALESCE(ur.id, $1) = $1 THEN COALESCE(ur.permissions, $2)
                 WHEN ur.permissions & 1 = 1 THEN $4
                 ELSE ur.permissions | $2
               END
             ) & $3 <> 0
           ORDER BY u.account_id"#,
        EVERYONE_ROLE_ID,
        everyone,
        any,
        flag::ALL,
    )
    .fetch_all(&state.db)
    .await?)
}

/// `NotificationRequest` upkeep after a filtered mention or quote: the
/// request's `last_status_id` and `prepare_notifications_count`, the filtered
/// mentions and quotes from the sender, counted to 100.
pub(crate) async fn update_notification_request(
    db: &sqlx::PgPool,
    recipient_id: i64,
    from_account_id: i64,
    status_id: Option<i64>,
) {
    let _ = sqlx::query!(
        r#"INSERT INTO notification_requests
               (account_id, from_account_id, last_status_id, notifications_count, created_at, updated_at)
           VALUES ($1, $2, $3,
                   (SELECT count(*) FROM (SELECT 1 FROM notifications
                    WHERE account_id = $1 AND from_account_id = $2 AND filtered
                      AND "type" IN ('mention', 'quote') LIMIT 100) n),
                   now(), now())
           ON CONFLICT (account_id, from_account_id) DO UPDATE
             SET notifications_count = EXCLUDED.notifications_count,
                 last_status_id = COALESCE($3, notification_requests.last_status_id),
                 updated_at = now()"#,
        recipient_id,
        from_account_id,
        status_id,
    )
    .execute(db)
    .await;
}

#[cfg(test)]
mod tests {
    use super::{cast_boolean, push_body, push_title, subscription_errors, NOTIFICATION_TYPES};
    use serde_json::json;

    #[test]
    fn collection_pushes_say_what_happened() {
        assert_eq!(
            push_title("en", "added_to_collection", "Alice"),
            "Alice added you to a collection"
        );
        assert_eq!(
            push_title("ko", "collection_update", "Alice"),
            "Alice updated a collection you are in"
        );
    }

    #[test]
    fn push_titles_are_the_mailers_subjects() {
        assert_eq!(
            push_title("en", "favourite", "Alice"),
            "Alice favorited your post"
        );
        assert_eq!(
            push_title("ko", "admin.report", "Alice"),
            "Alice 님이 신고를 제출했습니다"
        );
        assert_eq!(
            push_title("en", "moderation_warning", "Alice"),
            "You have received a moderation warning"
        );
        // Every type but these three has a subject in both locales.
        for t in NOTIFICATION_TYPES {
            if matches!(
                *t,
                "annual_report" | "added_to_collection" | "collection_update"
            ) {
                continue;
            }
            for locale in ["en", "ko"] {
                assert!(
                    !push_title(locale, t, "A").starts_with("Translation missing"),
                    "{locale} {t}"
                );
            }
        }
        // Any other locale gets the English subject (divergences.toml,
        // `push-titles-in-english-and-korean`).
        assert_eq!(push_title("ja", "follow", "A"), "A is now following you");
        assert_eq!(
            push_title("ja", "annual_report", "A"),
            "Translation missing: ja.notification_mailer.annual_report.subject"
        );
    }

    #[test]
    fn subscriptions_are_validated_as_mastodon_validates_them() {
        let (_, p256dh) = super::generate_vapid_keypair().unwrap();
        let auth = "tBHItJI5svbpez7KI4CCXg";
        assert!(subscription_errors("https://push.example/a", &p256dh, auth).is_empty());
        assert_eq!(
            subscription_errors("ftp://push.example/a", &p256dh, auth),
            ["Endpoint is invalid"]
        );
        assert_eq!(
            subscription_errors("https://push.example/a", "BNotAKey", auth),
            ["is not a valid Ed25519 or Curve25519 key"]
        );
        assert_eq!(
            subscription_errors("", "", ""),
            [
                "Endpoint can't be blank",
                "Endpoint is invalid",
                "Key p256dh can't be blank",
                "Key auth can't be blank",
                "is not a valid Ed25519 or Curve25519 key",
            ]
        );
    }

    #[test]
    fn alerts_are_cast_as_active_model_casts_booleans() {
        assert_eq!(cast_boolean(&json!(true)), Some(true));
        assert_eq!(cast_boolean(&json!("1")), Some(true));
        assert_eq!(cast_boolean(&json!("yes")), Some(true));
        assert_eq!(cast_boolean(&json!("false")), Some(false));
        assert_eq!(cast_boolean(&json!("OFF")), Some(false));
        assert_eq!(cast_boolean(&json!(0)), Some(false));
        assert_eq!(cast_boolean(&json!("")), None);
        assert_eq!(cast_boolean(&json!(null)), None);
    }

    #[test]
    fn push_bodies_are_truncated_as_rails_truncates() {
        assert_eq!(push_body("<p>Hi &amp; bye</p>"), "Hi & bye");
        assert_eq!(push_body("<p>a</p><p>b</p>"), "ab");
        let long = "a".repeat(200);
        let body = push_body(&long);
        assert_eq!(body.chars().count(), 140);
        assert!(body.ends_with("..."));
    }
}
