//! ActivityPub activity delivery to remote inboxes.
//!
//! Who an activity goes to is eunha's: the recipient sets below are SQL over
//! Mastodon's tables. Sending it is ojak's: [`Deliverer`] queues each
//! delivery in `eunha.ojak_queue` and its loops send them, retrying with
//! backoff, draft-cavage first and RFC 9421 when an inbox refuses it, through
//! a client that refuses private and reserved addresses.
//!
//! What happens to a delivery that fails is Mastodon's
//! (`ActivityPub::DeliveryWorker`): the same statuses are given up on at
//! once, the same circuit breaker holds back an inbox that keeps failing, a
//! delivery is retried on the same schedule, and [`DeliveryFailureTracker`]
//! marks a server unavailable as Mastodon's does.

use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

use crate::federation::delivery_failures::DeliveryFailureTracker;
use crate::state::AppState;

/// Deliveries in flight across the whole process, whichever instance they
/// belong to. Sized once, before any instance starts, from the value every
/// instance agreed on; a process that never sizes it, such as a test, gets the
/// default. Every instance's deliverer holds a permit of it per delivery.
static DELIVERY_PERMITS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> =
    std::sync::OnceLock::new();

/// Size the process-wide delivery limit. Only the first call has an effect.
pub fn set_process_delivery_concurrency(permits: usize) {
    let _ = DELIVERY_PERMITS.set(Arc::new(tokio::sync::Semaphore::new(permits.max(1))));
}

fn delivery_permits() -> Arc<tokio::sync::Semaphore> {
    DELIVERY_PERMITS
        .get_or_init(|| {
            Arc::new(tokio::sync::Semaphore::new(
                crate::config::WorkersConfig::default().process_delivery_concurrency,
            ))
        })
        .clone()
}

/// How often to prune deliveries given up on.
const DELIVERY_CLEANUP_INTERVAL: Duration = Duration::from_secs(3600);

/// The table ojak keeps eunha's queue in (migrations/011_feder_queue.sql, renamed in 013_ojak_queue.sql).
pub const QUEUE_TABLE: &str = "eunha.ojak_queue";

/// The queue deliveries wait in, within [`QUEUE_TABLE`].
pub const QUEUE: &str = "delivery";

/// The queue a send to at most [`PRIORITY_MAX_INBOXES`] inboxes waits in, and
/// whose deliveries take free slots first: a direct message, a reply, a
/// follow or its answer is not held behind a post to thousands of servers.
pub const PRIORITY_QUEUE: &str = "delivery-priority";

/// How many inboxes a send may have and still go ahead of a fan-out.
pub const PRIORITY_MAX_INBOXES: usize = 8;

/// The queue `ActivityPub::LowPriorityDeliveryWorker`'s deliveries wait in,
/// Mastodon's `pull` queue: claimed only when the others have nothing due.
pub const LOW_PRIORITY_QUEUE: &str = "delivery-pull";

/// `ActivityPub::LowPriorityDeliveryWorker`'s `retry: 8`: nine attempts.
pub const LOW_PRIORITY_ATTEMPTS: u32 = 9;

/// What `ActivityPub::Forwarder` passes on goes as
/// `ActivityPub::LowPriorityDeliveryWorker`: on the `pull` queue, tried nine
/// times on the same schedule as other deliveries.
pub fn low_priority() -> ojak::deliverer::Batch {
    ojak::deliverer::Batch {
        max_attempts: Some(LOW_PRIORITY_ATTEMPTS),
        low_priority: true,
        ..ojak::deliverer::Batch::default()
    }
}

/// The tag a migrated follower's `Follow` is sent with, which says whom to
/// unfollow once it has been delivered.
pub fn migrated_follow_tag(follower_id: i64, old_target_id: i64) -> String {
    format!("migrated-follow:{follower_id}:{old_target_id}")
}

/// What follows a delivery having settled: for a migrated follower's
/// `Follow`, the unfollow of the old account
/// (`ActivityPub::MigratedFollowDeliveryWorker#unfollow_old_account!`),
/// queued as a job.
pub async fn delivery_settled(
    db: &sqlx::PgPool,
    queues: &crate::background::QueueWakes,
    settled: &ojak::deliverer::Settled,
) {
    let Some((follower, old_target)) = settled.tag.as_deref().and_then(parse_migrated_follow_tag)
    else {
        return;
    };
    let job = crate::moves::UnfollowMigratedWorker {
        source_account_id: follower,
        old_target_account_id: old_target,
    };
    match crate::jobs::perform_async_in(db, job).await {
        Ok(_) => queues.jobs.notify_one(),
        Err(error) => {
            tracing::error!(%error, "could not queue the unfollow of a migrated follow");
        }
    }
}

fn parse_migrated_follow_tag(tag: &str) -> Option<(i64, i64)> {
    let (follower, old_target) = tag.strip_prefix("migrated-follow:")?.split_once(':')?;
    Some((follower.parse().ok()?, old_target.parse().ok()?))
}

/// Whether an inbox answering `status` is answering what will not change, as
/// Mastodon's `response_error_unsalvageable?` has it: 501, and any 4xx but
/// 401, 408 and 429. A 401 is final too when the sender is deleted or
/// suspended for good (`unsalvageable_authorization_failure?`), which
/// [`SigningKeys`] answers as `gone`.
pub fn unsalvageable(status: u16) -> bool {
    status == 501 || ((400..500).contains(&status) && !matches!(status, 401 | 408 | 429))
}

/// Mastodon's circuit breaker for deliveries (`STOPLIGHT_FAILURE_THRESHOLD`
/// and `STOPLIGHT_COOL_OFF_TIME`), kept for each inbox as Mastodon keeps it,
/// in Redis ([`RedisBreakers`]), so that every process delivering for an
/// instance counts the same failures.
pub const BREAKER: ojak::deliverer::CircuitBreaker = ojak::deliverer::CircuitBreaker {
    threshold: 10,
    cool_off: Duration::from_secs(60),
    scope: ojak::deliverer::BreakerScope::Inbox,
};

/// Mastodon's retry schedule for deliveries: `retry: 16` in
/// `ActivityPub::DeliveryWorker`, so seventeen attempts in all, each failure
/// waited out as its `sidekiq_retry_in` and Sidekiq's own jitter have it. The
/// last is tried about two and a half days after the first.
pub const RETRY: ojak::queue::RetryPolicy = ojak::queue::RetryPolicy::custom(retry_in, 17);

/// How long Mastodon waits after the `failed`-th failure of a delivery, one
/// for the first. Sidekiq counts retries from nought, so `count` is one less:
/// `count**4 + 15` seconds and up to half `count**4` more, which Mastodon's
/// `sidekiq_retry_in` adds, truncated to whole seconds, and up to
/// `10 * (count + 1)` more, which Sidekiq's `delay_for` adds to anything.
pub fn retry_in(failed: u32) -> Duration {
    use rand::Rng as _;

    let count = u64::from(failed.saturating_sub(1));
    let base = count.saturating_pow(4);
    let mut rng = rand::rng();
    // `rand(0.5 * count**4)`: below one second when that is nought, as Ruby's
    // `rand(0.0)` is, and truncated with the rest by `to_i`.
    let mastodon = rng.random::<f64>() * (0.5 * base as f64).max(1.0);
    let sidekiq = rng.random_range(0..10 * (count + 1));
    Duration::from_secs(base + 15 + mastodon as u64 + sidekiq)
}

/// An instance's deliverer.
pub type Deliverer = ojak::deliverer::Deliverer<
    ojak_postgres::PostgresQueue,
    ojak::deliverer::CachingSenderKeys<SigningKeys>,
>;

/// Build an instance's deliverer, from its `[workers]` settings.
///
/// # Errors
///
/// When the HTTP client cannot be built.
#[allow(clippy::too_many_arguments)]
pub fn deliverer(
    db: sqlx::PgPool,
    encryptor: Option<crate::rails_encryption::Encryptor>,
    workers: &crate::config::WorkersConfig,
    client: ojak::client::Client,
    tracker: DeliveryFailureTracker,
    breakers: RedisBreakers,
    queues: Arc<crate::background::QueueWakes>,
    synchronization: Option<crate::federation::followers_synchronization::DigestCache>,
) -> anyhow::Result<Deliverer> {
    let queue = ojak_postgres::PostgresQueue::with_table(db.clone(), QUEUE_TABLE)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let config = ojak::deliverer::DelivererConfig {
        batch: usize::try_from(workers.delivery_batch).unwrap_or(50).max(1),
        concurrency: workers.delivery_concurrency.max(1),
        // eunha limits deliveries per process, not per host.
        per_host: workers.delivery_concurrency.max(1),
        idle_poll: Duration::from_secs(workers.queue_idle_poll_seconds.max(1)),
        shared_limit: Some(delivery_permits()),
        queue: QUEUE.to_owned(),
        priority: Some(ojak::deliverer::Priority {
            queue: PRIORITY_QUEUE.to_owned(),
            max_inboxes: PRIORITY_MAX_INBOXES,
        }),
        breaker: Some(BREAKER),
        low_priority: Some(LOW_PRIORITY_QUEUE.to_owned()),
        permanent: unsalvageable,
        permanent_if_sender_gone: |status| status == 401,
        retry: RETRY,
        // Sidekiq retries on its schedule whatever the server asked, and a
        // delivery Stoplight held is retried like any other failure.
        wait_as_asked: false,
        ..ojak::deliverer::DelivererConfig::default()
    };
    let attempts = tracker.clone();
    let settled_db = db.clone();
    let synchronization_db = db.clone();
    Ok(ojak::deliverer::Deliverer::new(
        queue,
        // A post fans out to thousands of inboxes signed with one key, and
        // loading it for each took two queries apiece: with many deliveries
        // in flight they held the instance's whole connection pool, and its
        // own requests timed out. So a key is kept for a while once loaded.
        ojak::deliverer::CachingSenderKeys::new(SigningKeys { db, encryptor }, SIGNING_KEY_TTL),
        client,
        config,
    )
    .on_failure(move |failure| {
        tracing::warn!(
            inbox = %failure.inbox,
            status = ?failure.status,
            error = %crate::error::sanitize_error_text(&failure.error),
            "gave up on a delivery"
        );
    })
    .on_attempt(move |attempt| attempts.record(attempt))
    .breaker_store(breakers)
    // `ActivityPub::MigratedFollowDeliveryWorker#unfollow_old_account!`, once
    // the `Follow` has been delivered or refused for good.
    .on_settled(move |settled| {
        let db = settled_db.clone();
        let queues = queues.clone();
        async move { delivery_settled(&db, &queues, &settled).await }
    })
    .skip_if(move |inbox, activity| {
        activity.get("type").and_then(Value::as_str) != Some("Follow")
            && tracker.is_unavailable_inbox(inbox)
    })
    // `ActivityPub::DeliveryWorker#synchronization_header`, written when the
    // delivery is made, for the account its key names on the instance whose
    // domain the key is on; none with `synchronization` off
    // (`DISABLE_FOLLOWERS_SYNCHRONIZATION`).
    .collection_synchronization(move |key_id: String, inbox: url::Url| {
        let db = synchronization_db.clone();
        let cache = synchronization.clone();
        async move {
            let cache = cache?;
            let domain = url::Url::parse(&key_id)
                .ok()?
                .host_str()
                .map(str::to_owned)?;
            let account_id = signing_account_id_in(&db, &key_id).await.ok()?;
            crate::federation::followers_synchronization::header_for(
                &db,
                &cache,
                &domain,
                account_id,
                inbox.as_str(),
            )
            .await
        }
    }))
}

/// The key a delivery is signed with, found by the key ID it was queued with:
/// loaded from the database and decrypted.
pub struct SigningKeys {
    db: sqlx::PgPool,
    encryptor: Option<crate::rails_encryption::Encryptor>,
}

/// How long a loaded signing key is used before it is loaded again, so that a
/// key replaced in the database is signed with soon after.
const SIGNING_KEY_TTL: Duration = Duration::from_secs(300);

impl ojak::deliverer::SenderKeys for SigningKeys {
    async fn key(
        &self,
        key_id: &str,
    ) -> Result<Option<ojak::sig::SenderKey>, ojak::queue::QueueError> {
        let transient = |e: anyhow::Error| ojak::queue::QueueError(e.to_string());
        let account_id = match signing_account_id_in(&self.db, key_id).await {
            Ok(id) => id,
            // A database that did not answer is worth another try; an account
            // that is gone is not.
            Err(e) if e.downcast_ref::<sqlx::Error>().is_some() => return Err(transient(e)),
            Err(_) => return Ok(None),
        };
        let pem = match crate::federation::keypair::signing_key_in(
            &self.db,
            self.encryptor.as_ref(),
            account_id,
        )
        .await
        {
            Ok(key) => key.private_key,
            Err(e) if e.downcast_ref::<sqlx::Error>().is_some() => return Err(transient(e)),
            Err(e) => {
                tracing::error!(key_id, error = %e, "no usable signing key; the delivery fails");
                return Ok(None);
            }
        };
        match ojak::sig::PrivateKey::from_pem(&pem) {
            Ok(private_key) => Ok(Some(ojak::sig::SenderKey {
                key_id: key_id.to_owned(),
                private_key: Arc::new(private_key),
            })),
            Err(e) => {
                tracing::error!(key_id, error = %e, "signing key does not parse; the delivery fails");
                Ok(None)
            }
        }
    }

    async fn gone(&self, key_id: &str) -> bool {
        sender_gone(&self.db, key_id).await
    }
}

/// Whether the account signing with `key_id` is gone for good:
/// `permanently_unavailable?`, deleted or suspended (the instance actor never
/// is) with no deletion request left to undo it.
pub async fn sender_gone(db: &sqlx::PgPool, key_id: &str) -> bool {
    let Ok(account_id) = signing_account_id_in(db, key_id).await else {
        return false;
    };
    sqlx::query_scalar!(
        r#"SELECT (a.suspended_at IS NOT NULL OR a.requested_deletion_at IS NOT NULL)
                      AND a.id <> $2
                      AND NOT EXISTS (SELECT 1 FROM account_deletion_requests r
                                      WHERE r.account_id = a.id) AS "gone!"
               FROM accounts a WHERE a.id = $1"#,
        account_id,
        crate::federation::instance_actor::INSTANCE_ACTOR_ID,
    )
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// Fetch the set of domains currently marked unavailable.
async fn unavailable_domains(state: &AppState) -> std::collections::HashSet<String> {
    sqlx::query_scalar!("SELECT domain FROM unavailable_domains")
        .fetch_all(&state.db)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect()
}

/// True if `inbox_url`'s host is in `unavailable`.
fn inbox_unavailable(inbox_url: &str, unavailable: &std::collections::HashSet<String>) -> bool {
    url::Url::parse(inbox_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .map(|h| unavailable.contains(&h))
        .unwrap_or(false)
}

/// Fan out an activity to all remote follower inboxes for `actor_account_id`.
pub async fn fanout_to_followers(
    state: &AppState,
    activity: Value,
    actor_account_id: i64,
    key_id: String,
) -> anyhow::Result<u64> {
    let inboxes = follower_inboxes(state, actor_account_id).await?;
    enqueue_to_inboxes(state, activity, inboxes, key_id).await
}

/// [`fanout_to_followers`], with a Linked Data Signature when `linked_data`
/// says Mastodon would make one.
pub async fn fanout_to_followers_signed(
    state: &AppState,
    activity: Value,
    actor_account_id: i64,
    key_id: String,
    linked_data: LinkedData,
) -> anyhow::Result<u64> {
    let inboxes = follower_inboxes(state, actor_account_id).await?;
    enqueue(state, activity, inboxes, key_id, true, linked_data, None).await
}

/// Forward another server's `activity` to the remote followers of
/// `account_id` but for the inboxes in `exclude`, signed by that account,
/// as Mastodon forwards a reply to a local post to its author's followers
/// (`ActivityPub::RawDistributionWorker`). It goes as it arrived: a proof of
/// ours on someone else's activity would say nothing true, and would replace
/// the author's.
pub async fn forward_to_followers(
    state: &AppState,
    activity: Value,
    account_id: i64,
    key_id: String,
    exclude: &[String],
) -> anyhow::Result<u64> {
    let mut inboxes = follower_inboxes(state, account_id).await?;
    inboxes.retain(|inbox| !exclude.contains(inbox));
    enqueue(
        state,
        activity,
        inboxes,
        key_id,
        false,
        LinkedData::Unsigned,
        None,
    )
    .await
}

/// Pass another server's `activity` on to `inboxes`, signed by `key_id`'s
/// account, as it arrived (`ActivityPub::Forwarder`): no proof or Linked
/// Data Signature of ours goes on someone else's activity. Unavailable
/// domains are left out, as for any delivery.
pub async fn forward_to_inboxes(
    state: &AppState,
    activity: Value,
    inboxes: Vec<String>,
    key_id: String,
) -> anyhow::Result<u64> {
    let unavailable = unavailable_domains(state).await;
    let inboxes = inboxes
        .into_iter()
        .filter(|inbox| !inbox_unavailable(inbox, &unavailable))
        .collect();
    enqueue(
        state,
        activity,
        inboxes,
        key_id,
        false,
        LinkedData::Unsigned,
        Some(&low_priority()),
    )
    .await
}

/// Send `activity` to the remote followers of `account_id`, signed with
/// `key_id`, without an integrity proof: for an activity signed as an
/// identity whose actor is no longer served, where a proof would name a key
/// nobody can fetch.
pub async fn fanout_to_followers_unproven(
    state: &AppState,
    activity: Value,
    account_id: i64,
    key_id: String,
    batch: Option<&ojak::deliverer::Batch>,
) -> anyhow::Result<u64> {
    let inboxes = follower_inboxes(state, account_id).await?;
    enqueue(
        state,
        activity,
        inboxes,
        key_id,
        false,
        LinkedData::Unsigned,
        batch,
    )
    .await
}

/// The inboxes of `actor_account_id`'s remote followers, a shared inbox once
/// for all the followers behind it, without unavailable domains.
pub async fn follower_inboxes(
    state: &AppState,
    actor_account_id: i64,
) -> anyhow::Result<Vec<String>> {
    let inboxes = sqlx::query!(
        r#"SELECT DISTINCT
             CASE WHEN a.shared_inbox_url IS NOT NULL AND a.shared_inbox_url <> ''
                  THEN a.shared_inbox_url
                  ELSE a.inbox_url
             END AS inbox
           FROM follows f
           JOIN accounts a ON a.id = f.account_id
           WHERE f.target_account_id = $1
             AND a.domain IS NOT NULL
             AND a.inbox_url <> ''
             AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL"#,
        actor_account_id,
    )
    .fetch_all(&state.db)
    .await?;

    let unavailable = unavailable_domains(state).await;
    let inboxes = inboxes
        .into_iter()
        .filter_map(|row| row.inbox)
        .filter(|inbox| !inbox.is_empty())
        .filter(|inbox| {
            if inbox_unavailable(inbox, &unavailable) {
                tracing::debug!(inbox, "skipping delivery to unavailable domain");
                false
            } else {
                true
            }
        })
        .collect();
    Ok(inboxes)
}

/// Compute the set of remote inboxes that should receive an account-level
/// activity (a profile `Update`), matching Mastodon's `AccountReachFinder`:
/// followers + reporters + accounts mentioned in the account's recent statuses +
/// accounts it recently followed + targets of its recent follow requests +
/// enabled relays, de-duplicated and minus suspended/unavailable domains.
///
/// "Recent" is the last two days, mirroring Mastodon's `STATUS_SINCE`.
pub async fn account_reach_inboxes(
    state: &AppState,
    account_id: i64,
) -> anyhow::Result<Vec<String>> {
    let rows: Vec<String> = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT inbox AS "inbox!" FROM (
            -- followers
            SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END AS inbox
            FROM follows f JOIN accounts a ON a.id = f.account_id
            WHERE f.target_account_id = $1 AND a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.inbox_url <> ''
            UNION
            -- reporters (accounts that reported this account)
            SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END
            FROM reports r JOIN accounts a ON a.id = r.account_id
            WHERE r.target_account_id = $1 AND a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.inbox_url <> ''
            UNION
            -- accounts mentioned in this account's recent statuses
            SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END
            FROM mentions m JOIN accounts a ON a.id = m.account_id JOIN statuses s ON s.id = m.status_id
            WHERE s.account_id = $1 AND s.deleted_at IS NULL AND s.created_at >= now() - interval '2 days'
              AND a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.inbox_url <> ''
            UNION
            -- accounts this account recently followed
            SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END
            FROM follows f JOIN accounts a ON a.id = f.target_account_id
            WHERE f.account_id = $1 AND f.created_at >= now() - interval '2 days'
              AND a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.inbox_url <> ''
            UNION
            -- targets of this account's recent follow requests
            SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END
            FROM follow_requests fr JOIN accounts a ON a.id = fr.target_account_id
            WHERE fr.account_id = $1 AND fr.created_at >= now() - interval '2 days'
              AND a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.inbox_url <> ''
            UNION
            -- enabled relays
            SELECT inbox_url FROM relays WHERE state = 2 AND inbox_url <> ''
        ) reach
        WHERE inbox <> ''
        "#,
        account_id,
    )
    .fetch_all(&state.db)
    .await?;

    let unavailable = unavailable_domains(state).await;
    Ok(rows
        .into_iter()
        .filter(|i| !i.is_empty())
        .filter(|i| !inbox_unavailable(i, &unavailable))
        .collect())
}

/// Compute the full set of remote inboxes that should receive a status-level
/// activity (Update/Delete), matching Mastodon's `StatusReachFinder#inboxes`:
/// followers + mentioned accounts + the replied-to author + the quoted author +
/// interactors (rebloggers/repliers/favouriters/quoters) + relays (public),
/// de-duplicated and minus suspended accounts and unavailable domains.
///
/// - `distributable`: status is public or unlisted (gates the replied-to author,
///   the thread-followers union, and — together with `unsafe_reach` — interactors).
/// - `unsafe_reach`: include interactors regardless of visibility (Delete uses this).
/// - `is_public`: also reach enabled relays.
/// - `followers_allowed`: include the author's followers (false for direct/limited).
/// - `reblog_of_account_id`: when set, the status is a reblog — Mastodon's
///   `StatusReachFinder` then reaches only the original author (plus followers +
///   relays), skipping the mention/quote/interactor unions.
/// - `extra_account_ids`: additional accounts to reach unconditionally. The
///   Delete path passes the rebloggers whose reblogs were just cascade-deleted:
///   the interactor union can no longer see them (their reblog rows are now
///   soft-deleted), mirroring Mastodon's `reblogs.rewhere(deleted_at: [nil,
///   @status.deleted_at])`, which keeps reaching reblogs removed alongside it.
#[allow(clippy::too_many_arguments)]
pub async fn status_reach_inboxes(
    state: &AppState,
    status_id: i64,
    author_id: i64,
    in_reply_to_account_id: Option<i64>,
    distributable: bool,
    unsafe_reach: bool,
    is_public: bool,
    followers_allowed: bool,
    reblog_of_account_id: Option<i64>,
    extra_account_ids: &[i64],
) -> anyhow::Result<Vec<String>> {
    let rows: Vec<String> = sqlx::query_scalar!(
        r#"
        SELECT DISTINCT inbox AS "inbox!" FROM (
            -- mentioned accounts (non-reblog statuses only)
            SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END AS inbox
            FROM mentions m JOIN accounts a ON a.id = m.account_id
            WHERE $8::bigint IS NULL AND m.status_id = $1 AND a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.inbox_url <> ''
            UNION
            -- replied-to author (distributable only)
            SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END
            FROM accounts a
            WHERE $8::bigint IS NULL AND $4::bool AND a.id = $3 AND a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.inbox_url <> ''
            UNION
            -- quoted author
            SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END
            FROM quotes q JOIN accounts a ON a.id = q.quoted_account_id
            WHERE $8::bigint IS NULL AND q.status_id = $1 AND a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.inbox_url <> ''
            UNION
            -- interactors (distributable or unsafe)
            SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END
            FROM accounts a
            WHERE $8::bigint IS NULL AND ($4::bool OR $5::bool) AND a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.inbox_url <> ''
              AND a.id IN (
                SELECT account_id FROM statuses WHERE reblog_of_id = $1 AND deleted_at IS NULL
                UNION SELECT account_id FROM statuses WHERE in_reply_to_id = $1 AND deleted_at IS NULL
                UNION SELECT account_id FROM favourites WHERE status_id = $1
                UNION SELECT account_id FROM quotes WHERE quoted_status_id = $1
              )
            UNION
            -- reblog: the original author
            SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END
            FROM accounts a
            WHERE a.id = $8 AND a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.inbox_url <> ''
            UNION
            -- followers (author's; plus a local thread author's followers for distributable replies)
            SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END
            FROM accounts a
            WHERE $7::bool AND a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.inbox_url <> ''
              AND (
                EXISTS (SELECT 1 FROM follows f WHERE f.account_id = a.id AND f.target_account_id = $2)
                OR (
                  $4::bool AND $3 IS NOT NULL
                  AND EXISTS (SELECT 1 FROM accounts ta WHERE ta.id = $3 AND ta.domain IS NULL)
                  AND EXISTS (SELECT 1 FROM follows f WHERE f.account_id = a.id AND f.target_account_id = $3)
                  AND a.domain NOT IN (SELECT domain FROM account_domain_blocks WHERE account_id = $2)
                )
              )
            UNION
            -- explicitly supplied extra accounts (e.g. rebloggers whose reblogs
            -- were cascade-deleted with this status, so the interactor union
            -- above no longer sees them)
            SELECT CASE WHEN a.shared_inbox_url <> '' THEN a.shared_inbox_url ELSE a.inbox_url END
            FROM accounts a
            WHERE a.id = ANY($9::bigint[]) AND a.domain IS NOT NULL AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL AND a.inbox_url <> ''
            UNION
            -- relays (public only)
            SELECT inbox_url FROM relays WHERE $6::bool AND state = 2 AND inbox_url <> ''
        ) reach
        WHERE inbox <> ''
        "#,
        status_id,
        author_id,
        in_reply_to_account_id,
        distributable,
        unsafe_reach,
        is_public,
        followers_allowed,
        reblog_of_account_id,
        extra_account_ids,
    )
    .fetch_all(&state.db)
    .await?;

    let unavailable = unavailable_domains(state).await;
    Ok(rows
        .into_iter()
        .filter(|i| !i.is_empty())
        .filter(|i| !inbox_unavailable(i, &unavailable))
        .collect())
}

/// `StatusReachFinder.new(status, unsafe:).inboxes` for the status with
/// `status_id`, read from its row, deleted or not: a deleted status still
/// reaches the boosters whose boosts went with it, as
/// `reblogs.rewhere(deleted_at: [nil, @status.deleted_at])` does.
pub async fn status_reach_of(
    state: &AppState,
    status_id: i64,
    unsafe_reach: bool,
) -> anyhow::Result<Vec<String>> {
    use crate::db::models::vis;
    let Some(status) = sqlx::query!(
        r#"SELECT s.account_id, s.in_reply_to_account_id, s.visibility, s.deleted_at,
                  o.account_id AS "reblog_of_account_id?"
           FROM statuses s LEFT JOIN statuses o ON o.id = s.reblog_of_id
           WHERE s.id = $1"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(Vec::new());
    };
    let deleted_boosters: Vec<i64> = match status.deleted_at {
        Some(deleted_at) => {
            sqlx::query_scalar!(
                "SELECT account_id FROM statuses WHERE reblog_of_id = $1 AND deleted_at = $2",
                status_id,
                deleted_at,
            )
            .fetch_all(&state.db)
            .await?
        }
        None => Vec::new(),
    };
    status_reach_inboxes(
        state,
        status_id,
        status.account_id,
        status.in_reply_to_account_id,
        matches!(status.visibility, vis::PUBLIC | vis::UNLISTED),
        unsafe_reach,
        status.visibility == vis::PUBLIC,
        matches!(
            status.visibility,
            vis::PUBLIC | vis::UNLISTED | vis::PRIVATE
        ),
        status.reblog_of_account_id,
        &deleted_boosters,
    )
    .await
}

/// Deliver to a specific set of inboxes (for mentions, DMs, consent replies).
pub async fn deliver_to_inboxes(
    state: &AppState,
    activity: Value,
    inboxes: Vec<String>,
    key_id: String,
) -> anyhow::Result<u64> {
    enqueue_to_inboxes(state, activity, inboxes, key_id).await
}

/// [`deliver_to_inboxes`], with a Linked Data Signature when `linked_data`
/// says Mastodon would make one.
pub async fn deliver_to_inboxes_signed(
    state: &AppState,
    activity: Value,
    inboxes: Vec<String>,
    key_id: String,
    linked_data: LinkedData,
) -> anyhow::Result<u64> {
    enqueue(state, activity, inboxes, key_id, true, linked_data, None).await
}

/// A status's `Create`, `Update` or `Announce`, as
/// `ActivityPub::DistributionWorker` sends it: [`deliver_to_inboxes_signed`],
/// with a `Collection-Synchronization` header on each delivery when
/// `synchronize_followers`.
pub async fn deliver_status_to_inboxes(
    state: &AppState,
    activity: Value,
    inboxes: Vec<String>,
    key_id: String,
    linked_data: LinkedData,
    synchronize_followers: bool,
) -> anyhow::Result<u64> {
    let batch = ojak::deliverer::Batch {
        synchronize_collection: synchronize_followers,
        ..ojak::deliverer::Batch::default()
    };
    enqueue(
        state,
        activity,
        inboxes,
        key_id,
        true,
        linked_data,
        Some(&batch),
    )
    .await
}

/// [`deliver_to_inboxes`], in `batch`.
pub async fn deliver_to_inboxes_tagged(
    state: &AppState,
    activity: Value,
    inboxes: Vec<String>,
    key_id: String,
    batch: &ojak::deliverer::Batch,
) -> anyhow::Result<u64> {
    enqueue(
        state,
        activity,
        inboxes,
        key_id,
        true,
        LinkedData::Unsigned,
        Some(batch),
    )
    .await
}

/// [`deliver_to_inboxes_signed`], in `batch`: tagged, and given up on at its
/// deadline, so that a batch of many accounts finishes.
pub async fn deliver_to_inboxes_in_batch(
    state: &AppState,
    activity: Value,
    inboxes: Vec<String>,
    key_id: String,
    linked_data: LinkedData,
    batch: &ojak::deliverer::Batch,
) -> anyhow::Result<u64> {
    enqueue(
        state,
        activity,
        inboxes,
        key_id,
        true,
        linked_data,
        Some(batch),
    )
    .await
}

/// Whether an activity goes with a Linked Data Signature (`RsaSignature2017`)
/// by its actor, which is what lets a relay, or a server forwarding it, pass
/// it on to servers that still believe it is the actor's.
///
/// Mastodon signs a payload whose record answers `sign?` when it is given a
/// signer (`Payloadable#serialize_payload`): a public or unlisted status's
/// `Create`, `Update` and `Announce`, an account's `Update`, and so on, and
/// not in authorized fetch mode, unless the payload is to be signed whatever
/// the mode (`always_sign`), as a status's `Delete` and an account's are.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LinkedData {
    /// Not signed: a follow, a like, a direct message.
    #[default]
    Unsigned,
    /// Signed, unless the instance is in authorized fetch mode.
    UnlessAuthorizedFetch,
    /// Signed whatever the mode (`always_sign`).
    Always,
}

impl LinkedData {
    /// For a status's activity: `Status#sign?` is `distributable?`, true of
    /// a public or unlisted status only.
    pub fn for_status(distributable: bool, signed: LinkedData) -> Self {
        if distributable {
            signed
        } else {
            LinkedData::Unsigned
        }
    }
}

/// Resolve the local signing account id from a `key_id` of the form
/// `{actor_url}#main-key`. The actor URL follows the account's id_scheme — either
/// `https://{domain}/users/{username}` or `https://{domain}/ap/users/{id}`
/// (see [`crate::federation::tag`]) — so match on whichever path shape it is
/// rather than the (empty for Mastodon imports) `accounts.uri` column.
async fn signing_account_id(state: &AppState, key_id: &str) -> anyhow::Result<i64> {
    signing_account_id_in(&state.db, key_id).await
}

async fn signing_account_id_in(db: &sqlx::PgPool, key_id: &str) -> anyhow::Result<i64> {
    let actor_url = key_id.split('#').next().unwrap_or(key_id);
    let url = url::Url::parse(actor_url)
        .map_err(|e| anyhow::anyhow!("invalid keyId actor URL {actor_url:?}: {e}"))?;
    let segments: Vec<&str> = url.path_segments().map(|s| s.collect()).unwrap_or_default();

    let id = match segments.as_slice() {
        // The instance actor (`https://{domain}/actor`) signs server-level
        // activities such as a Reject of a follow targeting it.
        ["actor"] => {
            sqlx::query_scalar!(
                "SELECT id FROM accounts WHERE id = $1",
                crate::federation::instance_actor::INSTANCE_ACTOR_ID,
            )
            .fetch_optional(db)
            .await?
        }
        ["ap", "users", id] => {
            let id: i64 = id
                .parse()
                .map_err(|_| anyhow::anyhow!("non-numeric ap account id in {actor_url:?}"))?;
            sqlx::query_scalar!(
                "SELECT id FROM accounts WHERE domain IS NULL AND id = $1",
                id,
            )
            .fetch_optional(db)
            .await?
        }
        ["users", username] => {
            sqlx::query_scalar!(
                "SELECT id FROM accounts
                 WHERE domain IS NULL AND lower(username) = lower($1) LIMIT 1",
                username,
            )
            .fetch_optional(db)
            .await?
        }
        _ => None,
    };
    id.ok_or_else(|| anyhow::anyhow!("no local signing account for key_id {key_id:?}"))
}

async fn enqueue_to_inboxes(
    state: &AppState,
    activity: Value,
    inboxes: Vec<String>,
    key_id: String,
) -> anyhow::Result<u64> {
    enqueue(
        state,
        activity,
        inboxes,
        key_id,
        true,
        LinkedData::Unsigned,
        None,
    )
    .await
}

/// Queue `activity` for `inboxes`, signed by `key_id`'s account, with that
/// account's integrity proof when `prove`, and its Linked Data Signature as
/// `linked_data` says.
async fn enqueue(
    state: &AppState,
    activity: Value,
    inboxes: Vec<String>,
    key_id: String,
    prove: bool,
    linked_data: LinkedData,
    batch: Option<&ojak::deliverer::Batch>,
) -> anyhow::Result<u64> {
    // Record the signing account, not its private key: the key is loaded from
    // `accounts` at send time so the secret lives in exactly one place.
    let actor_account_id = signing_account_id(state, &key_id).await?;

    // Never deliver to domains we've defederated (admin domain block at suspend
    // severity), even for direct sends like consent replies and mentions.
    let suspended = crate::federation::moderation::suspended_domains(state).await;
    let mut inboxes = inboxes
        .into_iter()
        .filter(|s| !s.is_empty())
        .filter(|inbox| {
            if crate::federation::moderation::inbox_suspended(inbox, &suspended) {
                tracing::debug!(inbox, "skipping delivery to suspended domain");
                false
            } else {
                true
            }
        })
        .collect::<Vec<_>>();
    inboxes.sort();
    inboxes.dedup();

    if inboxes.is_empty() {
        return Ok(0);
    }

    // Sign once, before the fan-out: every inbox receives the same bytes, and
    // the proof travels with the activity rather than with the connection.
    let activity = if prove {
        attach_integrity_proof(state, activity, actor_account_id, &key_id).await
    } else {
        activity
    };
    // The Linked Data Signature after the proof, as Fedify orders them: it
    // covers the proof, and a proof's verifier leaves `signature` out, as
    // Mastodon's and ojak's do.
    let activity =
        attach_linked_data_signature(state, activity, actor_account_id, &key_id, linked_data).await;

    // One statement for the whole fan-out: a per-inbox INSERT costs a
    // round-trip per follower, which for a large account is the dominant cost
    // of posting, on the request path. The sender is the key ID, which is how
    // the deliverer finds the signing key when it sends.
    let urls: Vec<url::Url> = inboxes
        .iter()
        .filter_map(|inbox| match url::Url::parse(inbox) {
            Ok(url) => Some(url),
            Err(e) => {
                tracing::debug!(inbox, error = %e, "skipping an inbox that is not a URL");
                None
            }
        })
        .collect();
    let queued = urls.len() as u64;
    state
        .deliverer
        .send_batch(
            &key_id,
            &activity,
            urls,
            batch.unwrap_or(&Default::default()),
        )
        .await
        .map_err(|e| anyhow::anyhow!("queueing deliveries: {e}"))?;

    Ok(queued)
}

/// Attach a FEP-8b32 integrity proof to an outgoing activity.
///
/// An HTTP Signature authenticates the connection an activity arrived over; a
/// proof authenticates the activity itself, so a relayed or forwarded copy can
/// still be attributed to its author. Mastodon 4.7 verifies these — it does not
/// produce them — and Fedify-based servers both produce and verify them.
///
/// Best-effort by design: an activity that cannot be signed is delivered
/// unsigned rather than not delivered, because the HTTP Signature is what peers
/// actually require. Instances with no encryption keys configured cannot store
/// an assertion key at all, and simply never attach one.
async fn attach_integrity_proof(
    state: &AppState,
    activity: Value,
    account_id: i64,
    key_id: &str,
) -> Value {
    if !state.config.sign_integrity_proofs || state.encryptor.is_none() {
        return activity;
    }
    if !activity.is_object() || activity.get("proof").is_some() {
        return activity;
    }

    let key = match crate::federation::keypair::assertion_key(state, account_id).await {
        Ok(key) => key,
        Err(e) => {
            tracing::debug!(account_id, error = %e, "no assertion key; delivering without a proof");
            return activity;
        }
    };

    // The proof's `verificationMethod` is the actor's, which the key id names
    // up to its fragment.
    let actor_url = key_id.split('#').next().unwrap_or(key_id);
    let verification_method = format!(
        "{actor_url}{}",
        crate::federation::keypair::ED25519_FRAGMENT
    );

    // A proof is only meaningful to a JSON-LD reader if `proof` is defined, so
    // the document has to carry the data integrity context before it is signed
    // — the signature covers `@context` too.
    let with_context = match ojak::sig::integrity::with_data_integrity_context(activity.clone()) {
        Some(document) => document,
        None => return activity,
    };

    match ojak::sig::integrity::sign_object_integrity_proof(
        &with_context,
        &verification_method,
        &key.seed,
        chrono::Utc::now().timestamp(),
    ) {
        Ok(signed) => signed,
        Err(e) => {
            tracing::warn!(account_id, error = %e, "could not sign an integrity proof");
            activity
        }
    }
}

/// The contexts Linked Data Signatures are made over: ojak's bundled ones, so
/// that signing an activity fetches nothing.
static JSON_LD_CONTEXTS: std::sync::LazyLock<ojak_jsonld::Registry> =
    std::sync::LazyLock::new(ojak_jsonld::Registry::bundled);

/// Sign an outgoing activity with its actor's `RsaSignature2017`, as
/// `Payloadable#serialize_payload` has `ActivityPub::LinkedDataSignature`
/// sign it, when `linked_data` says Mastodon would: `created` now,
/// `expires` in two days, the security context added to `@context`.
///
/// Best-effort, as Mastodon's own is in effect: an activity that cannot be
/// signed — its account has no usable key, or it names a context ojak does
/// not ship — goes without, since the HTTP Signature is what its recipients
/// require; only a server passing it on loses by it.
async fn attach_linked_data_signature(
    state: &AppState,
    activity: Value,
    account_id: i64,
    key_id: &str,
    linked_data: LinkedData,
) -> Value {
    match linked_data {
        LinkedData::Unsigned => return activity,
        // `Payloadable#signing_enabled?`.
        LinkedData::UnlessAuthorizedFetch
            if crate::settings::authorized_fetch_mode(state).await =>
        {
            return activity;
        }
        _ => {}
    }
    let key = match crate::federation::keypair::signing_key(state, account_id).await {
        Ok(key) => key,
        Err(e) => {
            tracing::debug!(account_id, error = %e, "no signing key; delivering without a Linked Data Signature");
            return activity;
        }
    };
    sign_linked_data_with(activity, key_id, &key.private_key)
}

/// An activity with an `RsaSignature2017` by `private_key`, named `key_id`,
/// as `serialize_payload(…, sign_with:)` makes one with a key other than the
/// account's own: what a key rotation's `Update` is signed with, the old key
/// the receiving servers still hold. The activity goes unsigned when the key
/// cannot sign it.
pub fn sign_linked_data_with(activity: Value, key_id: &str, private_key: &str) -> Value {
    let private_key = match ojak::sig::PrivateKey::from_pem(private_key) {
        Ok(key) => key,
        Err(e) => {
            tracing::warn!(key_id, error = %e, "unreadable signing key; delivering without a Linked Data Signature");
            return activity;
        }
    };
    let now = chrono::Utc::now().timestamp();
    match ojak::sig::linked_data::sign(
        &JSON_LD_CONTEXTS,
        &activity,
        key_id,
        &private_key,
        now,
        now + ojak::sig::linked_data::DEFAULT_LIFETIME_SECONDS,
    ) {
        Ok(signed) => signed,
        Err(e) => {
            tracing::warn!(key_id, error = %e, "could not make a Linked Data Signature");
            activity
        }
    }
}

/// Periodically delete deliveries given up on more than a week ago, whose
/// `last_error` is kept that long for debugging. Deliveries that went through
/// are deleted as they go.
pub async fn run_delivery_cleanup(state: AppState) {
    while !state.stop.is_cancelled() {
        match state
            .deliverer
            .queue()
            .prune_failed(Duration::from_secs(7 * 24 * 3600))
            .await
        {
            Ok(0) => {}
            Ok(n) => tracing::info!(deleted = n, "pruned failed deliveries"),
            Err(e) => tracing::error!(error = %e, "delivery cleanup failed"),
        }
        crate::background::rest(&state.stop, DELIVERY_CLEANUP_INTERVAL).await;
    }
}

/// Mastodon's Stoplight for an inbox, kept in Redis as
/// `Stoplight::DataStore::Redis` keeps it ([`ojak_redis::RedisBreakers`]), so
/// that every process delivering for the instance holds back the same inboxes
/// and lets one probe through between them: the failures in a row
/// (`stoplight:<inbox>:failures`), when an open breaker turns half-open
/// (`stoplight:<inbox>:recovery_after`, in milliseconds), and the lock its
/// probe holds (`stoplight:<inbox>:probe`), under the instance's key prefix
/// on the coordination Redis. Each lasts a week after it last changed, as
/// Stoplight's metadata does. A breaker Redis cannot be asked about lets the
/// delivery through. A probe's lock is released with the pooled Redis's
/// `eunha_compare_delete` function when the keyspace is shared, as every
/// other eunha lock is.
#[derive(Clone)]
pub struct RedisBreakers(ojak_redis::RedisBreakers<redis::aio::ConnectionManager>);

impl RedisBreakers {
    pub fn new(
        redis: redis::aio::ConnectionManager,
        keys: crate::redis_keys::RedisKeyspace,
    ) -> Self {
        let release = if keys.is_shared() {
            ojak_redis::ProbeRelease::Function("eunha_compare_delete".into())
        } else {
            ojak_redis::ProbeRelease::Eval
        };
        Self(ojak_redis::RedisBreakers::new(redis, keys.key("")).release_with(release))
    }
}

impl std::ops::Deref for RedisBreakers {
    type Target = ojak_redis::RedisBreakers<redis::aio::ConnectionManager>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ojak::deliverer::BreakerStore for RedisBreakers {
    fn admit<'a>(
        &'a self,
        breaker: &'a ojak::deliverer::CircuitBreaker,
        key: &'a str,
    ) -> ojak::deliverer::BreakerFuture<'a, ojak::deliverer::Admission> {
        Box::pin(self.0.admit(breaker, key))
    }

    fn record<'a>(
        &'a self,
        breaker: &'a ojak::deliverer::CircuitBreaker,
        key: &'a str,
        failed: bool,
        probe: Option<ojak::deliverer::Probe>,
    ) -> ojak::deliverer::BreakerFuture<'a, ()> {
        Box::pin(async move {
            if let Err(error) = self.0.record(breaker, key, failed, probe).await {
                tracing::warn!(%error, "could not record a delivery in its circuit breaker");
            }
        })
    }
}
