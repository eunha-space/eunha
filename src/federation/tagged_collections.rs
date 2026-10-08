//! The collections a remote post links to (`tagged_objects`), as
//! `ActivityPub::Activity::Create#process_tagged_collection` and
//! `ActivityPub::ProcessStatusUpdateService#update_tagged_objects!` take
//! them from its `FeaturedCollection` tags, and
//! `TaggedCollectionResolveWorker` resolves those it could not reach.

use serde_json::Value;

use crate::state::AppState;

/// `PROCESSING_DELAY`: when an unreached collection is tried again, in
/// seconds.
const PROCESSING_DELAY: std::ops::RangeInclusive<u64> = 30..=600;

/// The `id`s of a note's `FeaturedCollection` tags.
#[must_use]
pub fn tagged_ids(note: &Value) -> Vec<String> {
    let tags = match note.get("tag") {
        Some(Value::Array(tags)) => tags.iter().collect::<Vec<_>>(),
        Some(tag @ Value::Object(_)) => vec![tag],
        _ => Vec::new(),
    };
    tags.into_iter()
        .filter(|tag| crate::federation::fetch_resource::type_matches(tag, &["FeaturedCollection"]))
        .filter_map(|tag| tag.get("id").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A collection eunha knows, with the URI it is known by
/// (`TagManager#uri_for`).
struct Known {
    id: i64,
    uri: String,
}

async fn known(state: &AppState, id: i64) -> sqlx::Result<Option<Known>> {
    Ok(sqlx::query!(
        "SELECT id, account_id, uri, local FROM collections WHERE id = $1",
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .map(|c| Known {
        id: c.id,
        uri: match c.uri.filter(|uri| !uri.is_empty() && !c.local) {
            Some(uri) => uri,
            None => crate::api::ap::collections::collection_uri(
                &state.instance.domain,
                c.account_id,
                c.id,
            ),
        },
    }))
}

/// `TagManager#uri_to_resource(uri, Collection)`, or else
/// `ActivityPub::FetchRemoteFeaturedCollectionService`: a collection on a
/// known account's server, fetched and stored. An error is a request that
/// did not get through (`HTTP_CONNECTION_ERRORS`), to be tried again later;
/// a server that answered with an error is not asked again.
async fn resolve(state: &AppState, uri: &str) -> anyhow::Result<Option<Known>> {
    if let Some(id) = crate::federation::local_uri::collection(state, uri).await {
        return Ok(known(state, id).await?);
    }
    if crate::federation::local_uri::is_local(state, uri) {
        return Ok(None);
    }
    // `FetchRemoteFeaturedCollectionService`: an answer that is not a known
    // account's collection is nothing to try again; only a request that did
    // not get through is an error, and tried again later.
    let Some(id) =
        crate::federation::featured_collections::fetch_remote_featured_collection(state, uri, None)
            .await?
    else {
        return Ok(None);
    };
    Ok(known(state, id).await?)
}

async fn tag(state: &AppState, status_id: i64, collection: &Known) -> sqlx::Result<()> {
    sqlx::query!(
        r#"INSERT INTO tagged_objects (status_id, object_type, object_id, ap_type, uri, created_at, updated_at)
           VALUES ($1, 'Collection', $2, 'FeaturedCollection', $3, now(), now())
           ON CONFLICT (status_id, object_type, object_id)
             WHERE object_type IS NOT NULL AND object_id IS NOT NULL
             DO UPDATE SET ap_type = 'FeaturedCollection', updated_at = now()"#,
        status_id,
        collection.id,
        collection.uri,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

async fn resolve_later(state: &AppState, status_id: i64, uri: String) {
    let delay = std::time::Duration::from_secs(rand::random_range(PROCESSING_DELAY));
    crate::jobs::push_in(
        state,
        delay,
        TaggedCollectionResolveWorker {
            status_id,
            uri,
            options: serde_json::json!({}),
        },
    )
    .await;
}

/// `ActivityPub::Activity::Create`: the collections a new post's tags name,
/// those it could not reach left to [`TaggedCollectionResolveWorker`].
pub async fn attach(state: &AppState, status_id: i64, note: &Value) {
    let mut unresolved = Vec::new();
    for uri in tagged_ids(note) {
        match resolve(state, &uri).await {
            Ok(Some(collection)) => {
                if let Err(error) = tag(state, status_id, &collection).await {
                    tracing::debug!(%error, uri, "tagged collection not stored");
                }
            }
            Ok(None) => {}
            Err(_) => unresolved.push(uri),
        }
    }
    unresolved.sort();
    unresolved.dedup();
    for uri in unresolved {
        resolve_later(state, status_id, uri).await;
    }
}

/// `ProcessStatusUpdateService#update_tagged_objects!`: an edited post's
/// collections become those its tags now name; one no longer named is
/// dropped, and one not reached is tried again later.
pub async fn update(state: &AppState, status_id: i64, note: &Value) {
    let mut unresolved = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for uri in tagged_ids(note) {
        match resolve(state, &uri).await {
            Ok(Some(collection)) => {
                if let Err(error) = tag(state, status_id, &collection).await {
                    tracing::debug!(%error, uri, "tagged collection not stored");
                }
                current.push(collection.uri);
            }
            Ok(None) => {}
            Err(_) => unresolved.push(uri),
        }
    }
    if let Err(error) = sqlx::query!(
        r#"DELETE FROM tagged_objects
           WHERE status_id = $1 AND (uri IS NULL OR NOT (uri = ANY($2)))"#,
        status_id,
        &current,
    )
    .execute(&state.db)
    .await
    {
        tracing::debug!(%error, "unused tagged collections not removed");
    }
    unresolved.sort();
    unresolved.dedup();
    for uri in unresolved {
        resolve_later(state, status_id, uri).await;
    }
}

/// `TaggedCollectionResolveWorker`: a collection a post links to that could
/// not be reached when the post arrived.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct TaggedCollectionResolveWorker {
    pub status_id: i64,
    pub uri: String,
    #[serde(default)]
    pub options: Value,
}

impl crate::jobs::Job for TaggedCollectionResolveWorker {
    const KIND: &'static str = "TaggedCollectionResolveWorker";
    /// `sidekiq_options queue: 'pull', retry: 7`.
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Pull)
        .retry(7);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        let exists = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM statuses WHERE id = $1) AS "e!""#,
            self.status_id,
        )
        .fetch_one(&state.db)
        .await?;
        if !exists {
            return Ok(());
        }
        let Some(collection) = resolve(state, &self.uri).await? else {
            return Ok(());
        };
        tag(state, self.status_id, &collection).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn only_featured_collection_tags_are_taken() {
        let note = json!({"tag": [
            {"type": "Hashtag", "name": "#x"},
            {"type": "FeaturedCollection", "id": "https://a.test/c/1"},
            {"type": ["FeaturedCollection"], "id": "https://a.test/c/2"},
            {"type": "FeaturedCollection"},
        ]});
        assert_eq!(
            super::tagged_ids(&note),
            ["https://a.test/c/1", "https://a.test/c/2"]
        );
    }
}
