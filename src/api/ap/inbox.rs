use serde_json::Value;

use crate::{error::AppResult, state::AppState};

mod attachment;
mod collection;
pub(crate) mod create;
mod fetch;
pub(crate) mod follow;
mod moderation;
mod poll_parser;
pub(crate) mod quote;
mod status;
pub(crate) mod status_parser;
use collection::{handle_add, handle_remove};
use create::handle_create;
pub use fetch::{
    fetch_remote_account, fetch_remote_poll, fetch_remote_status, fetch_remote_status_by_url,
    fetch_remote_status_prefetched, fetch_remote_status_with, resolve_or_fetch_remote_account,
    resolve_or_fetch_remote_account_prefetched, store_key_fetched_actor, unanswered, FetchOptions,
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

/// Whether two ids share a host, as Mastodon compares them, or for portable
/// ids a DID.
pub(super) use ojak::origin::same_authority as same_host;

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

/// Acquire the `create:{uri}` lock that serializes a status Create against a
/// concurrent Delete's `delete_later`, so the tombstone can't be set between the
/// Create's `delete_arrived_first?` check and its insert (Mastodon's
/// `with_redis_lock("create:#{object_uri}")`). Best-effort: retries briefly,
/// then proceeds without the lock rather than blocking an inbox request.
pub(super) async fn acquire_create_lock(
    state: &AppState,
    uri: &str,
) -> Option<crate::redis_lock::RedisLock> {
    if uri.is_empty() {
        return None;
    }
    let name = format!("create:{uri}");
    for attempt in 0..40 {
        if let Some(lock) =
            crate::redis_lock::try_acquire(state, &name, crate::redis_lock::DEFAULT_TTL_MS).await
        {
            return Some(lock);
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
    received_from(state, activity, None).await
}

/// Set on an activity by eunha, and never taken from its sender, when it came
/// through an enabled relay: Mastodon's `requested_through_relay?`, which
/// lets a public post from an account nobody here follows in.
pub(crate) const THROUGH_RELAY: &str = "eunha:requestedThroughRelay";

/// [`received`], delivered by `forwarder` rather than by its sender's
/// server: a relay, or a server forwarding a reply. Such an activity was
/// taken on its sender's proof or Linked Data Signature, or as its sender's
/// server serves it, and reads as JSON-LD processing gives it when it was
/// its Linked Data Signature (`ActivityPub::ProcessActivityService`, which
/// compacts it first).
pub async fn received_from(
    state: &AppState,
    activity: Value,
    forwarder: Option<&str>,
) -> AppResult<()> {
    received_at(state, activity, forwarder, None).await
}

/// Set on an activity by eunha, and never taken from its sender, when it
/// arrived at a local account's own inbox: the account's id, Mastodon's
/// `delivered_to_account_id`, which `ActivityPub::ProcessingWorker` carries.
pub(crate) const DELIVERED_TO: &str = "eunha:deliveredToAccountId";

/// Set on an activity by eunha, and never taken from its sender, when an
/// actor other than its own passed it on: that actor's URI, Mastodon's
/// `relayed_through_actor`.
pub(crate) const RELAYED_THROUGH: &str = "eunha:relayedThroughActor";

/// `followed_by_local_accounts?`: someone here follows `account_id`, or the
/// actor that passed the activity on (`relayed_through_actor.
/// passive_relationships.exists?`).
pub(super) async fn followed_by_local_accounts(
    state: &AppState,
    activity: &Value,
    account_id: i64,
) -> AppResult<bool> {
    let relayed_through = activity
        .get(RELAYED_THROUGH)
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM follows f
             WHERE f.target_account_id = $1
                OR f.target_account_id IN (SELECT id FROM accounts WHERE uri = $2 AND $2 <> '')
           ) AS "e!""#,
        account_id,
        relayed_through,
    )
    .fetch_one(&state.db)
    .await?)
}

/// [`received_from`], delivered to the inbox of the local account
/// `delivered_to`, or to the shared inbox when `None`.
pub async fn received_at(
    state: &AppState,
    activity: Value,
    forwarder: Option<&str>,
    delivered_to: Option<i64>,
) -> AppResult<()> {
    let mut activity = activity;
    if let Some(members) = activity.as_object_mut() {
        members.remove(THROUGH_RELAY);
        members.remove(DELIVERED_TO);
        members.remove(RELAYED_THROUGH);
        if let Some(forwarder) = forwarder {
            members.insert(
                RELAYED_THROUGH.to_owned(),
                Value::String(forwarder.to_owned()),
            );
            if through_enabled_relay(state, forwarder).await {
                members.insert(THROUGH_RELAY.to_owned(), Value::Bool(true));
            }
        }
        if let Some(account_id) = delivered_to {
            members.insert(DELIVERED_TO.to_owned(), Value::from(account_id));
        }
    }
    received_now(state, activity).await
}

/// The local account whose inbox `activity` was delivered to
/// (`@options[:delivered_to_account_id]`), if it still exists.
pub(super) async fn delivered_to(state: &AppState, activity: &Value) -> Option<i64> {
    let id = activity.get(DELIVERED_TO)?.as_i64()?;
    sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE id = $1 AND domain IS NULL",
        id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
}

/// `status_from_uri`: the status a URI names, local or already known, and
/// not discarded (`Status`'s default scope).
pub(super) async fn kept_status(state: &AppState, uri: &str) -> AppResult<Option<i64>> {
    let Some(id) = crate::federation::local_uri::status(state, uri).await else {
        return Ok(None);
    };
    Ok(sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        id
    )
    .fetch_optional(&state.db)
    .await?)
}

/// Whether `actor` is an enabled relay's: its inbox is one (`Relay.find_by(
/// inbox_url:)&.enabled?`).
async fn through_enabled_relay(state: &AppState, actor: &str) -> bool {
    let inbox: Option<String> = sqlx::query_scalar!(
        "SELECT inbox_url FROM accounts WHERE uri = $1 AND domain IS NOT NULL LIMIT 1",
        actor,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    crate::relays::is_enabled_relay_inbox(state, inbox.as_deref().unwrap_or("")).await
}

async fn received_now(state: &AppState, activity: Value) -> AppResult<()> {
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
    // A portable actor is named with the gateways it can be fetched from,
    // and known by its canonical id: it is fetched now, while the hints are
    // there, and every handler after sees the canonical id.
    let mut activity = activity;
    if ojak::portable::ApUri::parse(&actor_uri).is_some() {
        if let Err(error) = resolve_or_fetch_remote_account(state, &actor_uri).await {
            tracing::warn!(actor_uri, %error, "could not fetch a portable actor");
        }
        if activity.get("actor").is_some_and(Value::is_string) {
            activity["actor"] = Value::String(crate::federation::portable::canonical(&actor_uri));
        }
    }
    // The work itself (DB writes, remote fetches, fan-out) need not happen on
    // the sender's connection: it is queued, as Mastodon queues an
    // `ActivityPub::ProcessingWorker`. Tests opt into inline processing so
    // they can assert on the result without racing the worker.
    if !sync_ingress() {
        crate::jobs::perform_async(state, ProcessingWorker { activity }).await?;
        return Ok(());
    }
    // Handled inline, an activity that fails — a fetch its handler needs was
    // not answered, say — is queued to be tried again, as it would have been
    // from the queue: whether it is accepted does not hang on processing it,
    // in Mastodon or here. It waits for the job loops, or a test's
    // [`drain_inbox_queue`], rather than running at once.
    if let Err(error) =
        process_activity(state, &state.instance.clone(), &activity_type, &activity).await
    {
        tracing::debug!(activity_type, %error, "activity failed inline; queued to retry");
        queue_activity(&state.db, &activity).await?;
    }
    Ok(())
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

/// When true, inbound activities are handled inline in the request instead of
/// going through the job queue. Set by integration tests so they can assert
/// on an activity's effect immediately after the POST returns, mirroring
/// [`crate::feed::enable_sync_fanout`].
static SYNC_INGRESS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn enable_sync_ingress() {
    SYNC_INGRESS.store(true, std::sync::atomic::Ordering::Relaxed);
}

pub fn sync_ingress() -> bool {
    SYNC_INGRESS.load(std::sync::atomic::Ordering::Relaxed)
}

/// `ActivityPub::ProcessingWorker`: a verified activity, processed off the
/// request on the `ingress` queue and retried eight times, on Sidekiq's
/// schedule, if it fails. The actor it was signed by is not carried, as
/// Mastodon carries its id: every handler reads it from the activity, which
/// [`received_from`] has already checked and made canonical.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ProcessingWorker {
    pub activity: Value,
}

impl crate::jobs::Job for ProcessingWorker {
    const KIND: &'static str = "ActivityPub::ProcessingWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Ingress)
        .retry(8);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        let activity_type = self
            .activity
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        process_activity(
            state,
            &state.instance.clone(),
            &activity_type,
            &self.activity,
        )
        .await
        .map_err(anyhow::Error::new)
    }
}

/// Queue `activity` for an [`ProcessingWorker`] without waking anything: the
/// job loops find it when they next look, and a test drains it with
/// [`drain_inbox_queue`].
pub async fn queue_activity(db: &sqlx::PgPool, activity: &Value) -> anyhow::Result<()> {
    crate::jobs::perform_async_in(
        db,
        ProcessingWorker {
            activity: activity.clone(),
        },
    )
    .await?;
    Ok(())
}

/// Run every queued [`ProcessingWorker`] that is due, one at a time, until
/// none is, returning how many ran: what tests use to process what they
/// queued without racing the job loops, and without counting the other jobs
/// processing them queues.
pub async fn drain_inbox_queue(state: &AppState) -> anyhow::Result<usize> {
    let mut ran = 0;
    loop {
        let due: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM eunha.jobs
             WHERE kind = $1 AND dead_at IS NULL AND run_at <= now() AND locked_at IS NULL
             ORDER BY run_at, id",
        )
        .bind(<ProcessingWorker as crate::jobs::Job>::KIND)
        .fetch_all(&state.db)
        .await?;
        if due.is_empty() {
            return Ok(ran);
        }
        for id in due {
            crate::jobs::run_now(state, id).await?;
            ran += 1;
        }
    }
}

/// `ProcessStatusUpdateService#update_poll!`: the poll as its status now
/// has it, fetched just now (`last_fetched_at`).
pub(super) async fn sync_remote_poll(
    state: &AppState,
    status_id: i64,
    account_id: i64,
    object: &Value,
) -> AppResult<()> {
    let Some(poll) = poll_parser::PollParser::parse(object) else {
        return Ok(());
    };
    // A poll with no options is not valid (`validates :options, presence`).
    if poll.options.is_empty() {
        return Ok(());
    }
    let votes_count = poll.votes_count();
    let poll_parser::PollParser {
        multiple,
        options,
        cached_tallies,
        expires_at,
        voters_count,
    } = poll;

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
                   voters_count = $7,
                   last_fetched_at = now(),
                   updated_at = now()
               WHERE id = $1"#,
            poll_id,
            &options as &[String],
            &cached_tallies as &[i64],
            votes_count,
            multiple,
            expires_at,
            voters_count,
        )
        .execute(&state.db)
        .await?;
        state.queues.polls.notify_one();
    } else {
        let poll_id = crate::snowflake::next_id();
        if let Some(inserted_poll_id) = sqlx::query_scalar!(
            r#"INSERT INTO polls
                 (id, status_id, account_id, options, cached_tallies, votes_count,
                  multiple, expires_at, voters_count, last_fetched_at, created_at, updated_at)
               SELECT $1,$2,$3,$4,$5,$6,$7,$8,$9,now(),now(),now()
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
            voters_count,
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

/// Store a remote `FeaturedCollection` the account `owner_id` features, and
/// the items it embeds.
pub(crate) async fn mirror_remote_collection(
    state: &AppState,
    owner_id: i64,
    collection: &Value,
) -> AppResult<()> {
    if let Some(id) = upsert_remote_collection(state, owner_id, collection).await? {
        if let Some(items) = collection.get("orderedItems").and_then(|v| v.as_array()) {
            for item in items {
                let _ = mirror_item_into(state, id, item).await;
            }
        }
    }
    Ok(())
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
    // The collection as it was, if it was: `previously_new_record?` and
    // `attribute_previously_changed?` for `NotifyOfCollectionUpdateService`.
    let row = sqlx::query!(
        r#"WITH previous AS (
               SELECT name, sensitive FROM collections WHERE uri = $5
           )
           INSERT INTO collections
             (account_id, name, discoverable, local, sensitive, item_count, uri, created_at, updated_at)
           VALUES ($1, $2, $3, false, $4, 0, $5, now(), now())
           ON CONFLICT (uri) WHERE uri IS NOT NULL
             DO UPDATE SET name = EXCLUDED.name, discoverable = EXCLUDED.discoverable,
                           sensitive = EXCLUDED.sensitive, updated_at = now()
           RETURNING id,
                     (SELECT name FROM previous) AS "previous_name?",
                     (SELECT sensitive FROM previous) AS "previous_sensitive?""#,
        owner_id,
        name,
        discoverable,
        sensitive,
        uri,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else { return Ok(None) };
    if let (Some(previous_name), Some(previous_sensitive)) =
        (row.previous_name, row.previous_sensitive)
    {
        if previous_name != name || previous_sensitive != sensitive {
            Box::pin(crate::api::mastodon::collections::notify_of_collection_update(state, row.id))
                .await;
        }
    }
    Ok(Some(row.id))
}

/// Mirror one `FeaturedItem` into a (remote) collection.
pub(super) async fn mirror_item_into(
    state: &AppState,
    collection_id: i64,
    item: &Value,
) -> AppResult<()> {
    let item_uri = item.get("id").and_then(|v| v.as_str());
    let account_uri = item
        .get("featuredObject")
        .and_then(ojak_vocab::json_ld_helper::value_or_id)
        .unwrap_or_default();
    if account_uri.is_empty() {
        return Ok(());
    }
    let Ok(account_id) = resolve_or_fetch_remote_account(state, account_uri).await else {
        return Ok(());
    };
    // `xmax = 0` holds for a row this statement inserted, not one it updated.
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO collection_items
             (collection_id, account_id, state, uri, position, created_at, updated_at)
           VALUES ($1, $2, 1, $3,
                   (SELECT COALESCE(MAX(position), 0) + 1 FROM collection_items WHERE collection_id = $1),
                   now(), now())
           ON CONFLICT (account_id, collection_id)
             DO UPDATE SET state = 1, uri = EXCLUDED.uri, updated_at = now()
           RETURNING (xmax = 0) AS "inserted!""#,
        collection_id,
        account_id,
        item_uri,
    )
    .fetch_one(&state.db)
    .await?;
    // The counter cache counts a created item, not an updated one.
    if inserted {
        crate::api::mastodon::collections::update_item_count(&state.db, collection_id, 1).await?;
    }
    Ok(())
}
