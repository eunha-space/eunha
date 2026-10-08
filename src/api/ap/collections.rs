//! ActivityPub representation of collections (Mastodon's FeaturedCollection).
//!
//! Serves a local account's collections as AP objects so remote servers can
//! discover and fetch them, as `CollectionsController`,
//! `ActivityPub::FeaturedCollectionsController`, `CollectionItemsController`
//! and `ActivityPub::FeatureAuthorizationsController` serve them, named as
//! `ActivityPub::TagManager` names them: a collection at
//! `/ap/users/{account_id}/collections/{id}`, its items at
//! `/ap/users/{account_id}/collection_items/{id}`, and an account's
//! collections at `/ap/users/{account_id}/featured_collections`. It also
//! builds the activities that distribute collection changes to followers.

use ojak::federation::CollectionDocument;
use serde_json::{json, Value};

use super::objects::AccountRef;
use crate::{
    error::{AppError, AppResult},
    state::AppState,
};

/// `ActivityPub::FeaturedCollectionsController::PER_PAGE`.
const COLLECTIONS_PER_PAGE: i64 = 5;

/// The context `ActivityPub::Adapter` gives a FeaturedCollection: what
/// `ActivityPub::FeaturedCollectionSerializer` and the `FeaturedItem` and
/// topic serializers nested in it declare.
fn collection_context() -> Value {
    super::context_helper::serialized_context(
        &["activitystreams"],
        &[
            "discoverable",
            "featured_collections",
            "hashtag",
            "sensitive",
        ],
    )
}

/// `TagManager#uri_for` a local collection.
pub(crate) fn collection_uri(domain: &str, account_id: i64, id: i64) -> String {
    format!("https://{domain}/ap/users/{account_id}/collections/{id}")
}

/// `TagManager#url_for` a collection: its page.
pub(crate) fn collection_url(domain: &str, id: i64) -> String {
    format!("https://{domain}/collections/{id}")
}

/// `TagManager#uri_for` an item of a local collection.
fn item_uri(domain: &str, account_id: i64, item_id: i64) -> String {
    format!("https://{domain}/ap/users/{account_id}/collection_items/{item_id}")
}

/// `ap_account_feature_authorization_url`: the stamp by which a local
/// account consented to being featured.
fn feature_authorization_uri(domain: &str, account_id: i64, item_id: i64) -> String {
    format!("https://{domain}/ap/users/{account_id}/feature_authorizations/{item_id}")
}

/// `ap_account_featured_collections_url`: an account's collections.
pub(crate) fn featured_collections_uri(domain: &str, account_id: i64) -> String {
    format!("https://{domain}/ap/users/{account_id}/featured_collections")
}

/// Resolve a member account's actor URI. Local accounts use their id_scheme-aware
/// canonical URI (the stored `uri` is empty for Mastodon-imported locals); remote
/// accounts use their stored `uri`.
pub(super) fn resolve_actor_uri(
    domain: &str,
    stored: Option<String>,
    is_local: bool,
    id: i64,
    id_scheme: Option<i32>,
    username: &str,
) -> String {
    if is_local {
        crate::federation::tag::account_uri(domain, id, id_scheme, username)
    } else {
        stored.unwrap_or_default()
    }
}

/// The account an item features: its id, whether it is local, its username,
/// URI scheme and stored URI.
type Featured = (i64, bool, String, Option<i32>, Option<String>);

/// `ActivityPub::FeaturedItemSerializer` on an item of a local collection.
fn featured_item(
    domain: &str,
    owner_id: i64,
    id: i64,
    account: Option<Featured>,
    approval_uri: Option<String>,
    created_at: chrono::NaiveDateTime,
) -> Value {
    let (featured_object, feature_authorization) = match account {
        Some((account_id, local, username, id_scheme, uri)) => (
            Some(resolve_actor_uri(
                domain, uri, local, account_id, id_scheme, &username,
            )),
            if local {
                Some(feature_authorization_uri(domain, account_id, id))
            } else {
                approval_uri
            },
        ),
        None => (None, approval_uri),
    };
    json!({
        "id": item_uri(domain, owner_id, id),
        "type": "FeaturedItem",
        "featuredObject": featured_object,
        "featureAuthorization": feature_authorization,
        "published": created_at.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

/// A collection's accepted items (`accepted_collection_items`), as
/// FeaturedItems.
async fn accepted_items(
    state: &AppState,
    domain: &str,
    owner_id: i64,
    collection_id: i64,
) -> AppResult<Vec<Value>> {
    let rows = sqlx::query!(
        r#"SELECT ci.id, ci.created_at, ci.approval_uri,
                  a.id AS "account_id?", a.uri AS account_uri, a.username AS "username?",
                  a.id_scheme, (a.domain IS NULL) AS "local?"
           FROM collection_items ci
           LEFT JOIN accounts a ON a.id = ci.account_id
           WHERE ci.collection_id = $1 AND ci.state = 1
           ORDER BY ci.position ASC, ci.id ASC"#,
        collection_id,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let account = match (r.account_id, r.username) {
                (Some(id), Some(username)) => Some((
                    id,
                    r.local.unwrap_or(false),
                    username,
                    r.id_scheme,
                    r.account_uri,
                )),
                _ => None,
            };
            featured_item(
                domain,
                owner_id,
                r.id,
                account,
                r.approval_uri,
                r.created_at,
            )
        })
        .collect())
}

/// Loaded collection fields needed for AP serialization.
pub struct ApCollection {
    pub id: i64,
    pub owner_id: i64,
    pub owner_id_scheme: Option<i32>,
    pub owner_username: String,
    pub name: String,
    pub description: Option<String>,
    pub language: Option<String>,
    pub sensitive: bool,
    pub discoverable: bool,
    pub tag_name: Option<String>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

impl ApCollection {
    /// Its owner's actor URI.
    #[must_use]
    pub fn actor_uri(&self, domain: &str) -> String {
        crate::federation::tag::account_uri(
            domain,
            self.owner_id,
            self.owner_id_scheme,
            &self.owner_username,
        )
    }

    /// Its own URI.
    #[must_use]
    pub fn uri(&self, domain: &str) -> String {
        collection_uri(domain, self.owner_id, self.id)
    }
}

/// Load a local collection (with its owner) for AP serialization.
pub async fn load_ap_collection(state: &AppState, id: i64) -> AppResult<Option<ApCollection>> {
    let row = sqlx::query!(
        r#"SELECT c.id, c.account_id, c.name, c.description, c.language, c.sensitive,
                  c.discoverable, c.created_at, c.updated_at, a.username, a.id_scheme,
                  t.name AS "tag_name?"
           FROM collections c
           JOIN accounts a ON a.id = c.account_id
           LEFT JOIN tags t ON t.id = c.tag_id
           WHERE c.id = $1 AND c.local = true AND a.domain IS NULL"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?;

    Ok(row.map(|r| ApCollection {
        id: r.id,
        owner_id: r.account_id,
        owner_id_scheme: r.id_scheme,
        owner_username: r.username,
        name: r.name,
        description: r.description,
        language: r.language,
        sensitive: r.sensitive,
        discoverable: r.discoverable,
        tag_name: r.tag_name,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }))
}

/// Build the FeaturedCollection AP object body (without `@context`), as
/// `ActivityPub::FeaturedCollectionSerializer` writes it.
pub async fn featured_collection_body(
    state: &AppState,
    domain: &str,
    c: &ApCollection,
) -> AppResult<Value> {
    let items = accepted_items(state, domain, c.owner_id, c.id).await?;
    let mut obj = json!({
        "id": c.uri(domain),
        "type": "FeaturedCollection",
        "totalItems": items.len(),
        "name": c.name,
        "attributedTo": c.actor_uri(domain),
        "url": collection_url(domain, c.id),
        "sensitive": c.sensitive,
        "discoverable": c.discoverable,
        "published": c.created_at.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "updated": c.updated_at.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });
    let members = obj.as_object_mut().expect("an object");
    match c.language.as_deref().filter(|l| !l.is_empty()) {
        Some(lang) => {
            members.insert("summaryMap".into(), json!({ lang: c.description }));
        }
        None => {
            members.insert("summary".into(), json!(c.description));
        }
    }
    // `topic`, through `ActivityPub::NoteSerializer::TagSerializer`.
    members.insert(
        "topic".into(),
        c.tag_name.as_ref().map_or(Value::Null, |name| {
            json!({
                "type": "Hashtag",
                "href": format!("https://{domain}/tags/{name}"),
                "name": format!("#{name}"),
            })
        }),
    );
    members.insert("orderedItems".into(), Value::Array(items));
    Ok(obj)
}

/// Whether the verified signer of a request, `signer`, is an account that
/// `owner_id` blocks or whose domain it blocks
/// (`blocking_or_domain_blocking?`): what `CollectionPolicy#show?` and
/// `AccountPolicy#index_collections?` refuse.
async fn blocks(state: &AppState, owner_id: i64, signer: Option<&str>) -> AppResult<bool> {
    let Some(signer) = signer else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar!(
        r#"SELECT (EXISTS (SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = a.id)
                   OR EXISTS (SELECT 1 FROM account_domain_blocks
                              WHERE account_id = $1 AND domain = a.domain)) AS "blocked!"
           FROM accounts a WHERE a.uri = $2 AND a.domain IS NOT NULL"#,
        owner_id,
        signer,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false))
}

/// `body` with `context` as its `@context`, before its other members.
fn with_context(body: &mut Value, context: Value) {
    if let Value::Object(members) = body {
        let mut ordered = serde_json::Map::new();
        ordered.insert("@context".into(), context);
        ordered.append(members);
        *members = ordered;
    }
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

/// `CollectionsController#show`: a local collection, at `/collections/{id}`
/// or, when `owner` names its owner, at
/// `/ap/users/{account_id}/collections/{id}`; not there for a signer its
/// owner blocks. Says too when it was last updated, which its caching
/// depends on.
pub async fn collection_document(
    state: &AppState,
    domain: &str,
    owner: Option<i64>,
    id: i64,
    signer: Option<&str>,
) -> AppResult<(Value, chrono::NaiveDateTime)> {
    let c = load_ap_collection(state, id)
        .await?
        .filter(|c| owner.is_none_or(|owner| owner == c.owner_id))
        .ok_or(AppError::NotFound)?;
    if blocks(state, c.owner_id, signer).await? {
        return Err(AppError::NotFound);
    }
    let mut body = featured_collection_body(state, domain, &c).await?;
    with_context(&mut body, collection_context());
    Ok((body, c.updated_at))
}

/// `CollectionItemsController#show`: an item of one of the local account
/// `owner`'s collections, as a FeaturedItem; not there for a signer the
/// owner blocks.
pub async fn collection_item_document(
    state: &AppState,
    domain: &str,
    owner: i64,
    id: i64,
    signer: Option<&str>,
) -> AppResult<Value> {
    let r = sqlx::query!(
        r#"SELECT ci.id, ci.created_at, ci.approval_uri,
                  a.id AS "account_id?", a.uri AS account_uri, a.username AS "username?",
                  a.id_scheme, (a.domain IS NULL) AS "local?"
           FROM collection_items ci
           JOIN collections c ON c.id = ci.collection_id
           JOIN accounts o ON o.id = c.account_id AND o.domain IS NULL
           LEFT JOIN accounts a ON a.id = ci.account_id
           WHERE ci.id = $1 AND c.account_id = $2"#,
        id,
        owner,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    if blocks(state, owner, signer).await? {
        return Err(AppError::NotFound);
    }
    let account = match (r.account_id, r.username) {
        (Some(id), Some(username)) => Some((
            id,
            r.local.unwrap_or(false),
            username,
            r.id_scheme,
            r.account_uri,
        )),
        _ => None,
    };
    let mut body = featured_item(domain, owner, r.id, account, r.approval_uri, r.created_at);
    with_context(
        &mut body,
        super::context_helper::serialized_context(&["activitystreams"], &["featured_collections"]),
    );
    Ok(body)
}

/// `ActivityPub::FeaturedCollectionsController#index`: the local account
/// `owner`'s collections, five to a page (`?page=`), each page embedding
/// its FeaturedCollections; not there for a signer the account blocks.
pub async fn featured_collections(
    state: &AppState,
    domain: &str,
    owner: i64,
    page: Option<&str>,
    signer: Option<&str>,
) -> AppResult<Value> {
    let account = super::objects::load_local_account(state, AccountRef::Id(owner)).await?;
    if blocks(state, account.id, signer).await? {
        return Err(AppError::NotFound);
    }
    let total = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM collections WHERE account_id = $1",
        account.id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);
    let uri = featured_collections_uri(domain, account.id);
    let parse = |uri: &str| url::Url::parse(uri).map_err(|error| AppError::Internal(error.into()));
    let total_items = Some(u64::try_from(total).unwrap_or(0));

    // `params[:page].present?`.
    let Some(page) = page.filter(|page| !page.trim().is_empty()) else {
        let mut document = CollectionDocument {
            id: Some(parse(&uri)?),
            total_items,
            first: Some(json!(format!("{uri}?page=1"))),
            ..CollectionDocument::default()
        }
        .to_value();
        with_context(
            &mut document,
            json!("https://www.w3.org/ns/activitystreams"),
        );
        return Ok(document);
    };
    // Kaminari's `page`: a number from one, anything else the first.
    let number = page
        .trim()
        .parse::<i64>()
        .ok()
        .filter(|n| *n >= 1)
        .unwrap_or(1);
    let ids = sqlx::query_scalar!(
        "SELECT id FROM collections WHERE account_id = $1 ORDER BY id OFFSET $2 LIMIT $3",
        account.id,
        (number - 1).saturating_mul(COLLECTIONS_PER_PAGE),
        COLLECTIONS_PER_PAGE,
    )
    .fetch_all(&state.db)
    .await?;
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(c) = load_ap_collection(state, id).await? {
            items.push(featured_collection_body(state, domain, &c).await?);
        }
    }
    let pages = (total + COLLECTIONS_PER_PAGE - 1) / COLLECTIONS_PER_PAGE;
    let page_uri = |n: i64| parse(&format!("{uri}?page={n}"));
    let mut id = parse(&uri)?;
    id.query_pairs_mut().append_pair("page", page);
    let context = if items.is_empty() {
        json!("https://www.w3.org/ns/activitystreams")
    } else {
        collection_context()
    };
    let mut document = CollectionDocument {
        id: Some(id),
        total_items,
        next: if number < pages {
            Some(page_uri(number + 1)?)
        } else {
            None
        },
        prev: if number > 1 {
            Some(page_uri(number - 1)?)
        } else {
            None
        },
        part_of: Some(parse(&uri)?),
        items: Some(items),
        ..CollectionDocument::default()
    }
    .to_value();
    with_context(&mut document, context);
    Ok(document)
}

/// `/users/{username}/collections`, where eunha used to say an account's
/// collections were: its discoverable collections' URIs. Mastodon serves
/// them at `featured_collections` ([`featured_collections`]), which actors
/// now name.
pub async fn account_collections(
    state: &AppState,
    domain: &str,
    who: AccountRef<'_>,
) -> AppResult<Value> {
    let account = super::objects::load_local_account(state, who).await?;

    let ids = sqlx::query_scalar!(
        "SELECT id FROM collections WHERE account_id = $1 AND discoverable = true ORDER BY created_at DESC",
        account.id,
    )
    .fetch_all(&state.db)
    .await?;

    let items: Vec<String> = ids
        .into_iter()
        .map(|id| collection_uri(domain, account.id, id))
        .collect();

    let body = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!(
            "{}/collections",
            crate::federation::tag::account_uri_of(domain, &account)
        ),
        "type": "OrderedCollection",
        "totalItems": items.len(),
        "orderedItems": items,
    });
    Ok(body)
}

/// `ActivityPub::FeatureAuthorizationsController#show`: the stamp proving
/// the local account `who` consented to being featured in a collection,
/// served at the URI it was asked at, which is the one it was issued
/// under: `/ap/users/{account_id}/feature_authorizations/{id}`, or
/// `/users/{username}/feature_authorizations/{id}`, where eunha issues
/// them. Not there for a signer the collection's owner blocks.
pub async fn feature_authorization_document(
    state: &AppState,
    domain: &str,
    who: AccountRef<'_>,
    id: i64,
    signer: Option<&str>,
) -> AppResult<Value> {
    let account = super::objects::load_local_account(state, who).await?;
    let row = sqlx::query!(
        r#"SELECT c.local AS collection_local, c.id AS collection_id, c.account_id AS owner_id,
                  c.uri AS "collection_uri?"
           FROM collection_items ci
           JOIN collections c ON c.id = ci.collection_id
           WHERE ci.id = $1 AND ci.state = 1 AND ci.account_id = $2"#,
        id,
        account.id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    if blocks(state, row.owner_id, signer).await? {
        return Err(AppError::NotFound);
    }

    let auth_id = match who {
        AccountRef::Id(_) => feature_authorization_uri(domain, account.id, id),
        AccountRef::Username(username) => {
            format!("https://{domain}/users/{username}/feature_authorizations/{id}")
        }
    };
    let collection_uri = match row.collection_uri {
        Some(uri) if !uri.is_empty() && !row.collection_local => uri,
        _ => collection_uri(domain, row.owner_id, row.collection_id),
    };
    let account_uri = crate::federation::tag::account_uri_of(domain, &account);

    crate::federation::consent::feature_authorization(&auth_id, &collection_uri, &account_uri)
        .map_err(AppError::Internal)
}

/// `ActivityPub::QuoteAuthorizationsController#show`: the stamp by which a
/// local account authorized a quote of one of its posts, at
/// `/users/{username}/quote_authorizations/{id}` (or under `/ap/users/{id}`).
/// Only an accepted quote of `who`'s whose two statuses are both still there
/// has one, and only a quoted status `reader` may be shown
/// (`StatusPolicy#show?`). Says too whether the quoted status is
/// distributable, which its caching depends on.
pub async fn quote_authorization_document(
    state: &AppState,
    who: AccountRef<'_>,
    id: i64,
    reader: Option<&super::objects::Reader>,
) -> AppResult<(Value, bool)> {
    let account = super::objects::load_local_account(state, who).await?;
    let quote = crate::quotes::find(&state.db, id)
        .await?
        .filter(|q| q.accepted() && q.quoted_account_id == Some(account.id))
        .ok_or(AppError::NotFound)?;
    let quoted_status_id = quote.quoted_status_id.ok_or(AppError::NotFound)?;
    // `@quote.status.present? && @quote.quoted_status.present?`.
    let statuses = sqlx::query!(
        r#"SELECT id, account_id, visibility FROM statuses
           WHERE id = ANY($1::bigint[]) AND deleted_at IS NULL"#,
        &[quote.status_id, quoted_status_id][..],
    )
    .fetch_all(&state.db)
    .await?;
    let Some(quoted) = statuses.iter().find(|s| s.id == quoted_status_id) else {
        return Err(AppError::NotFound);
    };
    if !statuses.iter().any(|s| s.id == quote.status_id) {
        return Err(AppError::NotFound);
    }
    // `authorize @quote.quoted_status, :show?`, its author unavailable
    // included.
    let author_unavailable = sqlx::query_scalar!(
        r#"SELECT (suspended_at IS NOT NULL OR requested_deletion_at IS NOT NULL) AS "u!"
           FROM accounts WHERE id = $1"#,
        quoted.account_id,
    )
    .fetch_one(&state.db)
    .await?;
    if author_unavailable
        || !super::objects::may_show(
            state,
            quoted.account_id,
            quoted.id,
            quoted.visibility,
            reader,
        )
        .await?
    {
        return Err(AppError::NotFound);
    }
    let distributable = matches!(
        quoted.visibility,
        crate::db::models::vis::PUBLIC | crate::db::models::vis::UNLISTED
    );

    let mut body = crate::quotes::authorization_object(state, &quote, true)
        .await
        .map_err(AppError::Internal)?
        .ok_or(AppError::NotFound)?;
    body["@context"] = crate::federation::consent::quote_authorization_context();
    Ok((body, distributable))
}

// ── Activity builders (for outbound distribution to followers) ─────────────────

/// `ActivityPub::AddFeaturedCollectionSerializer`: a new collection was
/// created.
#[must_use]
pub fn add_collection_activity(domain: &str, c: &ApCollection, collection_obj: Value) -> Value {
    json!({
        "@context": collection_context(),
        "type": "Add",
        "actor": c.actor_uri(domain),
        "target": featured_collections_uri(domain, c.owner_id),
        "object": collection_obj,
    })
}

/// `ActivityPub::UpdateFeaturedCollectionSerializer`: a collection's
/// metadata or items changed.
#[must_use]
pub fn update_collection_activity(domain: &str, c: &ApCollection, collection_obj: Value) -> Value {
    json!({
        "@context": collection_context(),
        "id": format!("{}#updates/{}", c.uri(domain), c.updated_at.and_utc().timestamp()),
        "type": "Update",
        "actor": c.actor_uri(domain),
        "to": [super::objects::PUBLIC_COLLECTION],
        "object": collection_obj,
    })
}

/// `ActivityPub::RemoveFeaturedCollectionSerializer`: a collection was
/// deleted.
#[must_use]
pub fn remove_collection_activity(
    domain: &str,
    actor: &str,
    owner_id: i64,
    collection_id: i64,
) -> Value {
    json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "type": "Remove",
        "actor": actor,
        "target": featured_collections_uri(domain, owner_id),
        "object": collection_uri(domain, owner_id, collection_id),
    })
}
