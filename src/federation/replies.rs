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
//! Mastodon splits the second into a worker that walks the tree and one
//! `FetchReplyWorker` job per reply, which fetches the reply again to store
//! it. Here one task does both with the same request: the reply's document is
//! what is stored and also where its own `replies` collection is read from.

use std::collections::HashSet;

use serde_json::Value;

use crate::db::models::Status as DbStatus;
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

/// The id of an item that is either a URI or an object carrying one
/// (`JsonLdHelper#value_or_id`).
fn value_or_id(value: &Value) -> Option<&str> {
    match value {
        Value::String(s) => Some(s.as_str()),
        Value::Object(o) => o.get("id").and_then(Value::as_str),
        _ => None,
    }
}

/// `JsonLdHelper#non_matching_uri_hosts?`: true unless both are HTTP(S) URIs
/// on the same host.
pub(crate) fn non_matching_uri_hosts(base: &str, comparison: &str) -> bool {
    let host = |uri: &str| {
        url::Url::parse(uri)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .and_then(|u| u.host_str().map(str::to_lowercase))
    };
    match (host(base), host(comparison)) {
        (Some(a), Some(b)) => a != b,
        _ => true,
    }
}

/// `JsonLdHelper#fetch_collection_page`: an embedded page as it is, or a
/// linked one fetched — only from `reference_uri`'s host, when one is given.
pub(crate) async fn fetch_collection_page(
    state: &AppState,
    collection_or_uri: &Value,
    reference_uri: Option<&str>,
) -> Option<Value> {
    match collection_or_uri {
        Value::Object(_) => Some(collection_or_uri.clone()),
        Value::String(uri) => {
            if reference_uri.is_some_and(|reference| non_matching_uri_hosts(reference, uri)) {
                return None;
            }
            crate::federation::fetch::signed_get_json(state, uri)
                .await
                .inspect_err(|error| tracing::debug!(uri, %error, "could not fetch replies page"))
                .ok()
                .filter(Value::is_object)
        }
        _ => None,
    }
}

/// `JsonLdHelper#collection_page_items`.
pub(crate) fn collection_page_items(collection: &Value) -> Vec<Value> {
    let items = match collection.get("type").and_then(Value::as_str) {
        Some("Collection" | "CollectionPage") => collection.get("items"),
        Some("OrderedCollection" | "OrderedCollectionPage") => collection.get("orderedItems"),
        _ => None,
    };
    match items {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Null) | None => Vec::new(),
        Some(item) => vec![item.clone()],
    }
}

/// `JsonLdHelper#collection_items`: the items of a collection, page after
/// page until `max_items` have been gathered or `max_pages` read, with the
/// page count Mastodon reports — which counts one more than was fetched when
/// the collection runs out first.
pub(crate) async fn collection_items(
    state: &AppState,
    collection_or_uri: &Value,
    max_pages: usize,
    max_items: usize,
    reference_uri: Option<&str>,
) -> Option<(Vec<Value>, usize)> {
    let mut collection = fetch_collection_page(state, collection_or_uri, reference_uri).await?;
    if let Some(first) = collection.get("first").filter(|f| is_present(f)).cloned() {
        collection = fetch_collection_page(state, &first, reference_uri).await?;
    }
    let mut items = Vec::new();
    let mut n_pages = 1;
    let mut page = Some(collection);
    while let Some(current) = page {
        items.extend(collection_page_items(&current));
        if items.len() >= max_items || n_pages >= max_pages {
            break;
        }
        page = match current.get("next").filter(|n| is_present(n)) {
            Some(next) => fetch_collection_page(state, next, reference_uri).await,
            None => None,
        };
        n_pages += 1;
    }
    Some((items, n_pages))
}

/// Rails' `present?` for a JSON value.
pub(crate) fn is_present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(s) => !s.trim().is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Array(a) => !a.is_empty(),
        _ => true,
    }
}

/// Fetch `uri` as Mastodon's `fetch_resource(uri, true)` does: the document
/// only if it says it is the object at that id.
async fn fetch_object(state: &AppState, uri: &str) -> Option<Value> {
    if crate::federation::moderation::actor_is_suspended(state, uri).await {
        return None;
    }
    let json = crate::federation::fetch::signed_get_json(state, uri)
        .await
        .inspect_err(|error| tracing::debug!(uri, %error, "could not fetch reply"))
        .ok()?;
    let id = json.get("id").and_then(Value::as_str)?;
    let canonical = crate::federation::portable::canonical;
    (id == uri || canonical(id) == canonical(uri)).then_some(json)
}

/// Store a fetched status, and say whether it was new to us.
async fn store(state: &AppState, uri: &str, json: Value) -> bool {
    match crate::api::ap::inbox::store_remote_status_prefetched(state, uri, json).await {
        Ok(Some((_, created))) => created,
        Ok(None) => false,
        Err(error) => {
            tracing::debug!(uri, %error, "could not store fetched reply");
            false
        }
    }
}

/// Fetch `uri` and store it, counting it in the refresh if it was new.
async fn fetch_reply(state: &AppState, uri: &str, refresh_key: Option<&str>) -> Option<Value> {
    let json = fetch_object(state, uri).await?;
    if store(state, uri, json.clone()).await {
        if let Some(key) = refresh_key {
            crate::async_refresh::increment_result_count(state, key, 1).await;
        }
    }
    Some(json)
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

/// `FetchAllRepliesWorker#get_replies` → `FetchAllRepliesService#call`: the
/// replies worth fetching from one status's collection, and the pages read.
async fn get_replies(
    state: &AppState,
    status_uri: &str,
    status_json: &Value,
    max_pages: usize,
) -> Option<(Vec<String>, usize)> {
    let collection = status_json.get("replies").filter(|r| !r.is_null())?;
    let (items, n_pages) = collection_items(
        state,
        collection,
        max_pages,
        COLLECTION_MAX_ITEMS,
        Some(status_uri),
    )
    .await?;
    Some((filter_all_replies(state, status_uri, &items).await, n_pages))
}

/// `ActivityPub::FetchAllRepliesWorker#perform`, finishing the async refresh
/// named `refresh_key` when it is done.
pub async fn fetch_all_replies(state: AppState, root_status_id: i64, refresh_key: String) {
    let guard = crate::async_refresh::FinishOnDrop::new(&state, &refresh_key);
    walk_replies(&state, root_status_id, &refresh_key).await;
    guard.finish().await;
}

async fn walk_replies(state: &AppState, root_status_id: i64, refresh_key: &str) {
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
        return;
    };

    // `get_root_replies`.
    let Some(root_json) = fetch_object(state, &root_uri).await else {
        return;
    };
    let Some((mut to_fetch, mut n_pages)) =
        get_replies(state, &root_uri, &root_json, MAX_PAGES).await
    else {
        return;
    };
    let mut discovered: HashSet<String> = to_fetch.iter().cloned().collect();

    while discovered.len() < MAX_REPLIES && n_pages < MAX_PAGES {
        let Some(next) = to_fetch.pop() else {
            break;
        };
        // Storing the reply is `FetchReplyWorker`'s job; reading its
        // collection from the same document is the walk's.
        let Some(json) = fetch_reply(state, &next, Some(refresh_key)).await else {
            continue;
        };
        let Some((replies, pages)) = get_replies(state, &next, &json, MAX_PAGES - n_pages).await
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

    // Replies discovered but not walked to are still stored, as Mastodon
    // queued a `FetchReplyWorker` for each when it found them.
    for uri in to_fetch {
        fetch_reply(state, &uri, Some(refresh_key)).await;
    }
    tracing::debug!(
        root = root_uri,
        replies = discovered.len(),
        "fetched replies"
    );
}

/// `ActivityPub::Activity::Create#fetch_replies` and `ActivityPub::
/// FetchRepliesService`: on a new remote status, fetch up to five replies
/// from the first page of its `replies`, from the author's server.
pub async fn fetch_replies_on_create(state: AppState, account_uri: String, collection: Value) {
    if !is_present(&collection) {
        return;
    }
    let Some((items, _)) = collection_items(
        &state,
        &collection,
        1,
        COLLECTION_MAX_ITEMS,
        Some(&account_uri),
    )
    .await
    else {
        return;
    };
    let uris: Vec<String> = items
        .iter()
        .filter_map(value_or_id)
        .filter(|uri| !non_matching_uri_hosts(&account_uri, uri))
        .take(COLLECTION_MAX_ITEMS)
        .map(str::to_owned)
        .collect();
    for uri in uris {
        // A status already held is one Mastodon would fetch again only to
        // refresh it, which eunha's store does not do.
        let known = sqlx::query_scalar!(
            "SELECT id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
            uri,
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
        if known.is_some()
            || crate::federation::local_uri::status(&state, &uri)
                .await
                .is_some()
        {
            continue;
        }
        fetch_reply(&state, &uri, None).await;
    }
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

    #[test]
    fn page_items_follow_the_page_type() {
        let page = serde_json::json!({"type": "OrderedCollectionPage", "orderedItems": ["a", {"id": "b"}]});
        let items = collection_page_items(&page);
        assert_eq!(
            items.iter().filter_map(value_or_id).collect::<Vec<_>>(),
            ["a", "b"]
        );
        let unordered = serde_json::json!({"type": "CollectionPage", "orderedItems": ["a"]});
        assert!(collection_page_items(&unordered).is_empty());
    }
}
