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

use ojak::synchronization::{CollectionSynchronization, Digest};
use serde_json::{json, Value};

use crate::error::AppResult;
use crate::state::AppState;

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
/// followers on `url`'s server, or `None` when it names none.
pub async fn remote_followers_hash(
    db: &sqlx::PgPool,
    account_id: i64,
    url: &str,
) -> sqlx::Result<Option<String>> {
    let Some(prefix) = url_prefix(url) else {
        return Ok(None);
    };
    let uris = followers_on(db, account_id, prefix).await?;
    Ok(Some(Digest::of(uris.iter().map(String::as_str)).to_hex()))
}

/// `Account#local_followers_hash`: the digest of a remote account's local
/// followers, each by the URI it has here.
pub async fn local_followers_hash(state: &AppState, account_id: i64) -> sqlx::Result<String> {
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
        .map(|a| crate::federation::tag::account_uri(domain, a.id, a.id_scheme, &a.username))
        .collect();
    Ok(Digest::of(uris.iter().map(String::as_str)).to_hex())
}

/// `ActivityPub::DeliveryWorker#synchronization_header`, for the local
/// account `account_id` of the instance at `domain` delivering to `inbox`:
/// its followers collection, the digest of its followers on the inbox's
/// server, and where they are listed (`account_followers_synchronization_url`,
/// the username route whichever scheme the account uses).
pub async fn header_for(
    db: &sqlx::PgPool,
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
    let digest = remote_followers_hash(db, account.id, inbox).await.ok()??;
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
/// checked, later. A header that does not parse is passed over.
pub async fn prepare(state: &AppState, signer: &str, raw: &str) {
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
