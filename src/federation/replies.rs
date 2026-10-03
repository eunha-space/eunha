//! Fetching the replies to remote statuses through their `replies`
//! collections, as Mastodon does at two moments:
//!
//!  -  when a status arrives in a `Create`, the first page of its `replies`
//!     (`ActivityPub::Activity::Create#fetch_replies` →
//!     `ActivityPub::FetchRepliesService`): up to five replies, from the
//!     author's own server;
//!  -  when a signed-in user opens a remote status's thread
//!     (`Api::V1::Statuses::ContextsController` →
//!     `ActivityPub::FetchAllRepliesWorker`): the whole reply tree, walked
//!     through each reply's own collection, up to [`MAX_REPLIES`] statuses and
//!     [`MAX_PAGES`] collection pages, at most once every
//!     [`FETCH_REPLIES_COOLDOWN_MINUTES`] per status. The request that starts
//!     it answers with an async refresh the client polls; its `result_count`
//!     counts the statuses that were new to us.
//!
//! Both run as jobs (crate::jobs), as Mastodon's do: `ActivityPub::
//! FetchRepliesWorker` and `ActivityPub::FetchAllRepliesWorker`, which queue
//! one `FetchReplyWorker` for each reply they find, all on the `pull` queue,
//! retried three times on `ExponentialBackoff`. A `FetchReplyWorker` fetches
//! the reply again and processes it as the `Create` it would have come in —
//! so its own first page of replies is read in turn — or, when it is already
//! held, as an `Update`. The walk and the replies it queues are one
//! [`crate::worker_batch::WorkerBatch`], whose end finishes the refresh.

use std::collections::HashSet;

use serde_json::Value;

use crate::db::models::Status as DbStatus;
use crate::federation::json_ld::{self, is_present, non_matching_uri_hosts, value_or_id};
use crate::state::AppState;

/// `Status::FetchRepliesConcern::FETCH_REPLIES_COOLDOWN_MINUTES`.
pub const FETCH_REPLIES_COOLDOWN_MINUTES: i64 = 15;
/// `Status::FetchRepliesConcern::FETCH_REPLIES_INITIAL_WAIT_MINUTES`.
pub const FETCH_REPLIES_INITIAL_WAIT_MINUTES: i64 = 5;
/// `ActivityPub::FetchAllRepliesWorker::MAX_REPLIES`: statuses discovered in
/// one walk of a reply tree.
pub const MAX_REPLIES: usize = 1000;
/// `ActivityPub::FetchAllRepliesWorker::MAX_PAGES`: collection pages fetched
/// in one walk.
pub const MAX_PAGES: usize = 500;
/// `ActivityPub::FetchRepliesService::MAX_REPLIES`. A collection is read page
/// by page until it has given at least this many items. `FetchAllReplies
/// Service` declares a larger `MAX_REPLIES`, but the `collection_items` call
/// it inherits resolves the constant lexically, to this one.
pub const COLLECTION_MAX_ITEMS: usize = 5;
/// `ActivityPub::FetchAllRepliesService::MAX_REPLIES`: replies kept from one
/// status's collection.
pub const MAX_REPLIES_PER_STATUS: usize = 500;

/// The async refresh a context view of `status_id` starts.
pub fn refresh_key(status_id: i64) -> String {
    format!("context:{status_id}:refresh")
}

/// `Status#should_fetch_replies?`: a remote, public or unlisted status, at
/// least five minutes old, whose replies were not fetched in the last fifteen.
pub fn should_fetch_replies(status: &DbStatus) -> bool {
    let now = chrono::Utc::now().naive_utc();
    let remote = status.local == Some(false) && status.uri.is_some();
    let distributable = matches!(
        status.visibility,
        crate::db::models::vis::PUBLIC | crate::db::models::vis::UNLISTED
    );
    remote
        && distributable
        && status.created_at <= now - chrono::Duration::minutes(FETCH_REPLIES_INITIAL_WAIT_MINUTES)
        && status
            .fetched_replies_at
            .is_none_or(|at| at <= now - chrono::Duration::minutes(FETCH_REPLIES_COOLDOWN_MINUTES))
}

/// `fetch_resource(uri, true)`, as the walk asks for a status to read its
/// `replies` from: `raise_on_error: :none`, so only a request that was not
/// answered at all is an `Err`.
async fn fetch_object(state: &AppState, uri: &str) -> anyhow::Result<Option<Value>> {
    json_ld::fetch_resource(state, uri, None, json_ld::RaiseOn::None).await
}

/// `ActivityPub::FetchAllRepliesService#filter_replies`: of the replies a
/// collection lists, plus the replies to that status we hold but would not be
/// told about if they changed, those worth fetching — not ones we have that
/// are local, new, or fetched recently. Those we have and will fetch have
/// their `fetched_replies_at` touched.
async fn filter_all_replies(state: &AppState, status_uri: &str, items: &[Value]) -> Vec<String> {
    let mut uris: Vec<String> = items
        .iter()
        .filter_map(value_or_id)
        .map(str::to_owned)
        .collect();

    let parent_id = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
        status_uri,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    if let Some(parent_id) = parent_id {
        // `Status.unsubscribed`: remote statuses whose author has a remote
        // follower, a local follow newer than the status's last update, or no
        // followers at all.
        let unsubscribed = sqlx::query_scalar!(
            r#"SELECT DISTINCT s.uri AS "uri!" FROM statuses s
                 LEFT JOIN follows f ON f.target_account_id = s.account_id
                 LEFT JOIN accounts fa ON fa.id = f.account_id
               WHERE s.in_reply_to_id = $1 AND s.deleted_at IS NULL
                 AND s.local = false AND s.uri IS NOT NULL
                 AND NOT (s.uri = ANY($2::text[]))
                 AND (fa.domain IS NOT NULL
                      OR NOT (f.created_at < s.updated_at)
                      OR f.id IS NULL)"#,
            parent_id,
            &uris,
        )
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
        uris.extend(unsubscribed);
    }

    // `should_not_fetch_replies`: local, created recently, or fetched recently.
    let dont_update: HashSet<String> = sqlx::query_scalar!(
        r#"SELECT uri AS "uri!" FROM statuses
           WHERE uri = ANY($1::text[]) AND deleted_at IS NULL
             AND (COALESCE(local, false) OR uri IS NULL
                  OR created_at >= now() - make_interval(mins => $2)
                  OR fetched_replies_at >= now() - make_interval(mins => $3))"#,
        &uris,
        FETCH_REPLIES_INITIAL_WAIT_MINUTES as i32,
        FETCH_REPLIES_COOLDOWN_MINUTES as i32,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .collect();

    // `should_fetch_replies.touch_all(:fetched_replies_at)`, which moves
    // `updated_at` with it.
    let _ = sqlx::query!(
        r#"UPDATE statuses SET fetched_replies_at = now(), updated_at = now()
           WHERE uri = ANY($1::text[]) AND deleted_at IS NULL
             AND local = false AND uri IS NOT NULL
             AND created_at <= now() - make_interval(mins => $2)
             AND (fetched_replies_at IS NULL
                  OR fetched_replies_at <= now() - make_interval(mins => $3))"#,
        &uris,
        FETCH_REPLIES_INITIAL_WAIT_MINUTES as i32,
        FETCH_REPLIES_COOLDOWN_MINUTES as i32,
    )
    .execute(&state.db)
    .await;

    let mut seen = HashSet::new();
    uris.into_iter()
        .filter(|uri| !dont_update.contains(uri) && seen.insert(uri.clone()))
        .take(MAX_REPLIES_PER_STATUS)
        .collect()
}

/// `FetchReplyWorker.push_bulk`: one job for each of `uris`, in the batch
/// `batch_id` when there is one — joined before the jobs are queued.
async fn push_fetch_reply_workers(
    state: &AppState,
    uris: &[String],
    batch_id: Option<&str>,
    request_id: Option<&str>,
) {
    let jids: Vec<Option<String>> = uris
        .iter()
        .map(|_| batch_id.map(|_| crate::worker_batch::random_id()))
        .collect();
    if let Some(batch_id) = batch_id {
        let batch = crate::worker_batch::WorkerBatch::new(Some(batch_id.to_owned()));
        let jids: Vec<String> = jids.iter().flatten().cloned().collect();
        batch.add_jobs(state, &jids).await;
    }
    for (uri, jid) in uris.iter().zip(jids) {
        crate::jobs::push(
            state,
            FetchReplyWorker {
                url: uri.clone(),
                prefetched_body: None,
                request_id: request_id.map(str::to_owned),
                batch_id: batch_id.map(str::to_owned),
                jid,
            },
        )
        .await;
    }
}

/// `FetchAllRepliesWorker#get_replies` → `FetchAllRepliesService#call`: the
/// replies worth fetching from one status's collection, each queued for a
/// `FetchReplyWorker` in the walk's batch, and the pages read. A collection
/// page that fails for the time being fails the walk, to be retried, as it
/// raises out of Mastodon's worker.
async fn get_replies(
    state: &AppState,
    status_uri: &str,
    status_json: &Value,
    max_pages: usize,
    batch_id: Option<&str>,
) -> anyhow::Result<Option<(Vec<String>, usize)>> {
    let Some(collection) = status_json.get("replies").filter(|r| !r.is_null()) else {
        return Ok(None);
    };
    let Some((items, n_pages)) = json_ld::collection_items(
        state,
        collection,
        Some(max_pages),
        Some(COLLECTION_MAX_ITEMS),
        status_uri,
        None,
    )
    .await?
    else {
        return Ok(None);
    };
    let uris = filter_all_replies(state, status_uri, &items).await;
    push_fetch_reply_workers(state, &uris, batch_id, None).await;
    Ok(Some((uris, n_pages)))
}

/// The context controller's `WorkerBatch.new.within { |batch|
/// batch.connect(refresh_key, threshold: 1.0); ActivityPub::
/// FetchAllRepliesWorker.perform_async(root_status_id, { 'batch_id' =>
/// batch.id }) }`: the walk and every reply it queues are one batch, whose
/// end finishes the async refresh named `refresh_key`.
pub async fn fetch_all_replies(state: &AppState, root_status_id: i64, refresh_key: String) {
    let batch = crate::worker_batch::WorkerBatch::new(None);
    batch.connect(state, &refresh_key, 1.0).await;
    let jid = crate::worker_batch::random_id();
    batch.add_jobs(state, std::slice::from_ref(&jid)).await;
    crate::jobs::push(
        state,
        FetchAllRepliesWorker {
            root_status_id,
            batch_id: Some(batch.id),
            jid: Some(jid),
            refresh_key: None,
        },
    )
    .await;
}

/// The options all three reply workers share: `queue: 'pull', retry: 3`.
const REPLIES_OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
    .queue(crate::jobs::Queue::Pull)
    .retry(3);

/// `ActivityPub::FetchAllRepliesWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct FetchAllRepliesWorker {
    pub root_status_id: i64,
    #[serde(default)]
    pub batch_id: Option<String>,
    #[serde(default)]
    pub jid: Option<String>,
    /// The refresh a walk queued before walks had batches finishes itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_key: Option<String>,
}

impl crate::jobs::Job for FetchAllRepliesWorker {
    const KIND: &'static str = "ActivityPub::FetchAllRepliesWorker";
    const OPTIONS: crate::jobs::Options = REPLIES_OPTIONS;

    fn retry_in(count: u32) -> Option<std::time::Duration> {
        crate::jobs::exponential_backoff(count)
    }

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        let membership = crate::worker_batch::Membership::new(
            state,
            self.batch_id.as_deref(),
            self.jid.as_deref(),
        );
        let legacy = self
            .refresh_key
            .as_deref()
            .map(|key| crate::async_refresh::FinishOnDrop::new(state, key));
        let walked = walk_replies(state, self.root_status_id, self.batch_id.as_deref()).await;
        if let Some(membership) = membership {
            membership.leave(false).await;
        }
        if let Some(legacy) = legacy {
            legacy.finish().await;
        }
        walked
    }
}

/// `ActivityPub::FetchRepliesWorker`: the first page of a new status's
/// replies.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct FetchRepliesWorker {
    pub account_uri: String,
    pub collection: Value,
    #[serde(default)]
    pub request_id: Option<String>,
}

impl crate::jobs::Job for FetchRepliesWorker {
    const KIND: &'static str = "ActivityPub::FetchRepliesWorker";
    const OPTIONS: crate::jobs::Options = REPLIES_OPTIONS;

    fn retry_in(count: u32) -> Option<std::time::Duration> {
        crate::jobs::exponential_backoff(count)
    }

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        fetch_replies(
            state,
            &self.account_uri,
            &self.collection,
            self.request_id.as_deref(),
        )
        .await
    }
}

/// `FetchReplyWorker`: fetch one status — or take the document already
/// fetched — and process it as `FetchRemoteStatusService` does, as the
/// `Create` it would have come in or, when we hold it, as an `Update`. It
/// leaves its batch however it ends, counting in the batch's refresh when
/// the status was new to us.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct FetchReplyWorker {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefetched_body: Option<Value>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub batch_id: Option<String>,
    #[serde(default)]
    pub jid: Option<String>,
}

impl crate::jobs::Job for FetchReplyWorker {
    const KIND: &'static str = "FetchReplyWorker";
    const OPTIONS: crate::jobs::Options = REPLIES_OPTIONS;

    fn retry_in(count: u32) -> Option<std::time::Duration> {
        crate::jobs::exponential_backoff(count)
    }

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        let membership = crate::worker_batch::Membership::new(
            state,
            self.batch_id.as_deref(),
            self.jid.as_deref(),
        );
        let result = crate::api::ap::inbox::fetch_remote_status_by_url(
            state,
            &self.url,
            self.prefetched_body,
            self.request_id,
        )
        .await;
        let created = matches!(result, Ok(Some((_, true))));
        if let Some(membership) = membership {
            membership.leave(created).await;
        }
        result
            .map(|_| ())
            .map_err(|error| anyhow::anyhow!("could not process {}: {error}", self.url))
    }
}

/// The walk. Fails, to be retried, where Mastodon's raises: when a request
/// for the root status is not answered at all (a status the server answers
/// with, success or not, is not retried), and when a collection page fails
/// for the time being. Each reply it finds is queued for a
/// `FetchReplyWorker` in `batch_id` as it is found; the walk itself fetches
/// a reply only to read its collection, and passes over one it cannot.
async fn walk_replies(
    state: &AppState,
    root_status_id: i64,
    batch_id: Option<&str>,
) -> anyhow::Result<()> {
    // `@root_status&.should_fetch_replies?` and `touch(:fetched_replies_at)`,
    // in one statement so that two requests cannot both start the walk.
    let root_uri = sqlx::query_scalar!(
        r#"UPDATE statuses SET fetched_replies_at = now(), updated_at = now()
           WHERE id = $1 AND deleted_at IS NULL
             AND local = false AND uri IS NOT NULL
             AND visibility IN (0, 1)
             AND created_at <= now() - make_interval(mins => $2)
             AND (fetched_replies_at IS NULL
                  OR fetched_replies_at <= now() - make_interval(mins => $3))
           RETURNING uri AS "uri!""#,
        root_status_id,
        FETCH_REPLIES_INITIAL_WAIT_MINUTES as i32,
        FETCH_REPLIES_COOLDOWN_MINUTES as i32,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let Some(root_uri) = root_uri else {
        return Ok(());
    };

    // `get_root_replies`: the root is refreshed from the same document, by a
    // `FetchReplyWorker` outside the batch.
    let Some(root_json) = fetch_object(state, &root_uri).await? else {
        return Ok(());
    };
    crate::jobs::push(
        state,
        FetchReplyWorker {
            url: root_uri.clone(),
            prefetched_body: Some(root_json.clone()),
            request_id: None,
            batch_id: None,
            jid: None,
        },
    )
    .await;
    let Some((mut to_fetch, mut n_pages)) =
        get_replies(state, &root_uri, &root_json, MAX_PAGES, batch_id).await?
    else {
        return Ok(());
    };
    let mut discovered: HashSet<String> = to_fetch.iter().cloned().collect();

    while discovered.len() < MAX_REPLIES && n_pages < MAX_PAGES {
        let Some(next) = to_fetch.pop() else {
            break;
        };
        // `get_replies_uri`: the reply is fetched to read its collection;
        // storing it is its `FetchReplyWorker`'s job.
        let Some(json) = fetch_object(state, &next).await.ok().flatten() else {
            continue;
        };
        let Some((replies, pages)) =
            get_replies(state, &next, &json, MAX_PAGES - n_pages, batch_id).await?
        else {
            continue;
        };
        let new: Vec<String> = replies
            .into_iter()
            .filter(|uri| discovered.insert(uri.clone()))
            .collect();
        to_fetch.extend(new);
        n_pages += pages;
    }

    tracing::debug!(
        root = root_uri,
        replies = discovered.len(),
        "fetched replies"
    );
    Ok(())
}

/// `ActivityPub::Activity::Create#fetch_replies`: on a new remote status,
/// queue the read of the first page of its `replies`, from the author's
/// server, carrying the `request_id` the status was processed under.
pub async fn fetch_replies_on_create(
    state: &AppState,
    account_uri: String,
    collection: Value,
    request_id: Option<String>,
) {
    if !is_present(&collection) {
        return;
    }
    crate::jobs::push(
        state,
        FetchRepliesWorker {
            account_uri,
            collection,
            request_id,
        },
    )
    .await;
}

/// `ActivityPub::FetchRepliesService#call`: up to five replies from the first
/// page of a collection, on the author's server, each queued for a
/// `FetchReplyWorker` — which processes one we already hold as an `Update`.
async fn fetch_replies(
    state: &AppState,
    account_uri: &str,
    collection: &Value,
    request_id: Option<&str>,
) -> anyhow::Result<()> {
    let Some((items, _)) = json_ld::collection_items(
        state,
        collection,
        Some(1),
        Some(COLLECTION_MAX_ITEMS),
        account_uri,
        None,
    )
    .await?
    else {
        return Ok(());
    };
    let uris: Vec<String> = items
        .iter()
        .filter_map(value_or_id)
        .filter(|uri| !non_matching_uri_hosts(account_uri, uri))
        .take(COLLECTION_MAX_ITEMS)
        .map(str::to_owned)
        .collect();
    push_fetch_reply_workers(state, &uris, None, request_id).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_match_by_host_alone() {
        assert!(!non_matching_uri_hosts(
            "https://a.example/users/x",
            "https://A.example:8443/notes/1"
        ));
        assert!(non_matching_uri_hosts(
            "https://a.example/users/x",
            "https://b.example/notes/1"
        ));
        assert!(non_matching_uri_hosts(
            "https://a.example/users/x",
            "ftp://a.example/notes/1"
        ));
    }
}
