//! A remote account's featured posts, hashtags and collections, synchronized
//! from the collections its actor names, as Mastodon's
//! `SynchronizeFeaturedCollectionWorker`, `SynchronizeFeaturedTagsCollectionWorker`
//! and `SynchronizeFeaturedCollectionsCollectionWorker` do after
//! `ProcessAccountService` stores the actor.

use anyhow::Result;
use serde_json::Value;

use crate::db::models::Account;
use crate::federation::replies::{collection_items, fetch_collection_page, non_matching_uri_hosts};
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
/// asked, its featured hashtags the `Hashtag`s there.
pub async fn fetch_featured_collection(
    state: &AppState,
    account_id: i64,
    collection: Option<&str>,
    hashtag: bool,
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
    let Some(json) = fetch_collection_page(state, &Value::String(url), None).await else {
        return Ok(());
    };
    let Some(uri) = account.stored_uri() else {
        return Ok(());
    };
    let Some((items, _)) = collection_items(state, &json, 1, usize::MAX, Some(uri)).await else {
        return Ok(());
    };

    // `process_note_items`.
    let mut status_ids = Vec::new();
    for item in &items {
        let item_uri = match item {
            Value::String(uri) => uri.as_str(),
            Value::Object(object) if object.get("type").and_then(Value::as_str) == Some("Note") => {
                object.get("id").and_then(Value::as_str).unwrap_or_default()
            }
            _ => continue,
        };
        if item_uri.is_empty()
            || crate::federation::moderation::domain_of(item_uri)
                .is_some_and(|host| host.eq_ignore_ascii_case(&state.instance.domain))
            || non_matching_uri_hosts(uri, item_uri)
        {
            continue;
        }
        let Ok(Some(status_id)) = crate::api::ap::inbox::fetch_remote_status(state, item_uri).await
        else {
            continue;
        };
        let pinnable = sqlx::query_scalar!(
            r#"SELECT (account_id = $2 AND reblog_of_id IS NULL AND visibility <> 3) AS "pinnable!"
               FROM statuses WHERE id = $1"#,
            status_id,
            account.id,
        )
        .fetch_optional(&state.db)
        .await?
        .unwrap_or(false);
        if pinnable && !status_ids.contains(&status_id) {
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
    for status_id in status_ids {
        sqlx::query!(
            r#"INSERT INTO status_pins (account_id, status_id, created_at, updated_at)
               VALUES ($1, $2, now(), now())
               ON CONFLICT DO NOTHING"#,
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
    let Ok(json) = crate::federation::fetch::signed_get_json(state, url).await else {
        return Ok(());
    };
    if json.get("id").and_then(Value::as_str) != Some(url) || !supported_context(&json) {
        return Ok(());
    }
    let Some((items, _)) = collection_items(
        state,
        &json,
        FEATURED_TAG_LIMIT,
        FEATURED_TAG_LIMIT,
        account.stored_uri(),
    )
    .await
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
    let Some((items, _)) = collection_items(
        state,
        &Value::String(collections_url),
        COLLECTIONS_MAX_PAGES,
        usize::MAX,
        Some(uri),
    )
    .await
    else {
        return Ok(());
    };
    for item in items.into_iter().take(COLLECTIONS_MAX_ITEMS) {
        let collection = match item {
            // `FetchRemoteFeaturedCollectionService`: only a collection not
            // yet known is processed.
            Value::String(collection_uri) => {
                let known = sqlx::query_scalar!(
                    r#"SELECT EXISTS (SELECT 1 FROM collections WHERE uri = $1 AND account_id = $2) AS "known!""#,
                    collection_uri,
                    account.id,
                )
                .fetch_one(&state.db)
                .await?;
                if known {
                    continue;
                }
                let Ok(json) =
                    crate::federation::fetch::signed_get_json(state, &collection_uri).await
                else {
                    continue;
                };
                if json.get("id").and_then(Value::as_str) != Some(collection_uri.as_str())
                    || !supported_context(&json)
                    || json.get("type").and_then(Value::as_str) != Some("FeaturedCollection")
                {
                    continue;
                }
                json
            }
            json @ Value::Object(_) => json,
            _ => continue,
        };
        // `ProcessFeaturedCollectionService`: the account's own collections,
        // on its own host.
        let id = collection
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if non_matching_uri_hosts(uri, id)
            || collection.get("attributedTo").and_then(Value::as_str) != Some(uri)
        {
            continue;
        }
        if let Err(error) =
            crate::api::ap::inbox::mirror_remote_collection(state, account.id, &collection).await
        {
            tracing::debug!(
                collection = id,
                ?error,
                "could not store a featured collection"
            );
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

/// `JsonLdHelper#supported_context?`.
fn supported_context(json: &Value) -> bool {
    const CONTEXT: &str = "https://www.w3.org/ns/activitystreams";
    match json.get("@context") {
        Some(Value::String(context)) => context == CONTEXT,
        Some(Value::Array(contexts)) => contexts.iter().any(|c| c.as_str() == Some(CONTEXT)),
        _ => false,
    }
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
