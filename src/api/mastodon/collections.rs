//! Collections API (Mastodon 4.6.0).
//!
//! Collections are curated, account-featuring lists with an approval workflow
//! (`pending` / `accepted` / `rejected` / `revoked`). Local accounts added to a
//! local collection are auto-accepted; remote accounts start `pending`.
//!
//! This implements the local REST surface. ActivityPub federation of
//! collections (Add/Remove/feature-request distribution) is not yet wired up.

use axum::{
    extract::{Path, Query},
    response::IntoResponse,
    Extension, Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    db::models::Account,
    error::{AppError, AppResult},
    middleware::{AuthenticatedUser, ResolvedInstance},
    state::AppState,
};

use crate::api::ap::collections as ap_coll;

const DEFAULT_LIMIT: i64 = 40;
const MAX_LIMIT: i64 = 100;
const MAX_ITEMS: i64 = 25;
const NAME_MAX: usize = 40;
const DESCRIPTION_MAX: usize = 100;
const DEFAULT_COLLECTION_LIMIT: i64 = 10;

// State enum: pending=0, accepted=1, rejected=2, revoked=3
fn state_str(state: i32) -> &'static str {
    match state {
        1 => "accepted",
        2 => "rejected",
        3 => "revoked",
        _ => "pending",
    }
}

/// `TagManager#uri_for` a local collection.
pub(crate) fn collection_uri(domain: &str, account_id: i64, id: i64) -> String {
    ap_coll::collection_uri(domain, account_id, id)
}

fn tag_url(domain: &str, name: &str) -> String {
    format!("https://{domain}/tags/{name}")
}

#[derive(Debug, Deserialize, Default)]
pub struct OffsetParams {
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}

impl OffsetParams {
    fn offset(&self) -> i64 {
        self.offset.unwrap_or(0).max(0)
    }
    fn limit(&self) -> i64 {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }
}

/// A loaded collection row plus its (optional) tag name.
struct CollectionRow {
    id: i64,
    account_id: i64,
    name: String,
    description: Option<String>,
    description_html: Option<String>,
    language: Option<String>,
    sensitive: bool,
    discoverable: bool,
    local: bool,
    uri: Option<String>,
    url: Option<String>,
    tag_name: Option<String>,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

async fn load_collection(state: &AppState, id: i64) -> AppResult<Option<CollectionRow>> {
    let row = sqlx::query!(
        r#"SELECT c.id, c.account_id, c.name, c.description, c.description_html,
                  c.language, c.sensitive, c.discoverable, c.local, c.uri, c.url,
                  c.created_at, c.updated_at, t.name AS "tag_name?"
           FROM collections c
           LEFT JOIN tags t ON t.id = c.tag_id
           WHERE c.id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?;

    Ok(row.map(|r| CollectionRow {
        id: r.id,
        account_id: r.account_id,
        name: r.name,
        description: r.description,
        description_html: r.description_html,
        language: r.language,
        sensitive: r.sensitive,
        discoverable: r.discoverable,
        local: r.local,
        uri: r.uri,
        url: r.url,
        tag_name: r.tag_name,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }))
}

/// Items visible to `viewer`: the owner sees pending+accepted, others only
/// accepted; items whose account is blocked by the viewer are excluded.
async fn visible_items(
    state: &AppState,
    c: &CollectionRow,
    viewer_id: Option<i64>,
) -> AppResult<Vec<(i64, i32, chrono::NaiveDateTime, Option<i64>)>> {
    let is_owner = viewer_id == Some(c.account_id);
    let rows = sqlx::query!(
        r#"SELECT ci.id, ci.state, ci.created_at, ci.account_id
           FROM collection_items ci
           WHERE ci.collection_id = $1
             AND ( ($2 AND ci.state IN (0, 1)) OR (NOT $2 AND ci.state = 1) )
             AND ( $3::bigint IS NULL OR ci.account_id IS NULL OR NOT EXISTS (
                   SELECT 1 FROM blocks b
                   WHERE b.account_id = $3 AND b.target_account_id = ci.account_id) )
           ORDER BY ci.position ASC, ci.id ASC"#,
        c.id,
        is_owner,
        viewer_id,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| (r.id, r.state, r.created_at, r.account_id))
        .collect())
}

fn item_entity(
    id: i64,
    state: i32,
    created_at: chrono::NaiveDateTime,
    account_id: Option<i64>,
) -> Value {
    let mut v = json!({
        "id": id.to_string(),
        "state": state_str(state),
        "created_at": created_at.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    });
    // account_id only included for pending/accepted items.
    if matches!(state, 0 | 1) {
        if let Some(aid) = account_id {
            v["account_id"] = Value::String(aid.to_string());
        }
    }
    v
}

/// Build the `REST::CollectionSerializer` JSON for a collection.
async fn collection_entity(
    state: &AppState,
    domain: &str,
    c: &CollectionRow,
    viewer_id: Option<i64>,
) -> AppResult<Value> {
    let items = visible_items(state, c, viewer_id).await?;
    let items_json: Vec<Value> = items
        .iter()
        .map(|(id, st, created, aid)| item_entity(*id, *st, *created, *aid))
        .collect();

    let uri = c
        .uri
        .clone()
        .unwrap_or_else(|| collection_uri(domain, c.account_id, c.id));
    // `TagManager#url_for`: a local collection's page.
    let url = if c.local {
        ap_coll::collection_url(domain, c.id)
    } else {
        c.url.clone().unwrap_or_else(|| uri.clone())
    };

    // `REST::CollectionSerializer#description`: a local collection's as
    // written, a remote one's HTML through `MASTODON_STRICT`.
    let description = if c.local {
        c.description.clone()
    } else {
        c.description_html
            .as_deref()
            .map(crate::formatter::sanitize::strict)
    };

    let tag = c
        .tag_name
        .as_ref()
        .map(|name| json!({ "name": name, "url": tag_url(domain, name) }));

    Ok(json!({
        "id": c.id.to_string(),
        "uri": uri,
        "name": c.name,
        "description": description,
        "language": c.language,
        "account_id": c.account_id.to_string(),
        "local": c.local,
        "sensitive": c.sensitive,
        "discoverable": c.discoverable,
        "url": url,
        "item_count": items_json.len(),
        "created_at": c.created_at.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "updated_at": c.updated_at.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "tag": tag,
        "items": items_json,
    }))
}

/// A collection as `REST::CollectionSerializer` renders it for `viewer`, or
/// `None` when it is gone.
pub(crate) async fn render(
    state: &AppState,
    collection_id: i64,
    viewer_id: Option<i64>,
) -> AppResult<Option<Value>> {
    let Some(c) = load_collection(state, collection_id).await? else {
        return Ok(None);
    };
    collection_entity(state, &state.instance.domain, &c, viewer_id)
        .await
        .map(Some)
}

/// `REST::StatusSerializer#tagged_collections` for each of `status_ids`:
/// the collections its `tagged_objects` name (`FeaturedCollection`s), as
/// `REST::CollectionSerializer` writes them, for a reader who is no one in
/// particular.
pub(crate) async fn tagged_collections(
    state: &AppState,
    status_ids: &[i64],
) -> AppResult<std::collections::HashMap<i64, Vec<Value>>> {
    let mut by_status: std::collections::HashMap<i64, Vec<Value>> =
        std::collections::HashMap::new();
    if status_ids.is_empty() {
        return Ok(by_status);
    }
    let rows = sqlx::query!(
        r#"SELECT t.status_id, t.object_id AS "collection_id!"
           FROM tagged_objects t JOIN collections c ON c.id = t.object_id
           WHERE t.status_id = ANY($1) AND t.ap_type = 'FeaturedCollection'
             AND t.object_type = 'Collection'
           ORDER BY t.id"#,
        status_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let domain = &state.instance.domain;
    for row in rows {
        if let Some(c) = load_collection(state, row.collection_id).await? {
            by_status
                .entry(row.status_id)
                .or_default()
                .push(collection_entity(state, domain, &c, None).await?);
        }
    }
    Ok(by_status)
}

// ── GET /api/v1/accounts/{id}/collections ─────────────────────────────────

pub async fn account_collections(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Path(account_id): Path<i64>,
    Query(params): Query<OffsetParams>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Value>> {
    if let Some(Extension(a)) = &auth {
        a.require_scope("read:collections")?;
    }
    let viewer_id = auth.as_ref().map(|Extension(a)| a.account_id);
    let only_discoverable = viewer_id != Some(account_id);

    let ids = sqlx::query_scalar!(
        r#"SELECT id FROM collections
           WHERE account_id = $1 AND ($2 = false OR discoverable = true)
           ORDER BY created_at DESC
           OFFSET $3 LIMIT $4"#,
        account_id,
        only_discoverable,
        params.offset(),
        params.limit(),
    )
    .fetch_all(&state.db)
    .await?;

    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(c) = load_collection(&state, id).await? {
            out.push(collection_entity(&state, &instance.domain, &c, viewer_id).await?);
        }
    }

    Ok(Json(json!({ "collections": out })))
}

// ── GET /api/v1/accounts/{id}/in_collections ──────────────────────────────

pub async fn account_in_collections(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(account_id): Path<i64>,
    Query(params): Query<OffsetParams>,
) -> AppResult<Json<Value>> {
    auth.require_scope("read:collections")?;

    let ids = sqlx::query_scalar!(
        r#"SELECT DISTINCT c.id
           FROM collections c
           JOIN collection_items ci ON ci.collection_id = c.id
           WHERE ci.account_id = $1 AND ci.state IN (0, 1)
           ORDER BY c.id DESC
           OFFSET $2 LIMIT $3"#,
        account_id,
        params.offset(),
        params.limit(),
    )
    .fetch_all(&state.db)
    .await?;

    let viewer_id = Some(auth.account_id);
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(c) = load_collection(&state, id).await? {
            out.push(collection_entity(&state, &instance.domain, &c, viewer_id).await?);
        }
    }

    Ok(Json(json!({ "collections": out })))
}

// ── GET /api/v1/collections/{id} ──────────────────────────────────────────

pub async fn show_collection(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Path(id): Path<i64>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Value>> {
    if let Some(Extension(a)) = &auth {
        a.require_scope("read:collections")?;
    }
    let viewer_id = auth.as_ref().map(|Extension(a)| a.account_id);

    let c = load_collection(&state, id)
        .await?
        .ok_or(AppError::NotFound)?;
    let collection = collection_entity(&state, &instance.domain, &c, viewer_id).await?;

    // accounts = [owner] + accounts of visible (pending/accepted) items.
    let items = visible_items(&state, &c, viewer_id).await?;
    let mut account_ids: Vec<i64> = vec![c.account_id];
    for (_, st, _, aid) in &items {
        if matches!(st, 0 | 1) {
            if let Some(aid) = aid {
                if !account_ids.contains(aid) {
                    account_ids.push(*aid);
                }
            }
        }
    }

    let db_accounts = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
        &account_ids,
    )
    .fetch_all(&state.db)
    .await?;
    // Preserve order: owner first, then items in order.
    let by_id: std::collections::HashMap<i64, Account> =
        db_accounts.into_iter().map(|a| (a.id, a)).collect();
    let ordered: Vec<Account> = account_ids
        .iter()
        .filter_map(|id| by_id.get(id).cloned())
        .collect();
    let accounts = super::accounts::batch_accounts_to_api(&state, &ordered).await;

    Ok(Json(
        json!({ "collection": collection, "accounts": accounts }),
    ))
}

// ── POST /api/v1/collections ──────────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct CreateCollectionForm {
    pub name: Option<String>,
    pub description: Option<String>,
    pub language: Option<String>,
    pub sensitive: Option<bool>,
    pub discoverable: Option<bool>,
    pub tag_name: Option<String>,
    #[serde(default)]
    pub account_ids: Vec<String>,
}

async fn resolve_tag(state: &AppState, tag_name: &str) -> AppResult<i64> {
    crate::tags::find_or_create(&state.db, tag_name)
        .await?
        .ok_or_else(|| AppError::Unprocessable("tag_name is invalid".into()))
}

pub async fn create_collection(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<CreateCollectionForm>,
) -> AppResult<Json<Value>> {
    auth.require_scope("write:collections")?;

    let name = form.name.unwrap_or_default();
    let name = name.trim();
    if name.is_empty() {
        return Err(AppError::Unprocessable("name can't be blank".into()));
    }
    if name.chars().count() > NAME_MAX {
        return Err(AppError::Unprocessable(format!(
            "name is too long (maximum is {NAME_MAX} characters)"
        )));
    }
    if let Some(desc) = &form.description {
        if desc.chars().count() > DESCRIPTION_MAX {
            return Err(AppError::Unprocessable(format!(
                "description is too long (maximum is {DESCRIPTION_MAX} characters)"
            )));
        }
    }

    // Per-user collection limit (role.collection_limit, default 10).
    let existing = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM collections WHERE account_id = $1",
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);
    if existing >= DEFAULT_COLLECTION_LIMIT {
        return Err(AppError::Unprocessable(format!(
            "You cannot create more than {DEFAULT_COLLECTION_LIMIT} collections"
        )));
    }

    let tag_id = match &form.tag_name {
        Some(t) if !t.trim().is_empty() => Some(resolve_tag(&state, t).await?),
        _ => None,
    };

    let new_id = sqlx::query_scalar!(
        r#"INSERT INTO collections
             (account_id, name, description, language, sensitive, discoverable, local, tag_id, item_count, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, true, $7, 0, now(), now())
           RETURNING id"#,
        auth.account_id,
        name,
        form.description.as_deref(),
        form.language.as_deref(),
        form.sensitive.unwrap_or(false),
        form.discoverable.unwrap_or(false),
        tag_id,
    )
    .fetch_one(&state.db)
    .await?;

    // Add initial accounts, if any.
    for raw in &form.account_ids {
        if let Ok(aid) = raw.parse::<i64>() {
            let _ = add_item(&state, new_id, aid).await;
        }
    }

    let c = load_collection(&state, new_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let entity = collection_entity(&state, &instance.domain, &c, Some(auth.account_id)).await?;
    distribute_collection(&state, &instance.domain, new_id, auth.account_id, true).await;
    Ok(Json(json!({ "collection": entity })))
}

// ── PUT /api/v1/collections/{id} ──────────────────────────────────────────

#[derive(Debug, Deserialize, Default)]
pub struct UpdateCollectionForm {
    pub name: Option<String>,
    pub description: Option<String>,
    pub language: Option<String>,
    pub sensitive: Option<bool>,
    pub discoverable: Option<bool>,
    pub tag_name: Option<String>,
}

pub async fn update_collection(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Json(form): Json<UpdateCollectionForm>,
) -> AppResult<Json<Value>> {
    auth.require_scope("write:collections")?;
    let c = load_collection(&state, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if c.account_id != auth.account_id {
        return Err(AppError::Forbidden);
    }

    if let Some(name) = &form.name {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(AppError::Unprocessable("name can't be blank".into()));
        }
        if trimmed.chars().count() > NAME_MAX {
            return Err(AppError::Unprocessable(format!(
                "name is too long (maximum is {NAME_MAX} characters)"
            )));
        }
    }
    if let Some(desc) = &form.description {
        if desc.chars().count() > DESCRIPTION_MAX {
            return Err(AppError::Unprocessable(format!(
                "description is too long (maximum is {DESCRIPTION_MAX} characters)"
            )));
        }
    }

    let tag_id = match &form.tag_name {
        Some(t) if !t.trim().is_empty() => Some(resolve_tag(&state, t).await?),
        Some(_) => None, // explicit blank clears the tag
        None => None,    // handled by COALESCE below (leave unchanged)
    };
    let clear_tag = matches!(&form.tag_name, Some(t) if t.trim().is_empty());

    sqlx::query!(
        r#"UPDATE collections SET
             name = COALESCE($2, name),
             description = COALESCE($3, description),
             language = COALESCE($4, language),
             sensitive = COALESCE($5, sensitive),
             discoverable = COALESCE($6, discoverable),
             tag_id = CASE WHEN $7 THEN NULL WHEN $8::bigint IS NOT NULL THEN $8 ELSE tag_id END,
             updated_at = now()
           WHERE id = $1"#,
        id,
        form.name.as_deref().map(str::trim),
        form.description.as_deref(),
        form.language.as_deref(),
        form.sensitive,
        form.discoverable,
        clear_tag,
        tag_id,
    )
    .execute(&state.db)
    .await?;

    let previous = c;
    let c = load_collection(&state, id)
        .await?
        .ok_or(AppError::NotFound)?;
    // `NotifyOfCollectionUpdateService#significantly_changed?`.
    if previous.name != c.name
        || previous.description != c.description
        || previous.description_html != c.description_html
        || previous.sensitive != c.sensitive
        || previous.tag_name != c.tag_name
    {
        notify_of_collection_update(&state, id).await;
    }
    let entity = collection_entity(&state, &instance.domain, &c, Some(auth.account_id)).await?;
    distribute_collection(&state, &instance.domain, id, auth.account_id, false).await;
    Ok(Json(json!({ "collection": entity })))
}

/// `NotifyOfCollectionUpdateService`, once the collection has changed in a
/// way its members are told of: a `collection_update` for each local account
/// whose item is accepted, from the collection's owner, replacing the one
/// before.
pub(crate) async fn notify_of_collection_update(state: &AppState, collection_id: i64) {
    let members = sqlx::query!(
        r#"SELECT ci.account_id AS "account_id!", c.account_id AS owner_id
           FROM collection_items ci
           JOIN collections c ON c.id = ci.collection_id
           JOIN accounts a ON a.id = ci.account_id
           WHERE ci.collection_id = $1 AND ci.state = 1 AND a.domain IS NULL
           ORDER BY ci.id"#,
        collection_id,
    )
    .fetch_all(&state.db)
    .await;
    let members = match members {
        Ok(members) => members,
        Err(error) => {
            tracing::warn!(%error, collection_id, "could not find a collection's members");
            return;
        }
    };
    for member in members {
        crate::push::notify_collection(
            state,
            member.account_id,
            "collection_update",
            ("Collection", collection_id),
            member.owner_id,
        )
        .await;
    }
}

/// `has_many :notifications, as: :activity, dependent: :destroy`: what a
/// collection's members were told of it goes with it.
pub(crate) async fn destroy_notifications<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    collection_id: i64,
) -> sqlx::Result<()> {
    sqlx::query!(
        "DELETE FROM notifications WHERE activity_type = 'Collection' AND activity_id = $1",
        collection_id,
    )
    .execute(executor)
    .await?;
    Ok(())
}

// ── DELETE /api/v1/collections/{id} ───────────────────────────────────────

pub async fn delete_collection(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("write:collections")?;
    let c = load_collection(&state, id)
        .await?
        .ok_or(AppError::NotFound)?;
    if c.account_id != auth.account_id {
        return Err(AppError::Forbidden);
    }
    sqlx::query!("DELETE FROM collections WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    destroy_notifications(&state.db, id).await?;
    distribute_collection_removal(&state, &instance.domain, id, auth.account_id).await;
    Ok(Json(json!({})))
}

// ── Collection items ──────────────────────────────────────────────────────

/// Insert (or no-op on conflict) an item adding `account_id` to `collection_id`.
/// Local accounts are auto-accepted; remote accounts start pending.
async fn add_item(state: &AppState, collection_id: i64, account_id: i64) -> AppResult<Value> {
    let target = sqlx::query!(
        r#"SELECT domain, suspended_at, requested_deletion_at, uri, inbox_url, shared_inbox_url
           FROM accounts WHERE id = $1"#,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    if target.suspended_at.is_some() || target.requested_deletion_at.is_some() {
        return Err(AppError::Unprocessable(
            "This account cannot be added to collections".into(),
        ));
    }

    // Enforce MAX_ITEMS pending+accepted.
    let current = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM collection_items WHERE collection_id = $1 AND state IN (0, 1)",
        collection_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(0);
    if current >= MAX_ITEMS {
        return Err(AppError::Unprocessable(format!(
            "Collections cannot have more than {MAX_ITEMS} items"
        )));
    }

    // Local accounts auto-accept; remote accounts start pending and must consent
    // via a FeatureRequest -> Accept/Reject handshake.
    let is_remote = target.domain.is_some();
    let item_state: i32 = if is_remote { 0 } else { 1 };
    let domain = &state.instance.domain;

    let owner = sqlx::query!(
        "SELECT a.id, a.username, a.id_scheme
         FROM collections c JOIN accounts a ON a.id = c.account_id
         WHERE c.id = $1 AND a.domain IS NULL",
        collection_id,
    )
    .fetch_optional(&state.db)
    .await?;

    let activity_uri: Option<String> = if is_remote {
        owner.as_ref().map(|o| {
            format!(
                "https://{domain}/users/{}/feature_requests/{}",
                o.username,
                crate::snowflake::next_id()
            )
        })
    } else {
        None
    };

    let row = sqlx::query!(
        r#"INSERT INTO collection_items
             (collection_id, account_id, state, activity_uri, position, created_at, updated_at)
           VALUES ($1, $2, $3, $4,
                   (SELECT COALESCE(MAX(position), 0) + 1 FROM collection_items WHERE collection_id = $1),
                   now(), now())
           ON CONFLICT (account_id, collection_id) DO NOTHING
           RETURNING id, state, created_at"#,
        collection_id,
        account_id,
        item_state,
        activity_uri.as_deref(),
    )
    .fetch_optional(&state.db)
    .await?;

    let row = match row {
        Some(r) => r,
        None => {
            return Err(AppError::Unprocessable(
                "This account is already in the collection".into(),
            ))
        }
    };

    refresh_item_count(state, collection_id).await?;

    // `notify_local_user` (`AddAccountToCollectionService`, and
    // `CreateCollectionService#notify_local_users` for the first members).
    if !is_remote {
        if let Some(owner) = &owner {
            crate::push::notify_collection(
                state,
                account_id,
                "added_to_collection",
                ("CollectionItem", row.id),
                owner.id,
            )
            .await;
        }
    }

    // Send the FeatureRequest asking the remote account for consent.
    if is_remote {
        if let (Some(owner), Some(activity_uri)) = (owner, activity_uri) {
            if crate::federation::keypair::has_signing_key(state, owner.id)
                .await
                .unwrap_or(false)
            {
                let inbox = if !target.shared_inbox_url.is_empty() {
                    target.shared_inbox_url.clone()
                } else {
                    target.inbox_url.clone()
                };
                let account_uri = target.uri.clone().unwrap_or_default();
                if !inbox.is_empty() && !account_uri.is_empty() {
                    let actor_url = crate::federation::tag::account_uri(
                        domain,
                        owner.id,
                        owner.id_scheme,
                        &owner.username,
                    );
                    let collection_uri = collection_uri(domain, owner.id, collection_id);
                    if let Ok(req) = crate::federation::consent::feature_request(
                        &activity_uri,
                        &actor_url,
                        &account_uri,
                        &collection_uri,
                    ) {
                        let key_id = format!("{actor_url}#main-key");
                        // `CollectionItem#sign?` (`FeatureRequestWorker`).
                        if let Err(e) = crate::federation::delivery::deliver_to_inboxes_signed(
                            state,
                            req,
                            vec![inbox],
                            key_id,
                            crate::federation::delivery::LinkedData::UnlessAuthorizedFetch,
                        )
                        .await
                        {
                            tracing::warn!(error = %e, "failed to enqueue feature request");
                        }
                    }
                }
            }
        }
    }

    Ok(item_entity(
        row.id,
        row.state,
        row.created_at,
        Some(account_id),
    ))
}

// ── ActivityPub distribution to followers (best-effort) ───────────────────────

/// The owner's username, if it's a local account with a usable signing key.
/// (The key itself is loaded from the account at delivery time.)
async fn owner_signing_username(state: &AppState, owner_account_id: i64) -> Option<String> {
    let row = sqlx::query!(
        "SELECT username FROM accounts WHERE id = $1 AND domain IS NULL",
        owner_account_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()??;
    crate::federation::keypair::has_signing_key(state, owner_account_id)
        .await
        .ok()?
        .then_some(row.username)
}

/// Distribute an `Add`/`Update(FeaturedCollection)` to the owner's followers.
pub(crate) async fn distribute_collection(
    state: &AppState,
    domain: &str,
    collection_id: i64,
    owner_account_id: i64,
    is_create: bool,
) {
    let Some(ap) = ap_coll::load_ap_collection(state, collection_id)
        .await
        .ok()
        .flatten()
    else {
        return;
    };
    let Ok(body) = ap_coll::featured_collection_body(state, domain, &ap).await else {
        return;
    };
    if owner_signing_username(state, owner_account_id)
        .await
        .is_none()
    {
        return;
    }
    let key_id = format!("{}#main-key", ap.actor_uri(domain));
    let activity = if is_create {
        ap_coll::add_collection_activity(domain, &ap, body)
    } else {
        ap_coll::update_collection_activity(domain, &ap, body)
    };
    if let Err(e) =
        crate::federation::delivery::fanout_to_followers(state, activity, owner_account_id, key_id)
            .await
    {
        tracing::warn!(error = %e, "failed to enqueue collection fanout");
    }
}

/// Distribute a `Remove` (collection deleted) to the owner's followers.
pub(crate) async fn distribute_collection_removal(
    state: &AppState,
    domain: &str,
    collection_id: i64,
    owner_account_id: i64,
) {
    if owner_signing_username(state, owner_account_id)
        .await
        .is_none()
    {
        return;
    }
    let Ok(Some(owner)) = sqlx::query!(
        "SELECT id_scheme, username FROM accounts WHERE id = $1",
        owner_account_id,
    )
    .fetch_optional(&state.db)
    .await
    else {
        return;
    };
    let actor = crate::federation::tag::account_uri(
        domain,
        owner_account_id,
        owner.id_scheme,
        &owner.username,
    );
    let key_id = format!("{actor}#main-key");
    let activity =
        ap_coll::remove_collection_activity(domain, &actor, owner_account_id, collection_id);
    if let Err(e) =
        crate::federation::delivery::fanout_to_followers(state, activity, owner_account_id, key_id)
            .await
    {
        tracing::warn!(error = %e, "failed to enqueue collection removal fanout");
    }
}

async fn refresh_item_count(state: &AppState, collection_id: i64) -> AppResult<()> {
    sqlx::query!(
        r#"UPDATE collections SET item_count =
             (SELECT COUNT(*) FROM collection_items WHERE collection_id = $1 AND state IN (0, 1))
           WHERE id = $1"#,
        collection_id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct AddItemForm {
    pub account_id: Option<String>,
}

/// POST /api/v1/collections/{id}/items
pub async fn add_collection_item(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(collection_id): Path<i64>,
    Json(form): Json<AddItemForm>,
) -> AppResult<Json<Value>> {
    auth.require_scope("write:collections")?;
    let c = load_collection(&state, collection_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if c.account_id != auth.account_id {
        return Err(AppError::Forbidden);
    }

    let account_id = form
        .account_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| AppError::Unprocessable("`account_id` parameter is missing".into()))?;

    let item = add_item(&state, collection_id, account_id).await?;
    distribute_collection(
        &state,
        &instance.domain,
        collection_id,
        auth.account_id,
        false,
    )
    .await;
    Ok(Json(json!({ "collection_item": item })))
}

/// DELETE /api/v1/collections/{id}/items/{item_id}
pub async fn delete_collection_item(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path((collection_id, item_id)): Path<(i64, i64)>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("write:collections")?;
    let c = load_collection(&state, collection_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if c.account_id != auth.account_id {
        return Err(AppError::Forbidden);
    }
    let deleted = sqlx::query!(
        "DELETE FROM collection_items WHERE id = $1 AND collection_id = $2",
        item_id,
        collection_id,
    )
    .execute(&state.db)
    .await?;
    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    refresh_item_count(&state, collection_id).await?;
    distribute_collection(
        &state,
        &instance.domain,
        collection_id,
        auth.account_id,
        false,
    )
    .await;
    Ok(Json(json!({})))
}

/// POST /api/v1/collections/{id}/items/{item_id}/revoke
pub async fn revoke_collection_item(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path((collection_id, item_id)): Path<(i64, i64)>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("write:collections")?;
    let c = load_collection(&state, collection_id)
        .await?
        .ok_or(AppError::NotFound)?;
    if c.account_id != auth.account_id {
        return Err(AppError::Forbidden);
    }
    let updated = sqlx::query!(
        "UPDATE collection_items SET state = 3, updated_at = now() WHERE id = $1 AND collection_id = $2",
        item_id,
        collection_id,
    )
    .execute(&state.db)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    refresh_item_count(&state, collection_id).await?;
    distribute_collection(
        &state,
        &instance.domain,
        collection_id,
        auth.account_id,
        false,
    )
    .await;
    Ok(Json(json!({})))
}
