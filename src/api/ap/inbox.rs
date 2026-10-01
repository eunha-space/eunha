use serde_json::Value;

use crate::{error::AppResult, state::AppState};

mod attachment;
mod collection;
mod create;
mod fetch;
mod follow;
mod moderation;
mod quote;
mod status;
use collection::{handle_add, handle_remove};
use create::handle_create;
pub use fetch::{
    fetch_remote_status, fetch_remote_status_prefetched, resolve_or_fetch_remote_account,
    resolve_or_fetch_remote_account_prefetched,
};
use follow::{handle_accept_reject, handle_follow, handle_undo};
use moderation::{handle_block, handle_flag, handle_move};
use quote::{handle_feature_request, handle_quote_request};
use status::{handle_announce, handle_delete, handle_like, handle_update};

/// `StatusParser#quote_policy` for a remote post by `account_id`, read
/// against the author's collections.
pub(super) async fn remote_quote_policy(state: &AppState, account_id: i64, object: &Value) -> i32 {
    let row = sqlx::query!(
        "SELECT followers_url, following_url, uri FROM accounts WHERE id = $1",
        account_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    match row {
        Some(r) => crate::db::models::quote_policy::parse(
            object,
            &r.followers_url,
            &r.following_url,
            r.uri.as_deref().unwrap_or_default(),
        ),
        None => 0,
    }
}

/// Returns true if a tag's `type` field equals `type_name`, handling both
/// string (`"Mention"`) and array (`["Mention", "Link"]`) forms.
pub(super) fn tag_type_is(tag: &Value, type_name: &str) -> bool {
    match tag.get("type") {
        Some(Value::String(s)) => s == type_name,
        Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some(type_name)),
        _ => false,
    }
}

/// Normalises a JSON field that may be a string, an array of strings, or absent
/// into an owned `Vec<String>`. Handles both `"x"` and `["x","y"]`.
pub(super) fn as_string_vec(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => vec![],
    }
}

/// Returns true when the two URI strings share the same host, or for
/// portable ids the same DID.
pub(super) fn same_host(a: &str, b: &str) -> bool {
    crate::federation::portable::same_authority(a, b)
}

/// TTL of a "delete arrived first" tombstone, matching Mastodon's 6 hours.
const DELETE_UPON_ARRIVAL_TTL: i64 = 6 * 60 * 60;

fn delete_upon_arrival_key(actor: &str, uri: &str) -> String {
    format!("delete_upon_arrival:{actor}:{uri}")
}

/// Remember that an Undo/Delete for `uri` from `actor` arrived, so a later,
/// out-of-order activity carrying that id is skipped rather than resurrecting
/// the deleted object (Mastodon's `delete_later!`).
pub(super) async fn delete_later(state: &AppState, actor: &str, uri: &str) {
    if actor.is_empty() || uri.is_empty() {
        return;
    }
    let mut redis = state.redis_coordination.clone();
    let key = state.redis_keys.key(delete_upon_arrival_key(actor, uri));
    let _: redis::RedisResult<()> = redis::cmd("SETEX")
        .arg(&key)
        .arg(DELETE_UPON_ARRIVAL_TTL)
        .arg(1)
        .query_async(&mut redis)
        .await;
}

/// Whether an Undo/Delete for `uri` from `actor` already arrived (Mastodon's
/// `delete_arrived_first?`).
pub(super) async fn delete_arrived_first(state: &AppState, actor: &str, uri: &str) -> bool {
    if actor.is_empty() || uri.is_empty() {
        return false;
    }
    let mut redis = state.redis_coordination.clone();
    let key = state.redis_keys.key(delete_upon_arrival_key(actor, uri));
    let exists: i64 = redis::cmd("EXISTS")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .unwrap_or(0);
    exists == 1
}

/// Autorelease TTL for the `create:{uri}` serialization lock, matching
/// Mastodon's default `with_redis_lock` timeout.
const CREATE_LOCK_TTL_MS: usize = 15 * 60 * 1000;

/// Best-effort Redis lock guard: releases the lock (only if still the owner) on
/// drop.
pub(super) struct RedisLock {
    redis: redis::aio::ConnectionManager,
    key: String,
    token: String,
    use_pooled_function: bool,
}

impl Drop for RedisLock {
    fn drop(&mut self) {
        let mut redis = self.redis.clone();
        let key = std::mem::take(&mut self.key);
        let token = std::mem::take(&mut self.token);
        let use_pooled_function = self.use_pooled_function;
        crate::tenants::spawn(async move {
            // A pooled Redis installs this named function so tenant users need
            // no permission to submit arbitrary Lua. Dedicated deployments
            // retain the EVAL fallback and require no provisioning change.
            if use_pooled_function {
                let released: redis::RedisResult<i64> = redis::cmd("FCALL")
                    .arg("eunha_compare_delete")
                    .arg(1)
                    .arg(&key)
                    .arg(&token)
                    .query_async(&mut redis)
                    .await;
                if released.is_ok() {
                    return;
                }
            }
            let _: redis::RedisResult<i64> = redis::cmd("EVAL")
                .arg("if redis.call('get', KEYS[1]) == ARGV[1] then return redis.call('del', KEYS[1]) else return 0 end")
                .arg(1)
                .arg(&key)
                .arg(&token)
                .query_async(&mut redis)
                .await;
        });
    }
}

/// Acquire the `create:{uri}` lock that serializes a status Create against a
/// concurrent Delete's `delete_later`, so the tombstone can't be set between the
/// Create's `delete_arrived_first?` check and its insert (Mastodon's
/// `with_redis_lock("create:#{object_uri}")`). Best-effort: retries briefly,
/// then proceeds without the lock rather than blocking an inbox request.
pub(super) async fn acquire_create_lock(state: &AppState, uri: &str) -> Option<RedisLock> {
    if uri.is_empty() {
        return None;
    }
    let key = state.redis_keys.key(format!("create:{uri}"));
    let token = crate::snowflake::next_id().to_string();
    let mut redis = state.redis_coordination.clone();
    for attempt in 0..40 {
        let acquired: redis::RedisResult<Option<String>> = redis::cmd("SET")
            .arg(&key)
            .arg(&token)
            .arg("NX")
            .arg("PX")
            .arg(CREATE_LOCK_TTL_MS)
            .query_async(&mut redis)
            .await;
        if matches!(acquired, Ok(Some(_))) {
            return Some(RedisLock {
                redis: state.redis_coordination.clone(),
                key,
                token,
                use_pooled_function: state.redis_keys.is_shared(),
            });
        }
        if attempt < 39 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    None
}

/// An activity ojak has received and authenticated: queued for the ingress
/// worker, or, in tests, handled at once.
///
/// Ojak's inbox (`super::serving`) has already done what this handler used
/// to: refused a suspended domain's activities before fetching a key for
/// them, verified the HTTP Signature or, failing it, an FEP-8b32 proof,
/// accepted and dropped a Delete it could not verify, and checked that the
/// actor and the activity are on the sender's origin. `activity` is the one
/// the sender sent, with anything embedded that the sender could not vouch
/// for reduced to its id, so a handler that reads an embedded object reads
/// one its sender owns and fetches anything else.
pub async fn received(state: &AppState, activity: Value) -> AppResult<()> {
    let activity_type = activity
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_owned();
    let actor_uri = activity
        .get("actor")
        .and_then(|a| a.as_str().or_else(|| a.get("id").and_then(|i| i.as_str())))
        .unwrap_or("")
        .to_owned();
    tracing::debug!(
        instance = %state.instance.domain,
        activity_type,
        body = %activity,
        "received ActivityPub activity"
    );
    // The work itself (DB writes, remote fetches, fan-out) need not happen on
    // the sender's connection: it is queued, as Mastodon's
    // ActivityPub::ProcessingWorker does. Tests opt into inline processing so
    // they can assert on the result without racing the worker.
    // A portable actor is named with the gateways it can be fetched from,
    // and known by its canonical id: it is fetched now, while the hints are
    // there, and every handler after sees the canonical id.
    let mut activity = activity;
    let actor_uri = if ojak::portable::ApUri::parse(&actor_uri).is_some() {
        if let Err(error) = resolve_or_fetch_remote_account(state, &actor_uri).await {
            tracing::warn!(actor_uri, %error, "could not fetch a portable actor");
        }
        let canonical = crate::federation::portable::canonical(&actor_uri);
        if activity.get("actor").is_some_and(Value::is_string) {
            activity["actor"] = Value::String(canonical.clone());
        }
        canonical
    } else {
        actor_uri
    };
    if !sync_ingress() {
        return enqueue_activity(state, &activity_type, &actor_uri, &activity).await;
    }
    process_activity(state, &state.instance.clone(), &activity_type, &activity).await
}

/// Dispatch a verified activity to its handler. Runs on the ingress queue in
/// production, inline under [`enable_sync_ingress`].
pub(super) async fn process_activity(
    state: &AppState,
    instance: &crate::config::InstanceConfig,
    activity_type: &str,
    activity: &Value,
) -> AppResult<()> {
    let outcome = match activity_type {
        "Follow" => {
            handle_follow(state, instance, activity).await?;
            "handled"
        }
        "Undo" => {
            handle_undo(state, instance, activity).await?;
            "handled"
        }
        "Create" => {
            handle_create(state, instance, activity).await?;
            "handled"
        }
        "Delete" => {
            handle_delete(state, instance, activity).await?;
            "handled"
        }
        "Announce" => {
            handle_announce(state, instance, activity).await?;
            "handled"
        }
        "Like" => {
            handle_like(state, instance, activity).await?;
            "handled"
        }
        "Accept" | "Reject" => {
            handle_accept_reject(state, instance, activity).await?;
            "handled"
        }
        "Update" => {
            handle_update(state, instance, activity).await?;
            "handled"
        }
        "Block" => {
            handle_block(state, activity).await?;
            "handled"
        }
        "Flag" => {
            handle_flag(state, activity).await?;
            "handled"
        }
        "Move" => {
            handle_move(state, activity).await?;
            "handled"
        }
        "Add" => {
            handle_add(state, activity).await?;
            "handled"
        }
        "Remove" => {
            handle_remove(state, activity).await?;
            "handled"
        }
        "QuoteRequest" => {
            handle_quote_request(state, instance, activity).await?;
            "handled"
        }
        "FeatureRequest" => {
            handle_feature_request(state, instance, activity).await?;
            "handled"
        }
        _ => "ignored",
    };
    tracing::debug!(activity_type, outcome, "ActivityPub activity processed");

    Ok(())
}

// ── Ingress queue ─────────────────────────────────────────────────────────

const INBOX_QUEUE_IDLE: std::time::Duration = std::time::Duration::from_millis(500);
const INBOX_QUEUE_ERROR_IDLE: std::time::Duration = std::time::Duration::from_secs(10);

/// When true, inbound activities are handled inline in the request instead of
/// going through the durable queue. Set by integration tests so they can assert
/// on an activity's effect immediately after the POST returns, mirroring
/// [`crate::feed::enable_sync_fanout`].
static SYNC_INGRESS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn enable_sync_ingress() {
    SYNC_INGRESS.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn sync_ingress() -> bool {
    SYNC_INGRESS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Record a verified activity for the ingress worker to process.
async fn enqueue_activity(
    state: &AppState,
    activity_type: &str,
    actor_uri: &str,
    activity: &Value,
) -> AppResult<()> {
    sqlx::query!(
        r#"INSERT INTO eunha.inbox_jobs
             (activity, activity_type, actor_uri, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())"#,
        activity,
        activity_type,
        actor_uri,
    )
    .execute(&state.db)
    .await?;
    state.queues.inbox.notify_one();
    Ok(())
}

/// Run one loop of the durable ingress queue. As with the delivery queue,
/// `index` only distinguishes sibling loops in `locked_by`; `FOR UPDATE SKIP
/// LOCKED` is what keeps concurrent claims disjoint.
pub async fn run_inbox_queue(state: AppState, index: usize) {
    let worker_id = format!(
        "{}:{}:ingress:{}",
        std::env::var("HOSTNAME").unwrap_or_else(|_| "eunha".into()),
        std::process::id(),
        index,
    );
    let workers = state.config.workers.sanitized();
    let batch = workers.inbox_batch;
    let concurrency = workers.inbox_concurrency;
    let mut idle = crate::background::IdleBackoff::new(INBOX_QUEUE_IDLE, workers.queue_idle_poll());

    while !state.stop.is_cancelled() {
        match run_inbox_queue_batch(&state, &worker_id, batch, concurrency).await {
            Ok(0) => idle.idle(&state.queues.inbox, &state.stop).await,
            Ok(n) => {
                idle.reset();
                tracing::debug!(count = n, "processed inbound ActivityPub activities");
            }
            Err(e) => {
                tracing::error!(error = %e, "ingress queue batch failed");
                crate::background::rest(&state.stop, INBOX_QUEUE_ERROR_IDLE).await;
            }
        }
    }
}

/// Claim and process one batch of queued activities, returning how many were
/// handled. Tests use this to drain the queue deterministically instead of
/// racing the always-running worker loop.
pub async fn drain_inbox_queue(state: &AppState) -> anyhow::Result<usize> {
    run_inbox_queue_batch(state, "drain", 100, 1).await
}

async fn run_inbox_queue_batch(
    state: &AppState,
    worker_id: &str,
    batch: i64,
    concurrency: usize,
) -> anyhow::Result<usize> {
    let jobs = sqlx::query!(
        r#"WITH picked AS (
             SELECT id
             FROM eunha.inbox_jobs
             WHERE failed_at IS NULL
               AND run_at <= now()
               AND (locked_at IS NULL OR locked_at < now() - interval '10 minutes')
             ORDER BY run_at ASC, id ASC
             LIMIT $1
             FOR UPDATE SKIP LOCKED
           )
           UPDATE eunha.inbox_jobs j
           SET locked_at = now(), locked_by = $2, updated_at = now()
           FROM picked
           WHERE j.id = picked.id
           RETURNING j.id, j.activity, j.activity_type, j.actor_uri,
                     j.attempts, j.max_attempts"#,
        batch,
        worker_id,
    )
    .fetch_all(&state.db)
    .await?;

    let count = jobs.len();
    let instance = (*state.instance).clone();

    // Activities for the same object can interleave here exactly as they can
    // across Sidekiq threads; the `create:{uri}` Redis lock and the
    // `delete_upon_arrival` tombstone already serialize the cases that matter.
    futures::stream::StreamExt::for_each_concurrent(
        futures::stream::iter(jobs),
        concurrency,
        |job| {
            let instance = instance.clone();
            async move {
                process_inbox_job(
                    state,
                    &instance,
                    job.id,
                    &job.activity_type,
                    &job.actor_uri,
                    &job.activity,
                    job.attempts,
                    job.max_attempts,
                )
                .await;
            }
        },
    )
    .await;

    Ok(count)
}

#[allow(clippy::too_many_arguments)]
async fn process_inbox_job(
    state: &AppState,
    instance: &crate::config::InstanceConfig,
    id: i64,
    activity_type: &str,
    actor_uri: &str,
    activity: &Value,
    attempts: i32,
    max_attempts: i32,
) {
    match process_activity(state, instance, activity_type, activity).await {
        Ok(()) => {
            let _ = sqlx::query!("DELETE FROM eunha.inbox_jobs WHERE id = $1", id)
                .execute(&state.db)
                .await;
        }
        Err(e) => {
            let next = attempts + 1;
            let err = crate::error::sanitize_error_text(&e.to_string());
            if next >= max_attempts {
                tracing::warn!(
                    id,
                    activity_type,
                    actor = actor_uri,
                    attempts = next,
                    error = %err,
                    "inbound activity failed permanently"
                );
                let _ = sqlx::query!(
                    r#"UPDATE eunha.inbox_jobs
                       SET attempts = $2, failed_at = now(), locked_at = NULL, locked_by = NULL,
                           last_error = $3, updated_at = now()
                       WHERE id = $1"#,
                    id,
                    next,
                    err,
                )
                .execute(&state.db)
                .await;
            } else {
                // Exponential backoff: 15s, 30s, 60s, … capped at 30 minutes.
                let backoff = (15_i64 << (next.clamp(1, 8) - 1)).min(1800);
                let run_at = chrono::Utc::now() + chrono::Duration::seconds(backoff);
                let _ = sqlx::query!(
                    r#"UPDATE eunha.inbox_jobs
                       SET attempts = $2, run_at = $3, locked_at = NULL, locked_by = NULL,
                           last_error = $4, updated_at = now()
                       WHERE id = $1"#,
                    id,
                    next,
                    run_at,
                    err,
                )
                .execute(&state.db)
                .await;
                tracing::warn!(
                    id,
                    activity_type,
                    attempts = next,
                    error = %err,
                    "inbound activity failed; will retry"
                );
            }
        }
    }
}

/// Periodically delete inbound activities that failed permanently. Their
/// `last_error` is kept for a week so a federation bug is still diagnosable
/// after the fact.
pub async fn run_inbox_cleanup(state: AppState) {
    while !state.stop.is_cancelled() {
        let deleted = sqlx::query!(
            r#"DELETE FROM eunha.inbox_jobs
               WHERE failed_at IS NOT NULL AND failed_at < now() - interval '7 days'"#,
        )
        .execute(&state.db)
        .await
        .map(|r| r.rows_affected());
        match deleted {
            Ok(0) => {}
            Ok(n) => tracing::info!(deleted = n, "pruned failed inbound activities"),
            Err(e) => tracing::error!(error = %e, "inbox job cleanup failed"),
        }
        crate::background::rest(&state.stop, std::time::Duration::from_secs(3600)).await;
    }
}

/// Recompute a collection's `item_count` (pending + accepted items).
pub(super) async fn refresh_collection_item_count(
    state: &AppState,
    collection_id: i64,
) -> AppResult<()> {
    sqlx::query!(
        r#"UPDATE collections SET item_count =
             (SELECT COUNT(*) FROM collection_items WHERE collection_id = $1 AND state IN (0, 1))
           WHERE id = $1"#,
        collection_id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

pub(super) async fn sync_remote_poll(
    state: &AppState,
    status_id: i64,
    account_id: i64,
    object: &Value,
) -> AppResult<()> {
    let Some(items) = object
        .get("oneOf")
        .or_else(|| object.get("anyOf"))
        .and_then(|v| v.as_array())
    else {
        return Ok(());
    };

    let multiple = object.get("anyOf").is_some();
    let options: Vec<String> = items
        .iter()
        .filter_map(|item| item.get("name").and_then(|v| v.as_str()).map(str::to_owned))
        .collect();
    if options.is_empty() {
        return Ok(());
    }

    let cached_tallies: Vec<i64> = items
        .iter()
        .map(|item| {
            item.get("replies")
                .and_then(|r| r.get("totalItems"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0)
        })
        .collect();
    let votes_count: i64 = cached_tallies.iter().sum();
    let expires_at = object
        .get("endTime")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc).naive_utc());

    if let Some(poll_id) =
        sqlx::query_scalar!("SELECT id FROM polls WHERE status_id = $1", status_id,)
            .fetch_optional(&state.db)
            .await?
    {
        sqlx::query!(
            r#"UPDATE polls
               SET options = $2,
                   cached_tallies = $3,
                   votes_count = $4,
                   multiple = $5,
                   expires_at = $6,
                   updated_at = now()
               WHERE id = $1"#,
            poll_id,
            &options as &[String],
            &cached_tallies as &[i64],
            votes_count,
            multiple,
            expires_at,
        )
        .execute(&state.db)
        .await?;
        state.queues.polls.notify_one();
    } else {
        let poll_id = crate::snowflake::next_id();
        if let Some(inserted_poll_id) = sqlx::query_scalar!(
            r#"INSERT INTO polls
                 (id, status_id, account_id, options, cached_tallies, votes_count,
                  multiple, expires_at, created_at, updated_at)
               SELECT $1,$2,$3,$4,$5,$6,$7,$8,now(),now()
               WHERE NOT EXISTS (SELECT 1 FROM polls WHERE status_id = $2)
               RETURNING id"#,
            poll_id,
            status_id,
            account_id,
            &options as &[String],
            &cached_tallies as &[i64],
            votes_count,
            multiple,
            expires_at,
        )
        .fetch_optional(&state.db)
        .await?
        {
            state.queues.polls.notify_one();
            sqlx::query!(
                "UPDATE statuses SET poll_id = $1 WHERE id = $2",
                inserted_poll_id,
                status_id,
            )
            .execute(&state.db)
            .await?;
        }
    }

    Ok(())
}

/// Read a JSON value that may be a string IRI or an object with an `id`.
pub(super) fn json_uri(v: Option<&Value>) -> &str {
    v.and_then(|x| {
        if x.is_string() {
            x.as_str()
        } else {
            x.get("id").and_then(|i| i.as_str())
        }
    })
    .unwrap_or("")
}

/// Insert or update a mirrored remote collection (`local = false`).
pub(super) async fn upsert_remote_collection(
    state: &AppState,
    owner_id: i64,
    coll: &Value,
) -> AppResult<Option<i64>> {
    let uri = coll.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if uri.is_empty() {
        return Ok(None);
    }
    let name = coll
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("Featured collection");
    let sensitive = coll
        .get("sensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let discoverable = coll
        .get("discoverable")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let id = sqlx::query_scalar!(
        r#"INSERT INTO collections
             (account_id, name, discoverable, local, sensitive, item_count, uri, created_at, updated_at)
           VALUES ($1, $2, $3, false, $4, 0, $5, now(), now())
           ON CONFLICT (uri) WHERE uri IS NOT NULL
             DO UPDATE SET name = EXCLUDED.name, discoverable = EXCLUDED.discoverable,
                           sensitive = EXCLUDED.sensitive, updated_at = now()
           RETURNING id"#,
        owner_id,
        name,
        discoverable,
        sensitive,
        uri,
    )
    .fetch_optional(&state.db)
    .await?;
    Ok(id)
}

/// Mirror one `FeaturedItem` into a (remote) collection.
pub(super) async fn mirror_item_into(
    state: &AppState,
    collection_id: i64,
    item: &Value,
) -> AppResult<()> {
    let item_uri = item.get("id").and_then(|v| v.as_str());
    let account_uri = json_uri(item.get("featuredObject"));
    if account_uri.is_empty() {
        return Ok(());
    }
    let Ok(account_id) = resolve_or_fetch_remote_account(state, account_uri).await else {
        return Ok(());
    };
    sqlx::query!(
        r#"INSERT INTO collection_items
             (collection_id, account_id, state, uri, position, created_at, updated_at)
           VALUES ($1, $2, 1, $3,
                   (SELECT COALESCE(MAX(position), 0) + 1 FROM collection_items WHERE collection_id = $1),
                   now(), now())
           ON CONFLICT (account_id, collection_id)
             DO UPDATE SET state = 1, uri = EXCLUDED.uri, updated_at = now()"#,
        collection_id,
        account_id,
        item_uri,
    )
    .execute(&state.db)
    .await?;
    refresh_collection_item_count(state, collection_id).await
}
