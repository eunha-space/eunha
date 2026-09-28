//! ActivityPub activity delivery to remote inboxes.
//!
//! Who an activity goes to is eunha's: the recipient sets below are SQL over
//! Mastodon's tables. Sending it is ojak's: [`Deliverer`] queues each
//! delivery in `eunha.ojak_queue` and its loops send them, retrying with
//! backoff, draft-cavage first and RFC 9421 when an inbox refuses it, through
//! a client that refuses private and reserved addresses.

use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

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

/// An instance's deliverer.
pub type Deliverer = ojak::deliverer::Deliverer<ojak_postgres::PostgresQueue, SigningKeys>;

/// Build an instance's deliverer, from its `[workers]` settings.
///
/// # Errors
///
/// When the HTTP client cannot be built.
pub fn deliverer(
    db: sqlx::PgPool,
    encryptor: Option<crate::rails_encryption::Encryptor>,
    workers: &crate::config::WorkersConfig,
    client: ojak::client::Client,
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
        ..ojak::deliverer::DelivererConfig::default()
    };
    let unavailable_db = db.clone();
    Ok(ojak::deliverer::Deliverer::new(
        queue,
        SigningKeys {
            db,
            encryptor,
            parsed: Default::default(),
        },
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
        // 410 Gone is a definitive signal the inbox no longer exists, so stop
        // delivering to its domain.
        if failure.status == Some(410) {
            if let Some(domain) = failure.inbox.host_str().map(str::to_owned) {
                let db = unavailable_db.clone();
                crate::tenants::spawn(async move { mark_domain_unavailable(&db, &domain).await });
            }
        }
    }))
}

/// The key a delivery is signed with, found by the key ID it was queued with.
pub struct SigningKeys {
    db: sqlx::PgPool,
    encryptor: Option<crate::rails_encryption::Encryptor>,
    /// Keys already loaded, decrypted and parsed, by key ID. A post fans out
    /// to thousands of inboxes signed with one key, and loading it for each
    /// took two queries apiece: with many deliveries in flight they held the
    /// instance's whole connection pool, and its own requests timed out.
    parsed: std::sync::Mutex<
        std::collections::HashMap<String, (std::time::Instant, ojak::delivery::SenderKey)>,
    >,
}

/// How long a loaded signing key is used before it is loaded again, so that a
/// key replaced in the database is signed with soon after.
const SIGNING_KEY_TTL: Duration = Duration::from_secs(300);

impl ojak::deliverer::SenderKeys for SigningKeys {
    async fn key(
        &self,
        key_id: &str,
    ) -> Result<Option<ojak::delivery::SenderKey>, ojak::queue::QueueError> {
        if let Some((loaded, key)) = self.parsed.lock().expect("signing keys").get(key_id) {
            if loaded.elapsed() < SIGNING_KEY_TTL {
                return Ok(Some(key.clone()));
            }
        }
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
        match ojak::delivery::PrivateKey::from_pem(&pem) {
            Ok(private_key) => {
                let key = ojak::delivery::SenderKey {
                    key_id: key_id.to_owned(),
                    private_key: Arc::new(private_key),
                };
                let mut parsed = self.parsed.lock().expect("signing keys");
                // Only the keys signing now are worth keeping.
                parsed.retain(|_, (loaded, _)| loaded.elapsed() < SIGNING_KEY_TTL);
                parsed.insert(key_id.to_owned(), (std::time::Instant::now(), key.clone()));
                Ok(Some(key))
            }
            Err(e) => {
                tracing::error!(key_id, error = %e, "signing key does not parse; the delivery fails");
                Ok(None)
            }
        }
    }
}

/// Deliver an activity to a single remote inbox, signed with the given key,
/// once and not through the queue.
pub async fn deliver(
    http: &reqwest::Client,
    activity: &Value,
    inbox_url: &str,
    key_id: &str,
    private_key_pem: &str,
) -> anyhow::Result<()> {
    let body = serde_json::to_vec(activity)?;
    tracing::debug!(inbox = inbox_url, "delivering ActivityPub activity");
    ojak_runtime::delivery::deliver(http, &body, inbox_url, key_id, private_key_pem).await
}

/// Record a domain as unavailable so future fan-outs skip it.
async fn mark_domain_unavailable(db: &sqlx::PgPool, domain: &str) {
    let _ = sqlx::query!(
        r#"INSERT INTO unavailable_domains (domain, created_at, updated_at)
           VALUES ($1, now(), now())
           ON CONFLICT (domain) DO UPDATE SET updated_at = now()"#,
        domain,
    )
    .execute(db)
    .await;
    tracing::info!(domain, "marked domain unavailable after 410 Gone");
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

/// Forward another server's `activity` to the remote followers of
/// `account_id`, signed by that account, as Mastodon forwards a reply to a
/// local post to its author's followers (ActivityPub §7.1.2). It goes as it
/// arrived: a proof of ours on someone else's activity would say nothing
/// true, and would replace the author's.
pub async fn forward_to_followers(
    state: &AppState,
    activity: Value,
    account_id: i64,
    key_id: String,
) -> anyhow::Result<u64> {
    let inboxes = follower_inboxes(state, account_id).await?;
    enqueue(state, activity, inboxes, key_id, false, None).await
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
    enqueue(state, activity, inboxes, key_id, false, batch).await
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

/// Deliver to a specific set of inboxes (for mentions, DMs, consent replies).
pub async fn deliver_to_inboxes(
    state: &AppState,
    activity: Value,
    inboxes: Vec<String>,
    key_id: String,
) -> anyhow::Result<u64> {
    enqueue_to_inboxes(state, activity, inboxes, key_id).await
}

/// [`deliver_to_inboxes`], in `batch`: tagged, and given up on at its
/// deadline, so that a batch of many accounts finishes.
pub async fn deliver_to_inboxes_in_batch(
    state: &AppState,
    activity: Value,
    inboxes: Vec<String>,
    key_id: String,
    batch: &ojak::deliverer::Batch,
) -> anyhow::Result<u64> {
    enqueue(state, activity, inboxes, key_id, true, Some(batch)).await
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
                "SELECT id FROM accounts WHERE domain IS NULL AND username = $1 LIMIT 1",
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
    enqueue(state, activity, inboxes, key_id, true, None).await
}

/// Queue `activity` for `inboxes`, signed by `key_id`'s account, with that
/// account's integrity proof when `prove`.
async fn enqueue(
    state: &AppState,
    activity: Value,
    inboxes: Vec<String>,
    key_id: String,
    prove: bool,
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
    let with_context = match with_data_integrity_context(activity.clone()) {
        Some(document) => document,
        None => return activity,
    };

    match ojak_runtime::integrity::sign_object_integrity_proof(
        &with_context,
        &verification_method,
        &key.seed,
    ) {
        Ok(signed) => signed,
        Err(e) => {
            tracing::warn!(account_id, error = %e, "could not sign an integrity proof");
            activity
        }
    }
}

/// Add the data integrity context to a document's `@context`, however it is
/// currently shaped, leaving it alone if it is already there.
fn with_data_integrity_context(mut activity: Value) -> Option<Value> {
    const DATA_INTEGRITY: &str = "https://w3id.org/security/data-integrity/v1";

    let object = activity.as_object_mut()?;
    match object.get_mut("@context") {
        Some(Value::Array(entries)) => {
            if !entries.iter().any(|e| e.as_str() == Some(DATA_INTEGRITY)) {
                entries.push(Value::from(DATA_INTEGRITY));
            }
        }
        Some(existing) => {
            let existing = existing.clone();
            object.insert(
                "@context".to_string(),
                Value::Array(vec![existing, Value::from(DATA_INTEGRITY)]),
            );
        }
        None => {
            object.insert("@context".to_string(), Value::from(DATA_INTEGRITY));
        }
    }
    Some(activity)
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
