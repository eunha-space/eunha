//! A remote account's featured posts, hashtags and collections, synchronized
//! from the collections its actor names, as Mastodon's
//! `SynchronizeFeaturedCollectionWorker`, `SynchronizeFeaturedTagsCollectionWorker`
//! and `SynchronizeFeaturedCollectionsCollectionWorker` do after
//! `ProcessAccountService` stores the actor.

use anyhow::Result;
use serde_json::Value;

use crate::db::models::Account;
use crate::federation::json_ld::{self, non_matching_uri_hosts, supported_context, RaiseOn};
use crate::state::AppState;

/// `FeaturedTag::LIMIT`.
const FEATURED_TAG_LIMIT: usize = 10;
/// `FetchFeaturedCollectionsCollectionService::MAX_PAGES`.
const COLLECTIONS_MAX_PAGES: usize = 10;
/// `FetchFeaturedCollectionsCollectionService::MAX_ITEMS`.
const COLLECTIONS_MAX_ITEMS: usize = 50;
/// What the three synchronizing workers share: the `pull` queue and
/// `lock: :until_executed, lock_ttl: 1.day`.
const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
    .queue(crate::jobs::Queue::Pull)
    .lock(crate::jobs::Lock::UntilExecuted(
        std::time::Duration::from_secs(24 * 3600),
    ));

/// `ActivityPub::SynchronizeFeaturedCollectionWorker.perform_async(id,
/// { hashtag:, collection:, request_id: })`: `note: true`, and `hashtag`
/// when the actor has no `featuredTags` of its own.
pub async fn synchronize_featured_collection_later(
    state: &AppState,
    account_id: i64,
    collection: Option<String>,
    hashtag: bool,
    request_id: &str,
) {
    crate::jobs::push(
        state,
        SynchronizeFeaturedCollectionWorker {
            account_id,
            collection,
            hashtag,
            request_id: Some(request_id.to_owned()),
        },
    )
    .await;
}

/// `ActivityPub::SynchronizeFeaturedCollectionWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SynchronizeFeaturedCollectionWorker {
    pub account_id: i64,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub hashtag: bool,
    #[serde(default)]
    pub request_id: Option<String>,
}

impl crate::jobs::Job for SynchronizeFeaturedCollectionWorker {
    const KIND: &'static str = "ActivityPub::SynchronizeFeaturedCollectionWorker";
    const OPTIONS: crate::jobs::Options = OPTIONS;

    async fn perform(self, state: &AppState) -> Result<()> {
        fetch_featured_collection(
            state,
            self.account_id,
            self.collection.as_deref(),
            self.hashtag,
            self.request_id.as_deref(),
        )
        .await
    }
}

/// `ActivityPub::SynchronizeFeaturedTagsCollectionWorker.perform_async(id, url)`.
pub async fn synchronize_featured_tags_collection_later(
    state: &AppState,
    account_id: i64,
    url: Option<String>,
) {
    crate::jobs::push(
        state,
        SynchronizeFeaturedTagsCollectionWorker { account_id, url },
    )
    .await;
}

/// `ActivityPub::SynchronizeFeaturedTagsCollectionWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SynchronizeFeaturedTagsCollectionWorker {
    pub account_id: i64,
    #[serde(default)]
    pub url: Option<String>,
}

impl crate::jobs::Job for SynchronizeFeaturedTagsCollectionWorker {
    const KIND: &'static str = "ActivityPub::SynchronizeFeaturedTagsCollectionWorker";
    const OPTIONS: crate::jobs::Options = OPTIONS;

    async fn perform(self, state: &AppState) -> Result<()> {
        fetch_featured_tags_collection(state, self.account_id, self.url.as_deref()).await
    }
}

/// `ActivityPub::SynchronizeFeaturedCollectionsCollectionWorker.perform_async(id, request_id)`.
pub async fn synchronize_featured_collections_collection_later(
    state: &AppState,
    account_id: i64,
    request_id: &str,
) {
    crate::jobs::push(
        state,
        SynchronizeFeaturedCollectionsCollectionWorker {
            account_id,
            request_id: Some(request_id.to_owned()),
        },
    )
    .await;
}

/// `ActivityPub::SynchronizeFeaturedCollectionsCollectionWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SynchronizeFeaturedCollectionsCollectionWorker {
    pub account_id: i64,
    #[serde(default)]
    pub request_id: Option<String>,
}

impl crate::jobs::Job for SynchronizeFeaturedCollectionsCollectionWorker {
    const KIND: &'static str = "ActivityPub::SynchronizeFeaturedCollectionsCollectionWorker";
    const OPTIONS: crate::jobs::Options = OPTIONS;

    async fn perform(self, state: &AppState) -> Result<()> {
        fetch_featured_collections_collection(state, self.account_id).await
    }
}

async fn remote_account(state: &AppState, account_id: i64) -> Result<Option<Account>> {
    Ok(sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = $1 AND domain IS NOT NULL AND suspended_at IS NULL",
        account_id
    )
    .fetch_optional(&state.db)
    .await?)
}

/// `ActivityPub::FetchFeaturedCollectionService`: the account's pinned posts
/// are the `Note`s of its `featured` collection's first page, and, when
/// asked, its featured hashtags the `Hashtag`s there. The collection is
/// fetched as it is named; its pages, and the posts, only from the account's
/// host, on behalf of a local follower. A page that fails for the time being
/// fails the job, to be retried.
pub async fn fetch_featured_collection(
    state: &AppState,
    account_id: i64,
    collection: Option<&str>,
    hashtag: bool,
    request_id: Option<&str>,
) -> Result<()> {
    let Some(account) = remote_account(state, account_id).await? else {
        return Ok(());
    };
    let url = collection
        .filter(|url| !url.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| account.featured_collection_url.clone())
        .filter(|url| !url.trim().is_empty());
    let Some(url) = url else {
        return Ok(());
    };
    // `fetch_collection_page(url)`: no reference, no on-behalf-of.
    let Some(json) =
        json_ld::fetch_resource_without_id_validation(state, &url, None, RaiseOn::Temporary)
            .await?
            .filter(json_ld::is_present)
    else {
        return Ok(());
    };
    let Some(uri) = account.stored_uri() else {
        return Ok(());
    };
    let local_follower = json_ld::local_follower(state, account.id).await;
    let Some((items, _)) =
        json_ld::collection_items(state, &json, Some(1), None, uri, local_follower).await?
    else {
        return Ok(());
    };

    // `process_note_items`: each fetched, as `FetchRemoteStatusService`
    // fetches it, on behalf of a local follower and only if the account is
    // its author.
    let mut status_ids = Vec::new();
    for item in &items {
        let item_uri = match item {
            Value::String(uri) => uri.as_str(),
            Value::Object(object) if object.get("type").and_then(Value::as_str) == Some("Note") => {
                object.get("id").and_then(Value::as_str).unwrap_or_default()
            }
            _ => continue,
        };
        if crate::federation::local_uri::is_local(state, item_uri)
            || non_matching_uri_hosts(uri, item_uri)
        {
            continue;
        }
        let fetched = crate::api::ap::inbox::fetch_remote_status_with(
            state,
            item_uri,
            crate::api::ap::inbox::FetchOptions {
                on_behalf_of: local_follower,
                expected_actor_uri: Some(uri.to_owned()),
                request_id: request_id.map(str::to_owned),
                ..Default::default()
            },
        )
        // A request the post's server does not answer fails the job, to be
        // retried, as it raises out of Mastodon's.
        .await?;
        let Some((status_id, _)) = fetched else {
            continue;
        };
        // `next unless status&.account_id == @account.id`.
        let authored = sqlx::query_scalar!(
            r#"SELECT (account_id = $2) AS "authored!" FROM statuses WHERE id = $1"#,
            status_id,
            account.id,
        )
        .fetch_optional(&state.db)
        .await?
        .unwrap_or(false);
        if authored && !status_ids.contains(&status_id) {
            status_ids.push(status_id);
        }
    }
    sqlx::query!(
        "DELETE FROM status_pins WHERE account_id = $1 AND status_id <> ALL($2)",
        account.id,
        &status_ids,
    )
    .execute(&state.db)
    .await?;
    // `StatusPin.create!` for each post not yet pinned, in order. A post
    // `StatusPinValidator` refuses — a boost, or a direct post — raises,
    // failing the job to be retried as Mastodon's is, with the pins before
    // it kept.
    for status_id in status_ids {
        let status = sqlx::query!(
            r#"SELECT reblog_of_id, visibility,
                      EXISTS (SELECT 1 FROM status_pins WHERE account_id = $2 AND status_id = $1)
                        AS "pinned!"
               FROM statuses WHERE id = $1"#,
            status_id,
            account.id,
        )
        .fetch_one(&state.db)
        .await?;
        if status.pinned {
            continue;
        }
        if status.reblog_of_id.is_some() {
            anyhow::bail!("Validation failed: Boosts cannot be pinned (status {status_id})");
        }
        if status.visibility == crate::db::models::vis::DIRECT {
            anyhow::bail!("Validation failed: Direct posts cannot be pinned (status {status_id})");
        }
        sqlx::query!(
            r#"INSERT INTO status_pins (account_id, status_id, created_at, updated_at)
               VALUES ($1, $2, now(), now())"#,
            account.id,
            status_id,
        )
        .execute(&state.db)
        .await?;
    }

    // `process_hashtag_items`.
    if hashtag {
        let names: Vec<String> = hashtag_names(&items)
            .into_iter()
            .map(|name| normalize_hashtag(&name))
            .collect();
        sync_featured_tags(state, &account, &names, false).await?;
    }
    Ok(())
}

/// `ActivityPub::FetchFeaturedTagsCollectionService`.
pub async fn fetch_featured_tags_collection(
    state: &AppState,
    account_id: i64,
    url: Option<&str>,
) -> Result<()> {
    let Some(url) = url.filter(|url| !url.trim().is_empty()) else {
        return Ok(());
    };
    let Some(account) = remote_account(state, account_id).await? else {
        return Ok(());
    };
    // `fetch_resource(url, true, local_follower)`.
    let local_follower = json_ld::local_follower(state, account.id).await;
    let Some(json) = json_ld::fetch_resource(state, url, local_follower, RaiseOn::None).await?
    else {
        return Ok(());
    };
    if !supported_context(&json) {
        return Ok(());
    }
    let Some(uri) = account.stored_uri() else {
        return Ok(());
    };
    let Some((items, _)) = json_ld::collection_items(
        state,
        &json,
        Some(FEATURED_TAG_LIMIT),
        Some(FEATURED_TAG_LIMIT),
        uri,
        local_follower,
    )
    .await?
    else {
        return Ok(());
    };
    let names: Vec<String> = hashtag_names(&items)
        .into_iter()
        .take(FEATURED_TAG_LIMIT)
        .collect();
    sync_featured_tags(state, &account, &names, true).await
}

/// The featured hashtags become `names`, compared by their normalized form.
/// With `rename`, a hashtag still featured takes the spelling given
/// (`FetchFeaturedTagsCollectionService`); without, the spelling stays.
async fn sync_featured_tags(
    state: &AppState,
    account: &Account,
    names: &[String],
    rename: bool,
) -> Result<()> {
    let mut wanted: Vec<(String, String)> = Vec::new();
    for name in names {
        let normalized = normalize_hashtag(name);
        if normalized.is_empty() || wanted.iter().any(|(n, _)| *n == normalized) {
            continue;
        }
        wanted.push((normalized, name.clone()));
    }
    let normalized: Vec<String> = wanted.iter().map(|(n, _)| n.clone()).collect();
    sqlx::query!(
        r#"DELETE FROM featured_tags ft USING tags t
           WHERE ft.tag_id = t.id AND ft.account_id = $1 AND lower(t.name) <> ALL($2)"#,
        account.id,
        &normalized,
    )
    .execute(&state.db)
    .await?;
    for (normalized, name) in wanted {
        let existing = sqlx::query_scalar!(
            r#"SELECT ft.id FROM featured_tags ft JOIN tags t ON t.id = ft.tag_id
               WHERE ft.account_id = $1 AND lower(t.name) = $2"#,
            account.id,
            normalized,
        )
        .fetch_optional(&state.db)
        .await?;
        if let Some(id) = existing {
            if rename {
                sqlx::query!(
                    "UPDATE featured_tags SET name = $2, updated_at = now() WHERE id = $1",
                    id,
                    name
                )
                .execute(&state.db)
                .await?;
            }
            continue;
        }
        let display = name.trim().trim_start_matches('#');
        let Some(tag_id) = crate::tags::find_or_create(&state.db, display).await? else {
            continue;
        };
        // `reset_data`: what the account has posted with it that others may
        // see.
        sqlx::query!(
            r#"INSERT INTO featured_tags
                 (account_id, tag_id, name, statuses_count, last_status_at, created_at, updated_at)
               SELECT $1, $2, $3, count(s.id), max(s.created_at), now(), now()
               FROM statuses s JOIN statuses_tags st ON st.status_id = s.id AND st.tag_id = $2
               WHERE s.account_id = $1 AND s.deleted_at IS NULL AND s.visibility IN (0, 1)
               ON CONFLICT (account_id, tag_id) DO NOTHING"#,
            account.id,
            tag_id,
            display,
        )
        .execute(&state.db)
        .await?;
    }
    Ok(())
}

/// `ActivityPub::FetchFeaturedCollectionsCollectionService`: the collections
/// the account features, each fetched or taken as embedded.
pub async fn fetch_featured_collections_collection(
    state: &AppState,
    account_id: i64,
) -> Result<()> {
    let Some(account) = remote_account(state, account_id).await? else {
        return Ok(());
    };
    let Some(collections_url) = account
        .collections_url
        .clone()
        .filter(|url| !url.trim().is_empty())
    else {
        return Ok(());
    };
    let Some(uri) = account.stored_uri() else {
        return Ok(());
    };
    let Some((items, _)) = json_ld::collection_items(
        state,
        &Value::String(collections_url),
        Some(COLLECTIONS_MAX_PAGES),
        None,
        uri,
        None,
    )
    .await?
    else {
        return Ok(());
    };
    for item in items.into_iter().take(COLLECTIONS_MAX_ITEMS) {
        let stored = match &item {
            // `FetchRemoteFeaturedCollectionService`, by its URI.
            Value::String(collection_uri) => {
                crate::federation::featured_collections::fetch_remote_featured_collection(
                    state,
                    collection_uri,
                    None,
                )
                .await
            }
            // `ProcessFeaturedCollectionService`, embedded.
            json @ Value::Object(_) => {
                crate::federation::featured_collections::process_featured_collection(
                    state, account.id, uri, json,
                )
                .await
            }
            _ => continue,
        };
        if let Err(error) = stored {
            tracing::debug!(?error, "could not store a featured collection");
        }
    }
    Ok(())
}

/// The names of the `Hashtag`s among `items`, without their `#`.
fn hashtag_names(items: &[Value]) -> Vec<String> {
    items
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("Hashtag"))
        .filter_map(|item| item.get("name").and_then(Value::as_str))
        .map(|name| name.strip_prefix('#').unwrap_or(name).to_owned())
        .collect()
}

/// `HashtagNormalizer#normalize`, without its Unicode compatibility and ASCII
/// folding: lower case, and only what a hashtag may hold.
fn normalize_hashtag(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '·' || *c == '\u{200c}')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashtags_are_compared_normalized() {
        assert_eq!(normalize_hashtag("Rust_Lang"), "rust_lang");
        assert_eq!(normalize_hashtag("한국어"), "한국어");
        assert_eq!(normalize_hashtag("a-b"), "ab");
    }
}
