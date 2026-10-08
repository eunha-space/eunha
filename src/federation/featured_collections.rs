//! Remote collections, as Mastodon keeps them:
//! `ActivityPub::ProcessFeaturedCollectionService`,
//! `ActivityPub::ProcessFeaturedItemService`,
//! `ActivityPub::VerifyFeaturedItemService` and
//! `ActivityPub::FetchRemoteFeaturedCollectionService`, with their workers
//! (`ProcessFeaturedItemWorker`, `VerifyFeaturedItemWorker`); and
//! `AccountPolicy#feature?`, which says who may feature whom.
//!
//! A remote collection belongs to the account that sent it, and only to that
//! account: it is stored only from its own host, attributed to that account,
//! and looked up among that account's collections, so no actor can write over
//! another's. Its items are taken on as pending, naming only the account they
//! feature (`object_uri`), and accepted once the `FeatureAuthorization` the
//! featured account gave is fetched from that account's host and holds.

use std::time::Duration;

use serde_json::Value;

use crate::federation::json_ld::{self, non_matching_uri_hosts, value_or_id, RaiseOn};
use crate::state::AppState;

/// `ProcessFeaturedCollectionService::ITEMS_LIMIT`.
pub const ITEMS_LIMIT: usize = 150;
/// `Collection::NAME_LENGTH_HARD_LIMIT`.
const NAME_LENGTH_HARD_LIMIT: usize = 256;
/// `Collection::DESCRIPTION_LENGTH_HARD_LIMIT`.
const DESCRIPTION_LENGTH_HARD_LIMIT: usize = 2048;
/// `ProcessFeaturedItemService::PROCESSING_DELAY`, `30.seconds..10.minutes`.
const PROCESSING_DELAY_SECS: std::ops::RangeInclusive<u64> = 30..=600;

/// `CollectionItem` states.
const PENDING: i32 = 0;
const ACCEPTED: i32 = 1;
const REJECTED: i32 = 2;

// ── AccountPolicy#feature? ────────────────────────────────────────────────

/// `AccountPolicy.new(owner, target).feature?`: whether `owner` may feature
/// `target` in a collection. `Account#featureable_by?` — a local account
/// when it is discoverable and either unlocked, followed by `owner` or
/// `owner` itself; a remote one when its `feature_approval_policy` answers
/// `automatic` or `manual` for `owner` — and neither blocks the other.
pub async fn may_feature(db: &sqlx::PgPool, owner_id: i64, target_id: i64) -> sqlx::Result<bool> {
    let Some(target) = sqlx::query!(
        r#"SELECT (a.domain IS NULL) AS "local!", COALESCE(a.discoverable, false) AS "discoverable!",
                  a.locked, a.feature_approval_policy,
                  EXISTS (SELECT 1 FROM follows
                          WHERE account_id = $1 AND target_account_id = $2) AS "followed_by_owner!",
                  EXISTS (SELECT 1 FROM follows
                          WHERE account_id = $2 AND target_account_id = $1) AS "follows_owner!",
                  EXISTS (SELECT 1 FROM blocks
                          WHERE (account_id = $1 AND target_account_id = $2)
                             OR (account_id = $2 AND target_account_id = $1)) AS "blocked!"
           FROM accounts a WHERE a.id = $2"#,
        owner_id,
        target_id,
    )
    .fetch_optional(db)
    .await?
    else {
        return Ok(false);
    };
    if target.blocked {
        return Ok(false);
    }
    let featureable = if target.local {
        target.discoverable && (!target.locked || target.followed_by_owner || owner_id == target_id)
    } else {
        matches!(
            crate::db::models::feature_policy::for_account(
                target.feature_approval_policy,
                owner_id == target_id,
                target.followed_by_owner,
                target.follows_owner,
            ),
            "automatic" | "manual"
        )
    };
    Ok(featureable)
}

// ── Locks ─────────────────────────────────────────────────────────────────

/// `with_redis_lock(name)`: the lock `lock:<name>`, waited for a moment, or
/// an error for the work to be tried again later
/// (`Mastodon::RaceConditionError`).
async fn redis_lock(state: &AppState, name: &str) -> anyhow::Result<crate::redis_lock::RedisLock> {
    let key = format!("lock:{name}");
    for attempt in 0..40 {
        if let Some(lock) =
            crate::redis_lock::try_acquire(state, &key, crate::redis_lock::DEFAULT_TTL_MS).await
        {
            return Ok(lock);
        }
        if attempt < 39 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    anyhow::bail!("could not acquire lock for {name}, try again later")
}

// ── ProcessFeaturedCollectionService ──────────────────────────────────────

/// A remote collection's columns, as `collection_attributes` reads them from
/// its `FeaturedCollection`.
struct Attributes {
    name: String,
    description_html: String,
    language: Option<String>,
    sensitive: bool,
    discoverable: bool,
    original_number_of_items: i32,
    tag_id: Option<i64>,
    url: String,
}

fn truncate(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

impl Attributes {
    /// The attributes, or `None` when `Collection`'s validations would
    /// refuse them: a blank name, `sensitive` or `discoverable` neither true
    /// nor false, or a topic whose hashtag may not be used.
    async fn read(state: &AppState, json: &Value, id: &str) -> anyhow::Result<Option<Self>> {
        let name = truncate(
            json.get("name").and_then(Value::as_str).unwrap_or(""),
            NAME_LENGTH_HARD_LIMIT,
        );
        if name.trim().is_empty() {
            return Ok(None);
        }
        let summary_map = json
            .get("summaryMap")
            .and_then(Value::as_object)
            .filter(|map| !map.is_empty());
        // `@json['summaryMap']&.values&.first || @json['summary'] || ''`.
        let description = summary_map
            .and_then(|map| map.values().next())
            .and_then(Value::as_str)
            .or_else(|| json.get("summary").and_then(Value::as_str))
            .unwrap_or("");
        let language = summary_map.and_then(|map| map.keys().next().cloned());
        let (Some(sensitive), Some(discoverable)) = (
            json.get("sensitive").and_then(Value::as_bool),
            json.get("discoverable").and_then(Value::as_bool),
        ) else {
            return Ok(None);
        };
        let original_number_of_items = json
            .get("totalItems")
            .and_then(Value::as_i64)
            .map_or(0, |n| i32::try_from(n).unwrap_or(i32::MAX));
        // `tag_name: @json.dig('topic', 'name')`, and `tag_is_usable`.
        let tag_id = match json
            .get("topic")
            .and_then(|topic| topic.get("name"))
            .and_then(Value::as_str)
        {
            Some(name) => match crate::tags::find_or_create(&state.db, name).await? {
                Some(tag_id) => {
                    let usable =
                        sqlx::query_scalar!("SELECT usable FROM tags WHERE id = $1", tag_id)
                            .fetch_one(&state.db)
                            .await?;
                    if usable == Some(false) {
                        return Ok(None);
                    }
                    Some(tag_id)
                }
                None => None,
            },
            None => None,
        };
        // `url`: the page it links to, else its id.
        let url = json
            .get("url")
            .and_then(|url| ojak_vocab::json_ld_helper::url_to_href(url, Some("text/html")))
            .filter(|url| !url.trim().is_empty())
            .filter(|url| !ojak_vocab::json_ld_helper::unsupported_uri_scheme(Some(url)))
            .unwrap_or(id)
            .to_owned();
        Ok(Some(Self {
            name,
            description_html: truncate(description, DESCRIPTION_LENGTH_HARD_LIMIT),
            language,
            sensitive,
            discoverable,
            original_number_of_items,
            tag_id,
            url,
        }))
    }
}

/// `ActivityPub::ProcessFeaturedCollectionService#call(account, json)`: the
/// `FeaturedCollection` `json` stored as one of the account's (`account_id`,
/// whose URI is `account_uri`) — only from the account's own host and
/// attributed to it, and only among its own collections, so that a
/// collection another account has is never touched. The items no longer
/// listed are pruned, and each of the first [`ITEMS_LIMIT`] listed is
/// processed by a [`ProcessFeaturedItemWorker`] at its place. Its id, or
/// `None` when it was not stored.
pub async fn process_featured_collection(
    state: &AppState,
    account_id: i64,
    account_uri: &str,
    json: &Value,
) -> anyhow::Result<Option<i64>> {
    let Some(uri) = json.get("id").and_then(Value::as_str) else {
        return Ok(None);
    };
    if account_uri.is_empty() || non_matching_uri_hosts(account_uri, uri) {
        return Ok(None);
    }
    if json.get("attributedTo").and_then(Value::as_str) != Some(account_uri) {
        return Ok(None);
    }

    let _lock = redis_lock(state, &format!("collection:{uri}")).await?;
    let Some(attributes) = Attributes::read(state, json, uri).await? else {
        tracing::debug!(collection = uri, "remote collection is not valid");
        return Ok(None);
    };
    // `(@json['orderedItems'] || [])[0, ITEMS_LIMIT]`.
    let items: Vec<Value> = json
        .get("orderedItems")
        .and_then(Value::as_array)
        .map(|items| items.iter().take(ITEMS_LIMIT).cloned().collect())
        .unwrap_or_default();
    let item_uris: Vec<String> = items
        .iter()
        .filter_map(value_or_id)
        .map(str::to_owned)
        .collect();

    let mut tx = state.db.begin().await?;
    // `@account.collections.find_or_initialize_by(uri: @json['id'])`.
    let previous = sqlx::query!(
        r#"SELECT id, local, name, description_html, language, sensitive, discoverable,
                  original_number_of_items, tag_id, url
           FROM collections WHERE account_id = $1 AND uri = $2 FOR UPDATE"#,
        account_id,
        uri,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Attributes {
        name,
        description_html,
        language,
        sensitive,
        discoverable,
        original_number_of_items,
        tag_id,
        url,
    } = attributes;
    let (collection_id, significantly_changed) = match &previous {
        Some(previous) => {
            let changed = previous.local
                || previous.name != name
                || previous.description_html.as_deref() != Some(description_html.as_str())
                || previous.language != language
                || previous.sensitive != sensitive
                || previous.discoverable != discoverable
                || previous.original_number_of_items != Some(original_number_of_items)
                || previous.tag_id != tag_id
                || previous.url.as_deref() != Some(url.as_str());
            // `update!` saves, and touches the row, only when something
            // changed.
            if changed {
                sqlx::query!(
                    r#"UPDATE collections
                       SET local = false, name = $2, description_html = $3, language = $4,
                           sensitive = $5, discoverable = $6, original_number_of_items = $7,
                           tag_id = $8, url = $9, updated_at = now()
                       WHERE id = $1"#,
                    previous.id,
                    name,
                    description_html,
                    language,
                    sensitive,
                    discoverable,
                    original_number_of_items,
                    tag_id,
                    url,
                )
                .execute(&mut *tx)
                .await?;
            }
            // `NotifyOfCollectionUpdateService#significantly_changed?`.
            let significant = previous.name != name
                || previous.description_html.as_deref() != Some(description_html.as_str())
                || previous.sensitive != sensitive
                || previous.tag_id != tag_id;
            (previous.id, significant)
        }
        None => {
            // A URI another account's collection holds is not taken over:
            // the unique index refuses it, as it refuses Mastodon's insert.
            let inserted = sqlx::query_scalar!(
                r#"INSERT INTO collections
                     (account_id, name, description_html, language, discoverable, local,
                      sensitive, item_count, original_number_of_items, tag_id, uri, url,
                      created_at, updated_at)
                   VALUES ($1, $2, $3, $4, $5, false, $6, 0, $7, $8, $9, $10, now(), now())
                   ON CONFLICT (uri) WHERE uri IS NOT NULL DO NOTHING
                   RETURNING id"#,
                account_id,
                name,
                description_html,
                language,
                discoverable,
                sensitive,
                original_number_of_items,
                tag_id,
                uri,
                url,
            )
            .fetch_optional(&mut *tx)
            .await?;
            let Some(id) = inserted else {
                tracing::warn!(
                    collection = uri,
                    account_uri,
                    "refused a collection another account already has"
                );
                return Ok(None);
            };
            (id, false)
        }
    };
    // `@collection.collection_items.where.not(uri: item_uris).delete_all`:
    // an item with no URI stays unless nothing is listed, and the counter
    // cache is left alone, as `delete_all` leaves it.
    if item_uris.is_empty() {
        sqlx::query!(
            "DELETE FROM collection_items WHERE collection_id = $1",
            collection_id
        )
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query!(
            "DELETE FROM collection_items WHERE collection_id = $1 AND uri <> ALL($2)",
            collection_id,
            &item_uris,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    // `process_items!`.
    for (index, item) in items.into_iter().enumerate() {
        crate::jobs::push(
            state,
            ProcessFeaturedItemWorker {
                collection_id,
                id_or_json: item,
                position: i32::try_from(index + 1).ok(),
            },
        )
        .await;
    }
    // `notify_about_update!`.
    if significantly_changed {
        Box::pin(
            crate::api::mastodon::collections::notify_of_collection_update(state, collection_id),
        )
        .await;
    }
    Ok(Some(collection_id))
}

// ── FetchRemoteFeaturedCollectionService ──────────────────────────────────

/// `ActivityPub::FetchRemoteFeaturedCollectionService#call(uri)`: the
/// collection at `uri`, fetched (`on_behalf_of` a local account, or as the
/// instance), when it is a `FeaturedCollection` of an account already known
/// here; the one stored already if that account has it, or else processed.
/// An error is a fetch that did not get through.
pub async fn fetch_remote_featured_collection(
    state: &AppState,
    uri: &str,
    on_behalf_of: Option<i64>,
) -> anyhow::Result<Option<i64>> {
    let Some(json) = json_ld::fetch_resource(state, uri, on_behalf_of, RaiseOn::None).await? else {
        return Ok(None);
    };
    if !json_ld::supported_context(&json)
        || json.get("type").and_then(Value::as_str) != Some("FeaturedCollection")
    {
        return Ok(None);
    }
    // Only a known account's (`Account.find_by(uri: json['attributedTo'])`).
    let Some(attributed_to) = json.get("attributedTo").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(account_id) = sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE uri = $1 AND domain IS NOT NULL ORDER BY id LIMIT 1",
        attributed_to,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(None);
    };
    // `account.collections.find_by(uri:)`.
    if let Some(existing) = sqlx::query_scalar!(
        "SELECT id FROM collections WHERE account_id = $1 AND uri = $2",
        account_id,
        uri,
    )
    .fetch_optional(&state.db)
    .await?
    {
        return Ok(Some(existing));
    }
    process_featured_collection(state, account_id, attributed_to, &json).await
}

// ── ProcessFeaturedItemService ────────────────────────────────────────────

/// `ActivityPub::ProcessFeaturedItemService#call(collection, uri_or_object,
/// position:)`: one `FeaturedItem` of the remote collection
/// `collection_id`, fetched when given by its URI. It has to be on the
/// collection's host, and its `featureAuthorization` on the featured
/// account's (unless that account is local); the featured object has to be
/// an actor. The item already stored under its URI is updated; else, for a
/// local account, the item it already accepted (from its `FeatureRequest`);
/// else a new item is taken on as pending, featuring nobody until its
/// authorization is verified, while the collection holds fewer than
/// [`ITEMS_LIMIT`]. An item not featuring a local account is then verified,
/// or, when its authorization cannot be fetched for now, verified later by a
/// [`VerifyFeaturedItemWorker`].
pub async fn process_featured_item(
    state: &AppState,
    collection_id: i64,
    uri_or_object: &Value,
    position: Option<i32>,
) -> anyhow::Result<Option<i64>> {
    let Some(collection_uri) = sqlx::query_scalar!(
        r#"SELECT uri AS "uri?" FROM collections WHERE id = $1"#,
        collection_id
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(None);
    };
    let collection_uri = collection_uri.unwrap_or_default();
    let fetched;
    let item = match uri_or_object {
        Value::String(uri) => {
            fetched = json_ld::fetch_resource(state, uri, None, RaiseOn::None)
                .await?
                .ok_or_else(|| anyhow::anyhow!("the featured item {uri} could not be fetched"))?;
            &fetched
        }
        object => object,
    };
    let Some(item_uri) = item.get("id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let actor_uri = item.get("featuredObject").and_then(value_or_id);
    let approval_uri = item.get("featureAuthorization").and_then(value_or_id);
    if collection_uri.is_empty() || non_matching_uri_hosts(&collection_uri, item_uri) {
        return Ok(None);
    }
    let local_actor_uri =
        actor_uri.is_some_and(|actor| crate::federation::local_uri::is_local(state, actor));
    // `non_matching_actor_and_approval_uris?`.
    if !local_actor_uri {
        match (actor_uri, approval_uri) {
            (Some(actor), Some(approval)) if !non_matching_uri_hosts(actor, approval) => {}
            _ => return Ok(None),
        }
    }
    // `non_supported_object_type?`.
    if !local_actor_uri {
        let actor = actor_uri.unwrap_or_default();
        let known = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM accounts WHERE uri = $1) AS "known!""#,
            actor,
        )
        .fetch_one(&state.db)
        .await?;
        if !known {
            let object = json_ld::fetch_resource(state, actor, None, RaiseOn::Temporary).await?;
            if !object.is_some_and(|object| {
                crate::federation::fetch_resource::type_matches(
                    &object,
                    &crate::federation::fetch_resource::ACTOR_TYPES,
                )
            }) {
                return Ok(None);
            }
        }
    }

    // `existing_item || pre_approved_item || new_item`.
    let existing = sqlx::query_scalar!(
        "SELECT id FROM collection_items WHERE collection_id = $1 AND uri = $2",
        collection_id,
        item_uri,
    )
    .fetch_optional(&state.db)
    .await?;
    let pre_approved = match (existing, actor_uri) {
        (None, Some(actor)) if local_actor_uri => {
            match crate::federation::local_uri::account(state, actor).await {
                Some(local) => {
                    sqlx::query_scalar!(
                        r#"SELECT ci.id FROM collection_items ci
                           JOIN accounts a ON a.id = ci.account_id AND a.domain IS NULL
                           WHERE ci.collection_id = $1 AND ci.account_id = $2
                             AND ci.state = $3 AND ci.uri IS NULL
                           ORDER BY ci.id LIMIT 1"#,
                        collection_id,
                        local,
                        ACCEPTED,
                    )
                    .fetch_optional(&state.db)
                    .await?
                }
                None => None,
            }
        }
        _ => None,
    };

    let _lock = redis_lock(state, &format!("collection_item:{item_uri}")).await?;
    let item_id = match existing.or(pre_approved) {
        Some(item_id) => {
            sqlx::query!(
                r#"UPDATE collection_items
                   SET position = COALESCE($2, position), uri = $3, object_uri = $4,
                       updated_at = now()
                   WHERE id = $1"#,
                item_id,
                position,
                item_uri,
                actor_uri,
            )
            .execute(&state.db)
            .await?;
            item_id
        }
        None => {
            let count = sqlx::query_scalar!(
                r#"SELECT count(*) AS "count!" FROM collection_items WHERE collection_id = $1"#,
                collection_id,
            )
            .fetch_one(&state.db)
            .await?;
            if count >= ITEMS_LIMIT as i64 {
                return Ok(None);
            }
            // An item featuring nobody yet has to say whom it would.
            let Some(actor) = actor_uri else {
                return Ok(None);
            };
            let published = item
                .get("published")
                .and_then(Value::as_str)
                .and_then(|published| chrono::DateTime::parse_from_rfc3339(published).ok())
                .map(|published| published.naive_utc());
            let mut tx = state.db.begin().await?;
            let inserted = sqlx::query_scalar!(
                r#"INSERT INTO collection_items
                     (collection_id, state, uri, object_uri, position, created_at, updated_at)
                   VALUES ($1, $2, $3, $4,
                           COALESCE($5, (SELECT COALESCE(MAX(position), 0) + 1
                                         FROM collection_items WHERE collection_id = $1)),
                           COALESCE($6, now() AT TIME ZONE 'UTC'), now())
                   ON CONFLICT (uri) WHERE uri IS NOT NULL DO NOTHING
                   RETURNING id"#,
                collection_id,
                PENDING,
                item_uri,
                actor,
                position,
                published,
            )
            .fetch_optional(&mut *tx)
            .await?;
            let Some(item_id) = inserted else {
                tracing::debug!(
                    item = item_uri,
                    "featured item belongs to another collection"
                );
                return Ok(None);
            };
            crate::api::mastodon::collections::update_item_count(&mut *tx, collection_id, 1)
                .await?;
            tx.commit().await?;
            item_id
        }
    };

    // `verify_authorization! unless @collection_item&.account&.local?`.
    let features_local = sqlx::query_scalar!(
        r#"SELECT (a.domain IS NULL) AS "local!" FROM collection_items ci
           JOIN accounts a ON a.id = ci.account_id WHERE ci.id = $1"#,
        item_id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    if !features_local {
        if let Some(approval_uri) = approval_uri {
            if let Err(error) = verify_featured_item(state, item_id, approval_uri).await {
                tracing::debug!(%error, approval_uri, "feature authorization verified later");
                let delay = Duration::from_secs(rand::random_range(PROCESSING_DELAY_SECS));
                crate::jobs::push_in(
                    state,
                    delay,
                    VerifyFeaturedItemWorker {
                        collection_item_id: item_id,
                        approval_uri: approval_uri.to_owned(),
                    },
                )
                .await;
            }
        }
    }
    Ok(Some(item_id))
}

// ── VerifyFeaturedItemService ─────────────────────────────────────────────

/// `ActivityPub::VerifyFeaturedItemService#call(collection_item,
/// approval_uri)`: the item accepted, featuring the account it names, when
/// the `FeatureAuthorization` at `approval_uri` is on that account's host,
/// names the item's collection and account, and is one; rejected when the
/// authorization is not there. An error is a fetch to be tried again.
pub async fn verify_featured_item(
    state: &AppState,
    item_id: i64,
    approval_uri: &str,
) -> anyhow::Result<()> {
    let Some(item) = sqlx::query!(
        r#"SELECT ci.object_uri, c.uri AS "collection_uri?"
           FROM collection_items ci JOIN collections c ON c.id = ci.collection_id
           WHERE ci.id = $1"#,
        item_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let Some(authorization) =
        json_ld::fetch_resource(state, approval_uri, None, RaiseOn::Temporary).await?
    else {
        sqlx::query!(
            "UPDATE collection_items SET state = $2, updated_at = now() WHERE id = $1",
            item_id,
            REJECTED,
        )
        .execute(&state.db)
        .await?;
        return Ok(());
    };
    let collection_uri = authorization.get("interactingObject").and_then(value_or_id);
    let Some(actor_uri) = authorization.get("interactionTarget").and_then(value_or_id) else {
        return Ok(());
    };
    if non_matching_uri_hosts(approval_uri, actor_uri) {
        return Ok(());
    }
    let matching_type = json_ld::supported_context(&authorization)
        && ojak_vocab::json_ld_helper::equals_or_includes(
            authorization.get("type"),
            "FeatureAuthorization",
        );
    if !matching_type
        || item.collection_uri.as_deref() != collection_uri
        || item.object_uri.as_deref() != Some(actor_uri)
    {
        return Ok(());
    }
    // `Account.where(uri: object_uri).first`, else fetched.
    let known = sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE uri = $1 ORDER BY id LIMIT 1",
        actor_uri,
    )
    .fetch_optional(&state.db)
    .await?;
    let account_id = match known {
        Some(id) => id,
        None => {
            match crate::api::ap::inbox::resolve_or_fetch_remote_account(state, actor_uri).await {
                Ok(id) => id,
                Err(_) => return Ok(()),
            }
        }
    };
    let accepted = sqlx::query!(
        r#"UPDATE collection_items
           SET account_id = $2, approval_uri = $3, state = $4, updated_at = now()
           WHERE id = $1"#,
        item_id,
        account_id,
        approval_uri,
        ACCEPTED,
    )
    .execute(&state.db)
    .await;
    match accepted {
        Ok(_) => Ok(()),
        // Another item of the collection features the account, or holds the
        // authorization: `update!` fails its uniqueness.
        Err(sqlx::Error::Database(error)) if error.is_unique_violation() => {
            tracing::debug!(item_id, approval_uri, "featured item duplicates another");
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

// ── Workers ───────────────────────────────────────────────────────────────

/// `ActivityPub::ProcessFeaturedItemWorker`: one item of a remote
/// collection, on the `pull` queue, retried three times.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct ProcessFeaturedItemWorker {
    pub collection_id: i64,
    pub id_or_json: Value,
    #[serde(default)]
    pub position: Option<i32>,
}

impl crate::jobs::Job for ProcessFeaturedItemWorker {
    const KIND: &'static str = "ActivityPub::ProcessFeaturedItemWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Pull)
        .retry(3);

    fn retry_in(count: u32) -> Option<Duration> {
        crate::jobs::exponential_backoff(count)
    }

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        Box::pin(process_featured_item(
            state,
            self.collection_id,
            &self.id_or_json,
            self.position,
        ))
        .await?;
        Ok(())
    }
}

/// `ActivityPub::VerifyFeaturedItemWorker`: an item's authorization,
/// verified again while it cannot be fetched, on the `pull` queue, retried
/// five times.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct VerifyFeaturedItemWorker {
    pub collection_item_id: i64,
    pub approval_uri: String,
}

impl crate::jobs::Job for VerifyFeaturedItemWorker {
    const KIND: &'static str = "ActivityPub::VerifyFeaturedItemWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Pull)
        .retry(5);

    fn retry_in(count: u32) -> Option<Duration> {
        crate::jobs::exponential_backoff(count)
    }

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        // A permanent failure answers nothing and rejects the item; what is
        // left to raise is worth trying again.
        Box::pin(verify_featured_item(
            state,
            self.collection_item_id,
            &self.approval_uri,
        ))
        .await
    }
}
