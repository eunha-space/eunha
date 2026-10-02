//! Keeping the indexes current, as Mastodon does it: a change to a row adds
//! its id to a Redis set, `chewy:queue:<Index>` (the `:mastodon` Chewy
//! strategy), and once a minute `Scheduler::IndexingScheduler` takes the ids
//! out a thousand at a time and imports them — indexing the rows still in
//! the index's scope and deleting the rest.
//!
//! The set is the durable queue: an id leaves it only once its batch was
//! written, so a search server that is down or refusing keeps the ids until a
//! later pass succeeds, and a restart loses nothing. The sets are named as
//! Mastodon names them, under the instance's Redis key prefix, so a queue a
//! Mastodon left behind on the same Redis is drained too.

use std::time::Duration;

use super::documents::{bulk_lines, documents};
use super::Index;
use crate::state::AppState;

/// `Scheduler::IndexingScheduler::IMPORT_BATCH_SIZE`.
const IMPORT_BATCH_SIZE: usize = 1000;
/// `SCAN_BATCH_SIZE`.
const SCAN_BATCH_SIZE: usize = 10 * IMPORT_BATCH_SIZE;
/// The scheduler's `interval: 1 minute`.
const INTERVAL: Duration = Duration::from_secs(60);
/// `Scheduler::InstanceRefreshScheduler`'s `cron: '0 * * * *'`.
const INSTANCES_INTERVAL: Duration = Duration::from_secs(3600);
/// `lock_ttl: 30.minutes`.
const LOCK_TTL_MS: usize = 30 * 60 * 1000;

fn queue_key(state: &AppState, index: Index) -> String {
    state
        .redis_keys
        .key(format!("chewy:queue:{}", index.class_name()))
}

/// The `:mastodon` strategy's `update`: queue `ids` for `index`. Nothing
/// without Elasticsearch; a Redis failure costs the update, not the request.
pub async fn enqueue(state: &AppState, index: Index, ids: &[i64]) {
    if state.search.is_none() || ids.is_empty() {
        return;
    }
    let mut redis = state.redis.clone();
    let result: redis::RedisResult<()> = redis::cmd("SADD")
        .arg(queue_key(state, index))
        .arg(ids)
        .query_async(&mut redis)
        .await;
    if let Err(error) = result {
        tracing::warn!(%error, index = index.base_name(), "could not queue a search index update");
    }
}

/// `update_index('statuses', :proper)` and `update_index('public_statuses',
/// :proper)`: a post was created, edited or deleted, or a boost of it was.
/// `proper_id` is the post itself, not the boost.
pub async fn status(state: &AppState, proper_id: i64) {
    enqueue(state, Index::Statuses, &[proper_id]).await;
    enqueue(state, Index::PublicStatuses, &[proper_id]).await;
}

/// `update_index('statuses', :status)`: a post was favourited or bookmarked,
/// or that was undone, which changes who may search it.
pub async fn status_interaction(state: &AppState, status_id: i64) {
    enqueue(state, Index::Statuses, &[status_id]).await;
}

/// `update_index('accounts', :self)` and the `AccountStat` one.
pub async fn account(state: &AppState, account_id: i64) {
    enqueue(state, Index::Accounts, &[account_id]).await;
}

/// [`account`] for several at once.
pub async fn accounts(state: &AppState, account_ids: &[i64]) {
    enqueue(state, Index::Accounts, account_ids).await;
}

/// `update_index('tags', :self)`.
pub async fn tags(state: &AppState, tag_ids: &[i64]) {
    enqueue(state, Index::Tags, tag_ids).await;
}

/// `Account::StatusesSearch#enqueue_update_public_statuses_index`: whether an
/// account is `indexable` changed, so its public posts join or leave the
/// public index. Mastodon hands this to a worker that imports or
/// deletes-by-query; eunha queues the posts, which the scheduler then indexes
/// or deletes by the same scope check.
pub async fn account_indexable_changed(state: &AppState, account_id: i64) {
    if state.search.is_none() {
        return;
    }
    let mut after = 0i64;
    loop {
        let ids: Vec<i64> = match sqlx::query_scalar(
            "SELECT id FROM statuses \
             WHERE account_id = $1 AND reblog_of_id IS NULL AND visibility = 0 AND id > $2 \
             ORDER BY id LIMIT 1000",
        )
        .bind(account_id)
        .bind(after)
        .fetch_all(&state.db)
        .await
        {
            Ok(ids) => ids,
            Err(error) => {
                tracing::warn!(%error, account_id, "could not queue an account's public posts");
                return;
            }
        };
        let Some(&last) = ids.last() else { return };
        enqueue(state, Index::PublicStatuses, &ids).await;
        after = last;
    }
}

/// Chewy's `import!(ids)` for one batch.
pub async fn import(state: &AppState, index: Index, ids: &[i64]) -> anyhow::Result<()> {
    let client = state
        .search
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Elasticsearch is not enabled"))?;
    let docs = documents(state, index, ids).await?;
    client
        .bulk(&client.index_name(index.base_name()), &bulk_lines(docs))
        .await
}

/// `Scheduler::IndexingScheduler#perform`: drain every queue.
pub async fn drain(state: &AppState) -> anyhow::Result<()> {
    if state.search.is_none() {
        return Ok(());
    }
    let Some(_lock) =
        crate::redis_lock::try_acquire(state, "search:indexing_scheduler", LOCK_TTL_MS).await
    else {
        return Ok(());
    };
    for index in Index::QUEUED {
        let key = queue_key(state, index);
        let mut redis = state.redis.clone();
        let mut cursor: u64 = 0;
        loop {
            let (next, ids): (u64, Vec<i64>) = redis::cmd("SSCAN")
                .arg(&key)
                .arg(cursor)
                .arg("COUNT")
                .arg(SCAN_BATCH_SIZE)
                .query_async(&mut redis)
                .await?;
            for batch in ids.chunks(IMPORT_BATCH_SIZE) {
                import(state, index, batch).await?;
                let _: () = redis::cmd("SREM")
                    .arg(&key)
                    .arg(batch)
                    .query_async(&mut redis)
                    .await?;
            }
            if next == 0 {
                break;
            }
            cursor = next;
        }
    }
    Ok(())
}

/// `InstancesIndex.sync`: every known domain indexed, and the ones no longer
/// known deleted.
pub async fn sync_instances(state: &AppState) -> anyhow::Result<()> {
    let Some(client) = state.search.as_ref() else {
        return Ok(());
    };
    let index = client.index_name(Index::Instances.base_name());
    let docs = super::documents::instances(state).await?;
    let known: std::collections::HashSet<String> = docs.iter().map(|(d, _)| d.clone()).collect();
    for chunk in docs.chunks(IMPORT_BATCH_SIZE) {
        client.bulk(&index, &bulk_lines(chunk.to_vec())).await?;
    }
    let indexed = super::deploy::indexed_ids(client, &index).await?;
    let gone: Vec<(String, Option<serde_json::Value>)> = indexed
        .into_iter()
        .filter(|d| !known.contains(d))
        .map(|d| (d, None))
        .collect();
    for chunk in gone.chunks(IMPORT_BATCH_SIZE) {
        client.bulk(&index, &bulk_lines(chunk.to_vec())).await?;
    }
    Ok(())
}

/// The scheduler: every minute drain the queues, and every hour sync the
/// instances index, until the instance stops.
pub async fn run(state: AppState) {
    if state.search.is_none() {
        return;
    }
    let mut since_instances = INSTANCES_INTERVAL;
    loop {
        crate::background::rest(&state.stop, INTERVAL).await;
        if state.stop.is_cancelled() {
            return;
        }
        if let Err(error) = drain(&state).await {
            tracing::warn!(%error, "could not import queued search index updates; they stay queued");
        }
        since_instances += INTERVAL;
        if since_instances >= INSTANCES_INTERVAL {
            since_instances = Duration::ZERO;
            if let Err(error) = sync_instances(&state).await {
                tracing::warn!(%error, "could not sync the instances index");
            }
        }
    }
}
