//! Followers collection synchronization (FEP-8fcf), as Mastodon does it.
//!
//! A local account's followers-only post goes out with a
//! `Collection-Synchronization` header naming the digest of its followers
//! on the receiving server (`ActivityPub::DeliveryWorker#synchronization_header`,
//! `Account#remote_followers_hash`), and that server can list them at
//! `/users/{username}/followers_synchronization`
//! (`ActivityPub::FollowersSynchronizationsController`). When a remote
//! account's delivery says its followers here are not who eunha thinks
//! (`ActivityPub::PrepareFollowersSynchronizationService`), its list is
//! fetched and our side brought in line with it
//! (`ActivityPub::FollowersSynchronizationWorker`,
//! `ActivityPub::SynchronizeFollowersService`). The digest and the header
//! are ojak's (`ojak::synchronization`).
//!
//! Both digests are cached as `Rails.cache` keeps them, for ten minutes and
//! until a follow of the account from that server comes or goes
//! (`Follow#invalidate_hash_cache`), in Redis under the instance's key
//! prefix. Mastodon's entries are Marshal-encoded under its `cache:`
//! namespace, so eunha keeps its own, the hex digest as it is, at the same
//! name without the namespace: `followers_hash:<id>:<prefix>/` and
//! `followers_hash:<id>:local`.
//!
//! An instance with `disable_followers_synchronization` set, Mastodon's
//! `DISABLE_FOLLOWERS_SYNCHRONIZATION=true`, neither sends the header nor
//! acts on one it receives.

use ojak::synchronization::{CollectionSynchronization, Digest};
use serde_json::{json, Value};

use crate::error::AppResult;
use crate::redis_keys::RedisKeyspace;
use crate::state::AppState;

/// How long a digest is kept: the `expires_in: 10.minutes` of Mastodon's
/// cache store.
const CACHE_EXPIRES_IN_SECS: u64 = 10 * 60;

/// The `Rails.cache` the digests are kept in: the instance's Redis, under
/// its key prefix.
#[derive(Clone)]
pub struct DigestCache {
    redis: redis::aio::ConnectionManager,
    keys: RedisKeyspace,
}

impl DigestCache {
    #[must_use]
    pub fn new(redis: redis::aio::ConnectionManager, keys: RedisKeyspace) -> Self {
        Self { redis, keys }
    }

    /// The instance's cache.
    #[must_use]
    pub fn of(state: &AppState) -> Self {
        Self::new(state.redis.clone(), state.redis_keys.clone())
    }

    /// `Rails.cache.fetch(key) { compute }`: what is kept under `key`, or
    /// else `compute`'s answer, kept for ten minutes. Redis failing to
    /// answer is a miss, and a digest it fails to keep is still returned.
    async fn fetch<F>(&self, key: &str, compute: F) -> sqlx::Result<String>
    where
        F: std::future::Future<Output = sqlx::Result<String>>,
    {
        let key = self.keys.key(key);
        let mut redis = self.redis.clone();
        let hit: Option<String> = redis::cmd("GET")
            .arg(&key)
            .query_async(&mut redis)
            .await
            .ok()
            .flatten();
        if let Some(hit) = hit {
            return Ok(hit);
        }
        let digest = compute.await?;
        let kept: redis::RedisResult<()> = redis::cmd("SET")
            .arg(&key)
            .arg(&digest)
            .arg("EX")
            .arg(CACHE_EXPIRES_IN_SECS)
            .query_async(&mut redis)
            .await;
        if let Err(error) = kept {
            tracing::debug!(%error, "followers digest not cached");
        }
        Ok(digest)
    }

    /// `Rails.cache.delete` of each of `keys`.
    async fn delete(&self, keys: &[String]) {
        if keys.is_empty() {
            return;
        }
        let mut redis = self.redis.clone();
        let mut del = redis::cmd("DEL");
        for key in keys {
            del.arg(self.keys.key(key));
        }
        if let Err(error) = del.query_async::<()>(&mut redis).await {
            tracing::warn!(%error, "followers digests not forgotten");
        }
    }
}

/// `Account#synchronization_uri_prefix`: `local` for a local account, else
/// its URI's `http(s)://host[:port]` and a slash.
fn synchronization_uri_prefix(local: bool, uri: Option<&str>) -> String {
    if local {
        return "local".to_owned();
    }
    format!("{}/", uri.and_then(url_prefix).unwrap_or_default())
}

/// `Follow#invalidate_hash_cache`, run once a follow of `target` by
/// `follower` is made or undone: the target's digest of its followers on
/// the follower's server is forgotten, unless both are local.
pub async fn follow_changed(state: &AppState, follower: i64, target: i64) {
    follows_changed(state, &[(follower, target)]).await;
}

/// [`follow_changed`] for each `(follower, target)` of `follows`, while
/// both accounts are still there to be read.
pub async fn follows_changed(state: &AppState, follows: &[(i64, i64)]) {
    if follows.is_empty() {
        return;
    }
    let (followers, targets): (Vec<i64>, Vec<i64>) = follows.iter().copied().unzip();
    let rows = sqlx::query!(
        r#"SELECT p.target AS "target!", f.domain IS NULL AS "follower_local!", f.uri AS follower_uri,
                  t.domain IS NULL AS "target_local!"
           FROM unnest($1::bigint[], $2::bigint[]) AS p(follower, target)
           JOIN accounts f ON f.id = p.follower
           JOIN accounts t ON t.id = p.target"#,
        &followers,
        &targets,
    )
    .fetch_all(&state.db)
    .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "followers digests not forgotten");
            return;
        }
    };
    let keys: Vec<String> = rows
        .iter()
        .filter(|row| !(row.follower_local && row.target_local))
        .map(|row| {
            format!(
                "followers_hash:{}:{}",
                row.target,
                synchronization_uri_prefix(row.follower_local, row.follower_uri.as_deref())
            )
        })
        .collect();
    DigestCache::of(state).delete(&keys).await;
}

/// The follows of and by `account_id`, for [`follows_changed`] before they
/// are deleted together.
pub async fn follows_involving(state: &AppState, account_id: i64) -> Vec<(i64, i64)> {
    sqlx::query!(
        "SELECT account_id, target_account_id FROM follows
         WHERE account_id = $1 OR target_account_id = $1",
        account_id,
    )
    .fetch_all(&state.db)
    .await
    .map(|rows| {
        rows.into_iter()
            .map(|row| (row.account_id, row.target_account_id))
            .collect()
    })
    .unwrap_or_default()
}

/// `Rails.cache.delete_matched("followers_hash:#{id}:*")`, for an account
/// another was merged into (`Account::Merging#merge_with!`).
pub async fn forget_all(state: &AppState, account_id: i64) {
    let pattern = state
        .redis_keys
        .key(format!("followers_hash:{account_id}:*"));
    let mut redis = state.redis.clone();
    let mut cursor: u64 = 0;
    loop {
        let page: redis::RedisResult<(u64, Vec<String>)> = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(1000)
            .query_async(&mut redis)
            .await;
        let Ok((next, keys)) = page else {
            return;
        };
        if !keys.is_empty() {
            let _: redis::RedisResult<()> =
                redis::cmd("DEL").arg(&keys).query_async(&mut redis).await;
        }
        if next == 0 {
            return;
        }
        cursor = next;
    }
}

/// `ActivityPub::DistributionWorker::MAX_FOLLOWERS_FOR_SYNCHRONIZATION`.
const MAX_FOLLOWERS_FOR_SYNCHRONIZATION: i64 = 25_000;

/// `ActivityPub::SynchronizeFollowersService::MAX_COLLECTION_PAGES`.
const MAX_COLLECTION_PAGES: usize = 10;

/// `Account::URL_PREFIX_RE`: a URL's `http(s)://host[:port]`.
#[must_use]
pub fn url_prefix(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host_end = rest.find('/').unwrap_or(rest.len());
    if host_end == 0 {
        return None;
    }
    Some(&url[..url.len() - rest.len() + host_end])
}

/// `ActivityPub::DistributionWorker#options`: whether a status's delivery
/// asks its receivers to check the author's followers there, which only a
/// followers-only post of an account with fewer than 25,000 followers does.
pub async fn synchronizes(state: &AppState, account_id: i64, visibility: i32) -> bool {
    if visibility != crate::db::models::vis::PRIVATE {
        return false;
    }
    let followers = sqlx::query_scalar!(
        "SELECT followers_count FROM account_stats WHERE account_id = $1",
        account_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .unwrap_or(0);
    followers < MAX_FOLLOWERS_FOR_SYNCHRONIZATION
}

/// The URIs of `account_id`'s followers whose URIs are on `prefix`
/// (`followers.matches_uri_prefix`).
async fn followers_on(
    db: &sqlx::PgPool,
    account_id: i64,
    prefix: &str,
) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar!(
        r#"SELECT a.uri AS "uri!" FROM follows f JOIN accounts a ON a.id = f.account_id
           WHERE f.target_account_id = $1 AND a.uri IS NOT NULL
             AND (a.uri = $2 OR left(a.uri, length($2) + 1) = $2 || '/')"#,
        account_id,
        prefix,
    )
    .fetch_all(db)
    .await
}

/// `Account#remote_followers_hash(url)`: the digest of the account's
/// followers on `url`'s server, or `None` when it names none, cached at
/// `followers_hash:<id>:<prefix>/`.
pub async fn remote_followers_hash(
    db: &sqlx::PgPool,
    cache: &DigestCache,
    account_id: i64,
    url: &str,
) -> sqlx::Result<Option<String>> {
    let Some(prefix) = url_prefix(url) else {
        return Ok(None);
    };
    let digest = cache
        .fetch(&format!("followers_hash:{account_id}:{prefix}/"), async {
            let uris = followers_on(db, account_id, prefix).await?;
            Ok(Digest::of(uris.iter().map(String::as_str)).to_hex())
        })
        .await?;
    Ok(Some(digest))
}

/// `Account#local_followers_hash`: the digest of a remote account's local
/// followers, each by the URI it has here, cached at
/// `followers_hash:<id>:local`.
pub async fn local_followers_hash(state: &AppState, account_id: i64) -> sqlx::Result<String> {
    DigestCache::of(state)
        .fetch(&format!("followers_hash:{account_id}:local"), async {
            let domain = &state.instance.domain;
            let followers = sqlx::query!(
                r#"SELECT a.id, a.id_scheme, a.username FROM follows f JOIN accounts a ON a.id = f.account_id
                   WHERE f.target_account_id = $1 AND a.domain IS NULL"#,
                account_id,
            )
            .fetch_all(&state.db)
            .await?;
            let uris: Vec<String> = followers
                .iter()
                .map(|a| {
                    crate::federation::tag::account_uri(domain, a.id, a.id_scheme, &a.username)
                })
                .collect();
            Ok(Digest::of(uris.iter().map(String::as_str)).to_hex())
        })
        .await
}

/// `ActivityPub::DeliveryWorker#synchronization_header`, for the local
/// account `account_id` of the instance at `domain` delivering to `inbox`:
/// its followers collection, the digest of its followers on the inbox's
/// server, and where they are listed (`account_followers_synchronization_url`,
/// the username route whichever scheme the account uses).
pub async fn header_for(
    db: &sqlx::PgPool,
    cache: &DigestCache,
    domain: &str,
    account_id: i64,
    inbox: &str,
) -> Option<String> {
    let account = sqlx::query!(
        "SELECT id, id_scheme, username FROM accounts WHERE id = $1 AND domain IS NULL",
        account_id,
    )
    .fetch_optional(db)
    .await
    .ok()??;
    let digest = remote_followers_hash(db, cache, account.id, inbox)
        .await
        .ok()??;
    let uris = crate::api::ap::serving::uris(domain).ok()?;
    let followers = crate::api::ap::serving::AccountUris::new(
        &uris,
        account.id,
        account.id_scheme,
        &account.username,
    )
    .uri(crate::api::ap::serving::Own::Followers)
    .ok()?;
    Some(
        CollectionSynchronization {
            collection_id: followers.into(),
            url: format!(
                "https://{domain}/users/{}/followers_synchronization",
                account.username
            ),
            digest,
        }
        .to_header(),
    )
}

/// `ActivityPub::FollowersSynchronizationsController#show`: the local
/// account's followers on the signer's server, by their URIs.
pub async fn document(
    state: &AppState,
    account: &crate::db::models::Account,
    signer: &str,
) -> AppResult<Value> {
    let prefix = url_prefix(signer).unwrap_or(signer);
    let items = followers_on(&state.db, account.id, prefix).await?;
    Ok(json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!(
            "https://{}/users/{}/followers_synchronization",
            state.instance.domain, account.username
        ),
        "type": "OrderedCollection",
        "orderedItems": items,
    }))
}

/// `InboxesController#process_collection_synchronization` and
/// `ActivityPub::PrepareFollowersSynchronizationService`: a delivery from
/// `signer` that says its followers here are not who eunha thinks has them
/// checked, later. A header that does not parse is passed over, and so is
/// every header when `disable_followers_synchronization` is set.
pub async fn prepare(state: &AppState, signer: &str, raw: &str) {
    if raw.trim().is_empty() || state.instance.disable_followers_synchronization {
        return;
    }
    let Some(params) = CollectionSynchronization::parse(raw) else {
        tracing::warn!("Error parsing Collection-Synchronization header");
        return;
    };
    let Ok(Some(account)) = sqlx::query!(
        "SELECT id, uri, followers_url FROM accounts WHERE uri = $1 AND domain IS NOT NULL LIMIT 1",
        signer,
    )
    .fetch_optional(&state.db)
    .await
    else {
        return;
    };
    let account_uri = account.uri.clone().unwrap_or_default();
    if params.collection_id != account.followers_url
        || crate::federation::json_ld::non_matching_uri_hosts(&account_uri, &params.url)
    {
        return;
    }
    match local_followers_hash(state, account.id).await {
        Ok(digest) if digest == params.digest => return,
        Ok(_) => {}
        Err(error) => {
            tracing::debug!(%error, "local followers not hashed");
            return;
        }
    }
    crate::jobs::push(
        state,
        FollowersSynchronizationWorker {
            account_id: account.id,
            url: params.url,
            expected_digest: Some(params.digest),
        },
    )
    .await;
}

/// `ActivityPub::FollowersSynchronizationWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct FollowersSynchronizationWorker {
    pub account_id: i64,
    pub url: String,
    #[serde(default)]
    pub expected_digest: Option<String>,
}

impl crate::jobs::Job for FollowersSynchronizationWorker {
    const KIND: &'static str = "ActivityPub::FollowersSynchronizationWorker";
    /// `sidekiq_options queue: 'push', lock: :until_executed`.
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Push)
        .lock(crate::jobs::Lock::UntilExecuted(
            crate::jobs::DEFAULT_LOCK_TTL,
        ));

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        synchronize(
            state,
            self.account_id,
            &self.url,
            self.expected_digest.as_deref(),
        )
        .await
    }
}

/// `JsonLdHelper#fetch_collection_page`: a page, fetched only from
/// `reference`'s host when one is given, as Mastodon fetches it
/// (`raise_on_error: :temporary`).
async fn fetch_page(
    state: &AppState,
    page: &Value,
    reference: Option<&str>,
) -> anyhow::Result<Option<Value>> {
    let uri = match page {
        Value::String(uri) => uri.clone(),
        Value::Object(_) => return Ok(Some(page.clone())),
        _ => return Ok(None),
    };
    if reference.is_some_and(|reference| {
        crate::federation::json_ld::non_matching_uri_hosts(reference, &uri)
    }) {
        return Ok(None);
    }
    crate::federation::json_ld::fetch_resource_without_id_validation(
        state,
        &uri,
        None,
        crate::federation::json_ld::RaiseOn::Temporary,
    )
    .await
}

/// `collection_page_items`: a page's `orderedItems` or `items`, by their
/// IDs.
fn page_items(page: &Value) -> Vec<String> {
    let items = match page.get("type").and_then(Value::as_str) {
        Some("Collection" | "CollectionPage") => page.get("items"),
        Some("OrderedCollection" | "OrderedCollectionPage") => page.get("orderedItems"),
        _ => None,
    };
    let items = match items {
        Some(Value::Array(items)) => items.clone(),
        Some(item) if !item.is_null() => vec![item.clone()],
        _ => Vec::new(),
    };
    items
        .iter()
        .filter_map(|item| match item {
            Value::String(id) => Some(id.clone()),
            Value::Object(object) => object.get("id").and_then(Value::as_str).map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// `ActivityPub::SynchronizeFollowersService`: the remote account's
/// followers here, as its server lists them at `url`. A local account it
/// lists that does not follow it here has its follow request accepted, or
/// sends the `Undo` of a follow eunha never knew of; once the whole list is
/// read, and only when it adds up to the digest, a local account it does
/// not list stops following it.
pub async fn synchronize(
    state: &AppState,
    account_id: i64,
    url: &str,
    expected_digest: Option<&str>,
) -> anyhow::Result<()> {
    let Some(account) = sqlx::query!(
        "SELECT id, uri, inbox_url FROM accounts WHERE id = $1",
        account_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let account_uri = account.uri.clone().unwrap_or_default();
    let expected_digest = expected_digest.filter(|d| !d.is_empty());
    // A digest that is not 32 bytes of hex never adds up to nothing, so
    // nothing is removed for it.
    let malformed = expected_digest.is_some_and(|hex| Digest::from_hex(hex).is_none());
    let mut digest = expected_digest.and_then(Digest::from_hex);
    let mut expected: Vec<i64> = Vec::new();

    // `process_collection!`: only true when the whole collection was read.
    let complete = 'walk: {
        let Some(mut collection) =
            fetch_page(state, &Value::String(url.to_owned()), Some(&account_uri)).await?
        else {
            break 'walk false;
        };
        if let Some(first) = collection.get("first").filter(|f| !f.is_null()).cloned() {
            match fetch_page(state, &first, Some(&account_uri)).await? {
                Some(page) => collection = page,
                None => break 'walk false,
            }
        }
        let mut pages_left = MAX_COLLECTION_PAGES;
        loop {
            let items = page_items(&collection);
            let mut page_followers = Vec::new();
            for uri in &items {
                if let Some(digest) = digest.as_mut() {
                    digest.add(uri);
                }
                if crate::federation::local_uri::is_local(state, uri) {
                    if let Some(id) = crate::federation::local_uri::account(state, uri).await {
                        page_followers.push(id);
                    }
                }
            }
            expected.extend(&page_followers);
            handle_unexpected_outgoing_follows(
                state,
                account.id,
                &account.inbox_url,
                &account_uri,
                &page_followers,
            )
            .await?;
            pages_left -= 1;
            let next = collection.get("next").filter(|n| match n {
                Value::String(s) => !s.is_empty(),
                Value::Null => false,
                _ => true,
            });
            let Some(next) = next.cloned() else {
                break 'walk true;
            };
            if pages_left == 0 {
                break 'walk false;
            }
            match fetch_page(state, &next, None).await? {
                Some(page) => collection = page,
                None => break 'walk false,
            }
        }
    };
    if !complete {
        return Ok(());
    }
    // Destructive, so only when the digests agree.
    if malformed || digest.as_ref().is_some_and(|digest| !digest.is_zero()) {
        return Ok(());
    }
    let unexpected = sqlx::query_scalar!(
        r#"SELECT f.account_id FROM follows f JOIN accounts a ON a.id = f.account_id
           WHERE f.target_account_id = $1 AND a.domain IS NULL AND NOT (f.account_id = ANY($2))"#,
        account.id,
        &expected,
    )
    .fetch_all(&state.db)
    .await?;
    for follower in unexpected {
        crate::api::mastodon::accounts::unfollow(state, follower, account.id, false)
            .await
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    }
    Ok(())
}

/// `handle_unexpected_outgoing_follows!`.
async fn handle_unexpected_outgoing_follows(
    state: &AppState,
    account_id: i64,
    inbox_url: &str,
    account_uri: &str,
    followers: &[i64],
) -> anyhow::Result<()> {
    for &follower in followers {
        let following = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2) AS "e!""#,
            follower,
            account_id,
        )
        .fetch_one(&state.db)
        .await?;
        if following {
            continue;
        }
        let request = sqlx::query_scalar!(
            "SELECT id FROM follow_requests WHERE account_id = $1 AND target_account_id = $2",
            follower,
            account_id,
        )
        .fetch_optional(&state.db)
        .await?;
        if let Some(request) = request {
            // The follow request went through, and its `Accept` was missed.
            crate::api::ap::inbox::follow::authorize_follow_request(state, request)
                .await
                .map_err(|error| anyhow::anyhow!("{error:?}"))?;
            continue;
        }
        // A follow eunha never knew of: its `Undo`, with no ID of the
        // `Follow` to name, as `UndoFollowSerializer` writes it for a
        // `Follow.new`.
        let Some(local) = sqlx::query!(
            "SELECT id, id_scheme, username FROM accounts WHERE id = $1 AND domain IS NULL",
            follower,
        )
        .fetch_optional(&state.db)
        .await?
        else {
            continue;
        };
        if inbox_url.is_empty() {
            continue;
        }
        let domain = &state.instance.domain;
        let actor =
            crate::federation::tag::account_uri(domain, local.id, local.id_scheme, &local.username);
        let undo = json!({
            "@context": "https://www.w3.org/ns/activitystreams",
            "id": format!("{actor}#follows//undo"),
            "type": "Undo",
            "actor": actor,
            "object": {
                "id": format!("{actor}#follows/"),
                "type": "Follow",
                "actor": actor,
                "object": account_uri,
            },
        });
        crate::federation::delivery::deliver_to_inboxes(
            state,
            undo,
            vec![inbox_url.to_owned()],
            format!("{actor}#main-key"),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_url_prefix_is_its_scheme_and_authority() {
        assert_eq!(
            super::url_prefix("https://a.test:8443/inbox"),
            Some("https://a.test:8443")
        );
        assert_eq!(super::url_prefix("http://a.test"), Some("http://a.test"));
        assert_eq!(super::url_prefix("ftp://a.test/x"), None);
        assert_eq!(super::url_prefix("https:///x"), None);
    }
}
