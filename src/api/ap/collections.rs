//! ActivityPub representation of collections (Mastodon's FeaturedCollection).
//!
//! Serves a local account's collections as AP objects so remote servers can
//! discover and fetch them, and provides the activity builders used to
//! distribute collection changes to followers.
//!
//! The bidirectional feature-request / feature-authorization handshake (for
//! featuring *remote* accounts with their consent) is not yet implemented; only
//! locally-owned collections and their accepted items are federated outbound.

use serde_json::{json, Value};

use super::objects::AccountRef;
use crate::{
    error::{AppError, AppResult},
    state::AppState,
};

/// JSON-LD context for FeaturedCollection objects.
fn collection_context() -> Value {
    json!([
        "https://www.w3.org/ns/activitystreams",
        {
            "toot": "http://joinmastodon.org/ns#",
            "sensitive": "as:sensitive",
            "discoverable": "toot:discoverable",
            "Hashtag": "as:Hashtag",
            "featuredCollections": { "@id": "toot:featuredCollections", "@type": "@id" },
            "FeaturedCollection": "toot:FeaturedCollection",
            "FeaturedItem": "toot:FeaturedItem",
            "featuredObject": { "@id": "toot:featuredObject", "@type": "@id" },
        }
    ])
}

fn collection_uri(domain: &str, id: i64) -> String {
    format!("https://{domain}/collections/{id}")
}

fn item_uri(domain: &str, collection_id: i64, item_id: i64) -> String {
    format!("https://{domain}/collections/{collection_id}/items/{item_id}")
}

/// Username-scheme local actor URI, for collection/featured contexts that only
/// carry the username. (Numeric-scheme accounts have no local collections to
/// serve, so the username form is sufficient here.)
fn actor_uri(domain: &str, username: &str) -> String {
    format!("https://{domain}/users/{username}")
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

/// A single accepted item, ready for FeaturedItem serialization.
struct ItemRow {
    id: i64,
    account_uri: String,
    created_at: chrono::NaiveDateTime,
}

/// Fetch a collection's accepted items joined with each account's AP URI.
async fn accepted_items(
    state: &AppState,
    domain: &str,
    collection_id: i64,
) -> AppResult<Vec<ItemRow>> {
    let rows = sqlx::query!(
        r#"SELECT ci.id, ci.created_at,
                  a.uri AS "account_uri?", a.username, a.domain
           FROM collection_items ci
           JOIN accounts a ON a.id = ci.account_id
           WHERE ci.collection_id = $1 AND ci.state = 1
           ORDER BY ci.position ASC, ci.id ASC"#,
        collection_id,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| {
            // Local accounts (domain NULL) may not have a stored uri; derive it.
            let account_uri = match (r.account_uri, r.domain) {
                (Some(uri), _) if !uri.is_empty() => uri,
                _ => actor_uri(domain, &r.username),
            };
            ItemRow {
                id: r.id,
                account_uri,
                created_at: r.created_at,
            }
        })
        .collect())
}

/// Build a FeaturedItem AP object (without `@context`, for embedding).
fn featured_item_object(domain: &str, collection_id: i64, item: &ItemRow) -> Value {
    json!({
        "id": item_uri(domain, collection_id, item.id),
        "type": "FeaturedItem",
        "featuredObject": item.account_uri,
        "published": item.created_at.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

/// Loaded collection fields needed for AP serialization.
pub struct ApCollection {
    pub id: i64,
    pub owner_username: String,
    pub name: String,
    pub description: Option<String>,
    pub language: Option<String>,
    pub sensitive: bool,
    pub discoverable: bool,
    pub url: Option<String>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

/// Load a local collection (with its owner's username) for AP serialization.
pub async fn load_ap_collection(state: &AppState, id: i64) -> AppResult<Option<ApCollection>> {
    let row = sqlx::query!(
        r#"SELECT c.id, c.name, c.description, c.language, c.sensitive,
                  c.discoverable, c.url, c.created_at, c.updated_at, a.username
           FROM collections c
           JOIN accounts a ON a.id = c.account_id
           WHERE c.id = $1 AND c.local = true AND a.domain IS NULL"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?;

    Ok(row.map(|r| ApCollection {
        id: r.id,
        owner_username: r.username,
        name: r.name,
        description: r.description,
        language: r.language,
        sensitive: r.sensitive,
        discoverable: r.discoverable,
        url: r.url,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }))
}

/// Build the FeaturedCollection AP object body (without `@context`).
pub async fn featured_collection_body(
    state: &AppState,
    domain: &str,
    c: &ApCollection,
) -> AppResult<Value> {
    let items = accepted_items(state, domain, c.id).await?;
    let ordered: Vec<Value> = items
        .iter()
        .map(|it| featured_item_object(domain, c.id, it))
        .collect();

    let mut obj = json!({
        "id": collection_uri(domain, c.id),
        "type": "FeaturedCollection",
        "name": c.name,
        "attributedTo": actor_uri(domain, &c.owner_username),
        "url": c.url.clone().unwrap_or_else(|| collection_uri(domain, c.id)),
        "sensitive": c.sensitive,
        "discoverable": c.discoverable,
        "published": c.created_at.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "updated": c.updated_at.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "totalItems": ordered.len(),
        "orderedItems": ordered,
    });

    if let Some(desc) = &c.description {
        if let Some(lang) = &c.language {
            obj["summaryMap"] = json!({ lang: desc });
        } else {
            obj["summary"] = json!(desc);
        }
    }

    Ok(obj)
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

/// `/collections/{id}` — the FeaturedCollection AP object.
pub async fn collection_document(state: &AppState, domain: &str, id: i64) -> AppResult<Value> {
    let c = load_ap_collection(state, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let mut body = featured_collection_body(state, domain, &c).await?;
    body["@context"] = collection_context();
    Ok(body)
}

/// `/users/{username}/collections` — an OrderedCollection of the account's
/// FeaturedCollection object URIs.
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
        .map(|id| collection_uri(domain, id))
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

/// `/users/{username}/feature_authorizations/{id}` — the FeatureAuthorization
/// stamp proving a local account consented to being featured in a collection.
pub async fn feature_authorization_document(
    state: &AppState,
    domain: &str,
    username: &str,
    id: i64,
) -> AppResult<Value> {
    let row = sqlx::query!(
        r#"SELECT c.local AS collection_local, c.id AS collection_id,
                  c.uri AS "collection_uri?", a.uri AS "account_uri?"
           FROM collection_items ci
           JOIN collections c ON c.id = ci.collection_id
           JOIN accounts a ON a.id = ci.account_id
           WHERE ci.id = $1 AND ci.state = 1
             AND a.username = $2 AND a.domain IS NULL"#,
        id,
        username,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let auth_id = format!("https://{domain}/users/{username}/feature_authorizations/{id}");
    let collection_uri = match row.collection_uri {
        Some(uri) if !uri.is_empty() => uri,
        _ => collection_uri(domain, row.collection_id),
    };
    let account_uri = match row.account_uri {
        Some(uri) if !uri.is_empty() => uri,
        _ => actor_uri(domain, username),
    };

    let mut body =
        crate::federation::consent::feature_authorization(&auth_id, &collection_uri, &account_uri)
            .map_err(AppError::Internal)?;
    body["@context"] = json!([
        "https://www.w3.org/ns/activitystreams",
        {
            "toot": "http://joinmastodon.org/ns#",
            "FeatureAuthorization": "toot:FeatureAuthorization",
            "interactingObject": { "@id": "toot:interactingObject", "@type": "@id" },
            "interactionTarget": { "@id": "toot:interactionTarget", "@type": "@id" },
        }
    ]);
    Ok(body)
}

/// `/users/{username}/quote_authorizations/{id}` — the QuoteAuthorization
/// stamp proving a local account authorized a quote of one of its posts.
pub async fn quote_authorization_document(
    state: &AppState,
    domain: &str,
    username: &str,
    id: i64,
) -> AppResult<Value> {
    let row = sqlx::query!(
        r#"SELECT qs.uri AS "quoted_status_uri?", ss.uri AS "quoting_status_uri?",
                  qa.id AS quoted_account_id, qa.id_scheme AS quoted_account_id_scheme
           FROM quotes q
           JOIN statuses qs ON qs.id = q.quoted_status_id
           JOIN statuses ss ON ss.id = q.status_id
           JOIN accounts qa ON qa.id = q.quoted_account_id
           WHERE q.id = $1 AND q.state = 1
             AND qa.username = $2 AND qa.domain IS NULL"#,
        id,
        username,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let (Some(quoted_status_uri), Some(quoting_status_uri)) =
        (row.quoted_status_uri, row.quoting_status_uri)
    else {
        return Err(AppError::NotFound);
    };

    let auth_id = format!("https://{domain}/users/{username}/quote_authorizations/{id}");
    // The quoted account is local, so its actor id follows from its id scheme.
    let quoted_account_uri = crate::federation::tag::account_uri(
        domain,
        row.quoted_account_id,
        row.quoted_account_id_scheme,
        username,
    );
    let mut body = crate::federation::consent::quote_authorization(
        &auth_id,
        &quoted_account_uri,
        &quoting_status_uri,
        &quoted_status_uri,
    )
    .map_err(AppError::Internal)?;
    body["@context"] = json!([
        "https://www.w3.org/ns/activitystreams",
        {
            "toot": "http://joinmastodon.org/ns#",
            "QuoteAuthorization": "toot:QuoteAuthorization",
            "interactingObject": { "@id": "toot:interactingObject", "@type": "@id" },
            "interactionTarget": { "@id": "toot:interactionTarget", "@type": "@id" },
        }
    ]);
    Ok(body)
}

// ── Activity builders (for outbound distribution to followers) ─────────────────

/// `Add(FeaturedCollection)` — a new collection was created.
pub fn add_collection_activity(domain: &str, owner_username: &str, collection_obj: Value) -> Value {
    let actor = actor_uri(domain, owner_username);
    json!({
        "@context": collection_context(),
        "type": "Add",
        "actor": actor,
        "target": format!("{actor}/collections"),
        "object": collection_obj,
    })
}

/// `Update(FeaturedCollection)` — a collection's metadata or items changed.
pub fn update_collection_activity(
    domain: &str,
    owner_username: &str,
    collection_id: i64,
    updated_unix: i64,
    collection_obj: Value,
) -> Value {
    let actor = actor_uri(domain, owner_username);
    json!({
        "@context": collection_context(),
        "id": format!("{}#updates/{}", collection_uri(domain, collection_id), updated_unix),
        "type": "Update",
        "actor": actor,
        "to": ["https://www.w3.org/ns/activitystreams#Public"],
        "object": collection_obj,
    })
}

/// `Remove` — a collection was deleted.
pub fn remove_collection_activity(domain: &str, owner_username: &str, collection_id: i64) -> Value {
    let actor = actor_uri(domain, owner_username);
    json!({
        "@context": collection_context(),
        "type": "Remove",
        "actor": actor,
        "target": format!("{actor}/collections"),
        "object": collection_uri(domain, collection_id),
    })
}
