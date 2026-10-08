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

/// Payload sent to the push endpoint, matching Mastodon's format.
#[derive(serde::Serialize)]
struct PushPayload<'a> {
    notification_id: i64,
    notification_type: &'a str,
    icon: &'a str,
    title: &'a str,
    body: &'a str,
    preferred_locale: &'a str,
}

/// Deliver a push notification to all subscriptions registered for `recipient_id`
/// where the corresponding alert type is enabled.
/// Failures are logged and swallowed — push is best-effort.
// A push carries what the payload needs — recipient, sender, type, subject, and
// the three display fields. Threading them through a struct would name the same
// eight things one level further away.
#[allow(clippy::too_many_arguments)]
pub async fn deliver(
    state: AppState,
    recipient_id: i64,
    from_account_id: i64,
    notification_id: i64,
    notification_type: &str,
    icon: &str,
    title: &str,
    body: &str,
) {
    if let Err(e) = try_deliver(
        &state,
        recipient_id,
        from_account_id,
        notification_id,
        notification_type,
        icon,
        title,
        body,
    )
    .await
    {
        tracing::warn!(error = %e, "push delivery error");
    }
}

// A push carries what the payload needs — recipient, sender, type, subject, and
// the three display fields. Threading them through a struct would name the same
// eight things one level further away.
#[allow(clippy::too_many_arguments)]
async fn try_deliver(
    state: &AppState,
    recipient_id: i64,
    from_account_id: i64,
    notification_id: i64,
    notification_type: &str,
    icon: &str,
    title: &str,
    body: &str,
) -> anyhow::Result<()> {
    // Look up subscriptions for the recipient that have this alert type enabled.
    let (alert_key, alert_default) = match notification_type {
        "follow" | "follow_request" => ("follow", "true"),
        "favourite" => ("favourite", "true"),
        "reblog" => ("reblog", "true"),
        "mention" => ("mention", "true"),
        "poll" => ("poll", "false"),
        "status" => ("status", "false"),
        "update" => ("update", "false"),
        "quote" => ("quote", "false"),
        "quoted_update" => ("quoted_update", "false"),
        "added_to_collection" => ("added_to_collection", "false"),
        "collection_update" => ("collection_update", "false"),
        _ => return Ok(()),
    };

    // Honor the subscription's delivery policy (Mastodon
    // Web::PushSubscription#policy_allows_notification?): all / followed /
    // follower / none, based on the recipient↔sender relationship. $1 =
    // recipient, $2 = sender.
    let subs_query = format!(
        r#"SELECT wps.id, wps.endpoint, wps.key_p256dh, wps.key_auth
           FROM web_push_subscriptions wps
           JOIN oauth_access_tokens oat ON oat.id = wps.access_token_id
           JOIN users u ON u.id = oat.resource_owner_id
           WHERE u.account_id = $1
             AND oat.revoked_at IS NULL
             AND COALESCE((wps.data->'alerts'->>'{}')::boolean, {})
             AND (
               $2 = $1
               OR COALESCE(wps.data->>'policy', 'all') = 'all'
               OR (COALESCE(wps.data->>'policy','all') = 'followed'
                   AND EXISTS (SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2))
               OR (COALESCE(wps.data->>'policy','all') = 'follower'
                   AND EXISTS (SELECT 1 FROM follows WHERE account_id = $2 AND target_account_id = $1))
             )"#,
        alert_key, alert_default,
    );

    let rows = sqlx::query_as::<_, (i64, String, String, String)>(&subs_query)
        .bind(recipient_id)
        .bind(from_account_id)
        .fetch_all(&state.db)
        .await?;

    if rows.is_empty() {
        return Ok(());
    }

    let payload = serde_json::to_string(&PushPayload {
        notification_id,
        notification_type,
        icon,
        title,
        body,
        preferred_locale: "en",
    })?;

    // `Web::PushNotificationWorker.perform_async(subscription.id,
    // notification.id)` for each, with the payload as it was rendered.
    for (id, _, _, _) in rows {
        crate::jobs::perform_async(
            state,
            PushNotificationWorker {
                web_push_subscription_id: id,
                payload: payload.clone(),
            },
        )
        .await?;
    }

    Ok(())
}

/// `Web::PushNotificationWorker`: one notification, to one subscription.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PushNotificationWorker {
    pub web_push_subscription_id: i64,
    pub payload: String,
}

impl crate::jobs::Job for PushNotificationWorker {
    const KIND: &'static str = "Web::PushNotificationWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Push)
        .retry(5);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        // `Web::PushSubscription.find`, else nothing to do.
        let Some(sub) = sqlx::query!(
            "SELECT endpoint, key_p256dh, key_auth FROM web_push_subscriptions WHERE id = $1",
            self.web_push_subscription_id
        )
        .fetch_optional(&state.db)
        .await?
        else {
            return Ok(());
        };
        send_one(
            state,
            &sub.endpoint,
            &sub.key_p256dh,
            &sub.key_auth,
            &state.instance.vapid_private_key,
            &self.payload,
        )
        .await
    }
}

async fn send_one(
    state: &AppState,
    endpoint: &str,
    p256dh: &str,
    auth: &str,
    vapid_private_pem: &str,
    payload: &str,
) -> anyhow::Result<()> {
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
    builder.set_payload(ContentEncoding::AesGcm, payload.as_bytes());
    builder.set_ttl(86400);

    let sig_builder = VapidSignatureBuilder::from_pem(vapid_private_pem.as_bytes(), &sub_info)?;
    builder.set_vapid_signature(sig_builder.build()?);

    let message = builder.build()?;

    send_with_reqwest(&state.http, message).await
}

async fn send_with_reqwest(
    http: &reqwest::Client,
    message: web_push::WebPushMessage,
) -> anyhow::Result<()> {
    let endpoint = message.endpoint.to_string();
    let ttl = message.ttl;
    let mut req = http.post(endpoint.as_str()).header("TTL", ttl.to_string());

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
    if !status.is_success() && status.as_u16() != 201 {
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!(status = %status, body = %body, endpoint = %endpoint, "push relay rejected message");
    }

    Ok(())
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
) -> Option<String> {
    use crate::api::mastodon::notifications::{group_type_prefix, MAXIMUM_GROUP_SPAN_HOURS};

    let prefix = group_type_prefix(notification_type, status_id)?;
    let redis_key = redis_keys.key(format!("notif-group/{recipient_id}/{prefix}"));
    let hour = 3600;
    let mut bucket = chrono::Utc::now().timestamp() / hour;

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

#[allow(clippy::too_many_arguments)]
pub async fn create_and_push(
    state: &AppState,
    recipient_id: i64,
    from_account_id: i64,
    notification_type: &'static str,
    status_id: Option<i64>,
    title: String,
    body: String,
    icon: String,
) {
    Box::pin(notify(
        state,
        recipient_id,
        from_account_id,
        notification_type,
        status_id,
        None,
        title,
        body,
        icon,
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
    let sender = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        from_account_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let Some(sender) = sender else { return };
    // `Web::NotificationSerializer`: the title the type has, and, with no
    // post, the sender's bio as the body.
    let name = if sender.display_name.trim().is_empty() {
        sender.username.clone()
    } else {
        sender.display_name.clone()
    };
    let title = collection_push_title(notification_type, &name);
    let body = push_body(&sender.note);
    let icon = crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &sender);
    Box::pin(notify(
        state,
        recipient_id,
        from_account_id,
        notification_type,
        None,
        Some(activity),
        title,
        body,
        icon,
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

/// `truncate(strip_tags(text), length: 140)`, entities decoded.
fn push_body(html: &str) -> String {
    let text = crate::search::elasticsearch::documents::plain_text(html, false);
    let text = text.trim();
    if text.chars().count() <= 140 {
        return text.to_owned();
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
    title: String,
    body: String,
    icon: String,
) {
    let db = state.db.clone();

    // Don't notify yourself — except for the types exempt from it.
    if recipient_id == from_account_id && !SELF_NOTIFIABLE_TYPES.contains(&notification_type) {
        return;
    }

    // Local-only: Mastodon's LocalNotificationWorker only notifies local
    // accounts, so never create a notification row for a remote recipient
    // (e.g. favouriting or boosting a remote author's post).
    let recipient_local = sqlx::query_scalar!(
        r#"SELECT (domain IS NULL) AS "local!" FROM accounts WHERE id = $1"#,
        recipient_id,
    )
    .fetch_optional(&db)
    .await
    .ok()
    .flatten()
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

    // Don't notify if there is a block in either direction; a staff mention
    // passes the recipient's own block (`@recipient.blocking?(@sender)`).
    let is_blocked = sqlx::query_scalar!(
        r#"SELECT 1 FROM blocks
           WHERE (NOT $3 AND account_id = $1 AND target_account_id = $2)
              OR (account_id = $2 AND target_account_id = $1)"#,
        recipient_id,
        from_account_id,
        staff_message,
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
                         SELECT m.account_id FROM mentions m WHERE m.status_id = s.id
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
        false,
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
    let group_key = notification_group_key(
        &mut state.redis_coordination.clone(),
        &state.redis_keys,
        recipient_id,
        notification_type,
        status_id,
    )
    .await;

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

    deliver(
        state.clone(),
        recipient_id,
        from_account_id,
        notification_id,
        notification_type,
        &icon,
        &title,
        &body,
    )
    .await;

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
/// none of which is filterable, so none passes through the block, mute and
/// policy checks [`create_and_push`] makes.
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
        let notification_id = sqlx::query_scalar!(
            r#"INSERT INTO notifications (account_id, from_account_id, "type", activity_type, activity_id, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, now(), now())
               RETURNING id"#,
            recipient_id,
            from_account_id,
            notification_type,
            activity_type,
            activity_id,
        )
        .fetch_one(&state.db)
        .await?;

        crate::streaming::fan_out::notification(state, recipient_id, notification_id).await;

        let (title, body) = match notification_type {
            "admin.report" => ("New report".to_string(), String::new()),
            "admin.sign_up" => ("New sign-up".to_string(), String::new()),
            _ => ("Moderation warning".to_string(), String::new()),
        };
        let icon = sqlx::query_as!(
            crate::db::models::Account,
            "SELECT * FROM accounts WHERE id = $1",
            from_account_id,
        )
        .fetch_optional(&state.db)
        .await?
        .map(|a| crate::api::mastodon::convert::account_avatar_url_for(&state.urls, &a))
        .unwrap_or_default();
        deliver(
            state.clone(),
            recipient_id,
            from_account_id,
            notification_id,
            notification_type,
            &icon,
            &title,
            &body,
        )
        .await;
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
    use super::{collection_push_title, push_body};

    #[test]
    fn collection_pushes_say_what_happened() {
        assert_eq!(
            collection_push_title("added_to_collection", "Alice"),
            "Alice added you to a collection"
        );
        assert_eq!(
            collection_push_title("collection_update", "Alice"),
            "Alice updated a collection you are in"
        );
    }

    #[test]
    fn push_bodies_are_truncated_as_rails_truncates() {
        assert_eq!(push_body("<p>Hi &amp; bye</p>"), "Hi & bye");
        let long = "a".repeat(200);
        let body = push_body(&long);
        assert_eq!(body.chars().count(), 140);
        assert!(body.ends_with("..."));
    }
}
