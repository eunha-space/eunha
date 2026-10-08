use crate::{
    error::{AppError, AppResult},
    middleware::{AuthenticatedUser, ResolvedInstance},
    state::AppState,
};
use axum::{
    extract::{Extension, Multipart, Path, Query},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

mod accounts;
mod blocks;
mod email_subscriptions;
mod federation;
mod reports;
mod terms_of_service;
mod trends;
// The moderation tools Mastodon has only as server-rendered admin pages.
mod action_logs;
mod appeals;
mod notes;
mod relationships;
mod statuses;
mod username_blocks;
mod users;
mod warning_presets;
// The server administration Mastodon has only as server-rendered admin pages.
pub(crate) mod announcements;
mod dashboard;
mod fasp;
mod follow_recommendations;
pub(crate) mod instances;
mod invites;
mod relays;
mod roles;
mod rules;
mod settings;
mod software_updates;
mod webhooks;

pub use accounts::*;
pub use blocks::*;
pub use email_subscriptions::*;
pub use federation::*;
pub use reports::*;
pub use terms_of_service::*;
pub use trends::*;
// The moderation tools Mastodon has only as server-rendered admin pages.
pub use action_logs::*;
pub use appeals::*;
pub use notes::*;
pub use relationships::*;
pub use statuses::*;
pub use username_blocks::*;
pub use users::*;
pub use warning_presets::*;
// The server administration Mastodon has only as server-rendered admin pages.
pub use announcements::*;
pub use dashboard::*;
pub use fasp::*;
pub use follow_recommendations::*;
pub use instances::*;
pub use invites::*;
pub use relays::*;
pub use roles::*;
pub use rules::*;
pub use settings::*;
pub use software_updates::*;
pub use webhooks::*;

/// `REST::AccountSerializer` of an account, if it exists.
pub(super) async fn api_account(
    state: &AppState,
    id: i64,
) -> AppResult<Option<super::types::Account>> {
    match sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?
    {
        Some(account) => Ok(Some(super::accounts::account_to_api(state, &account).await)),
        None => Ok(None),
    }
}

/// `limit` and the id bounds of `to_a_paginated_by_id`.
#[derive(Debug, Deserialize)]
pub struct PageParams {
    pub limit: Option<super::extractors::FlexId>,
    pub max_id: Option<super::extractors::FlexId>,
    pub since_id: Option<super::extractors::FlexId>,
    pub min_id: Option<super::extractors::FlexId>,
}

impl PageParams {
    /// `limit_param(default, max)`.
    pub(super) fn limit(&self, default: i64, max: i64) -> i64 {
        self.limit.map_or(default, |l| l.0.abs().min(max))
    }
}

// ── Admin auth guard ──────────────────────────────────────────────────────

pub use crate::moderation::role::flag as perm;

/// Mastodon's `UserRole#computed_permissions` of an account's role, as
/// `(position, permissions)`; see [`crate::moderation::role`].
pub(super) async fn computed_permissions(
    state: &AppState,
    account_id: i64,
) -> AppResult<(i32, i64)> {
    let role = crate::moderation::role::of_account(&state.db, account_id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    Ok((role.position, role.computed))
}

/// `authorize` against a policy that is a single `role.can?(flag)`, judged by
/// the acting role (nobody's when the user is disabled).
pub(crate) async fn require_permission(
    state: &AppState,
    account_id: i64,
    flag: i64,
) -> AppResult<()> {
    let role = crate::moderation::role::acting(&state.db, account_id).await?;
    crate::moderation::role::authorize(role.can(&[flag]))
}

mod metrics;
pub use metrics::*;

/// `SoftwareVersionsDimension`, in eunha's terms.
pub(super) async fn software_versions(state: &AppState) -> AppResult<Vec<serde_json::Value>> {
    let pg_version_raw: String = sqlx::query_scalar!("SELECT version()")
        .fetch_one(&state.db)
        .await?
        .unwrap_or_default();
    let pg_version = pg_version_raw
        .split_whitespace()
        .nth(1)
        .unwrap_or("unknown")
        .to_string();

    let mut redis = state.redis.clone();
    let redis_info: String = redis::cmd("INFO")
        .arg("server")
        .query_async(&mut redis)
        .await
        .unwrap_or_default();
    let redis_version = parse_redis_info_field(&redis_info, "redis_version")
        .unwrap_or_else(|| "unknown".to_string());

    let eunha_version = crate::version::EUNHA_FULL.to_string();

    Ok(vec![
        serde_json::json!({"key": "mastodon", "human_key": "Eunha", "value": eunha_version.clone(), "human_value": eunha_version}),
        serde_json::json!({"key": "postgresql", "human_key": "PostgreSQL", "value": pg_version.clone(), "human_value": pg_version}),
        serde_json::json!({"key": "redis", "human_key": "Redis", "value": redis_version.clone(), "human_value": redis_version}),
    ])
}

/// `SpaceUsageDimension`, in eunha's terms.
pub(super) async fn space_usage(state: &AppState) -> AppResult<Vec<serde_json::Value>> {
    let pg_size: i64 = sqlx::query_scalar!("SELECT pg_database_size(current_database())")
        .fetch_one(&state.db)
        .await?
        .unwrap_or(0);

    let redis_size = if state.config.redis_process_metrics
        && !state.redis_keys.is_shared()
        && state.config.redis_coordination_url.is_none()
    {
        let mut redis = state.redis.clone();
        let redis_mem_info: String = redis::cmd("INFO")
            .arg("memory")
            .query_async(&mut redis)
            .await
            .unwrap_or_default();
        parse_redis_info_field(&redis_mem_info, "used_memory").and_then(|v| v.parse::<i64>().ok())
    } else {
        None
    };

    let media_size: i64 = sqlx::query_scalar!(
        r#"SELECT
           COALESCE((SELECT SUM(COALESCE(file_file_size,0) + COALESCE(thumbnail_file_size,0)) FROM media_attachments), 0)
           + COALESCE((SELECT SUM(COALESCE(image_file_size,0)) FROM custom_emojis), 0)
           + COALESCE((SELECT SUM(COALESCE(image_file_size,0)) FROM preview_cards), 0)
           + COALESCE((SELECT SUM(COALESCE(avatar_file_size,0) + COALESCE(header_file_size,0)) FROM accounts), 0)"#
    ).fetch_one(&state.db).await?.unwrap_or(0);

    let human = metrics::number_to_human_size;
    Ok(vec![
        serde_json::json!({
            "key": "postgresql", "human_key": "PostgreSQL",
            "value": pg_size.to_string(), "unit": "bytes",
            "human_value": human(pg_size),
        }),
        redis_size
            .map(|bytes| {
                serde_json::json!({
                    "key": "redis", "human_key": "Redis",
                    "value": bytes.to_string(), "unit": "bytes",
                    "human_value": human(bytes),
                })
            })
            .unwrap_or_else(|| {
                serde_json::json!({
                    "key": "redis", "human_key": "Redis",
                    "value": null, "unit": "bytes",
                    "human_value": "Unavailable for shared Redis",
                })
            }),
        serde_json::json!({
            "key": "media", "human_key": "Media storage",
            "value": media_size.to_string(), "unit": "bytes",
            "human_value": human(media_size),
        }),
    ])
}

fn parse_redis_info_field(info: &str, field: &str) -> Option<String> {
    info.lines()
        .find(|l| l.starts_with(field))
        .and_then(|l| l.split_once(':').map(|x| x.1))
        .map(|v| v.trim().to_string())
}

// ── Admin CustomEmoji type ────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AdminCustomEmoji {
    pub id: String,
    pub shortcode: String,
    pub url: String,
    pub static_url: String,
    pub visible_in_picker: bool,
    pub disabled: bool,
    pub category: Option<String>,
}

// ── GET /api/v1/admin/custom_emojis ──────────────────────────────────────

pub async fn list_admin_custom_emojis(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminCustomEmoji>>> {
    require_permission(&state, auth.account_id, perm::MANAGE_CUSTOM_EMOJIS).await?;

    let rows = sqlx::query!(
        "SELECT id, shortcode, image_remote_url, visible_in_picker, disabled
         FROM custom_emojis WHERE domain IS NULL ORDER BY shortcode",
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| {
                let url = r.image_remote_url.unwrap_or_default();
                AdminCustomEmoji {
                    id: r.id.to_string(),
                    shortcode: r.shortcode,
                    url: url.clone(),
                    static_url: url,
                    visible_in_picker: r.visible_in_picker,
                    disabled: r.disabled,
                    category: None,
                }
            })
            .collect(),
    ))
}

// ── POST /api/v1/admin/custom_emojis ─────────────────────────────────────

pub async fn create_admin_custom_emoji(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    mut multipart: Multipart,
) -> AppResult<Json<AdminCustomEmoji>> {
    require_permission(&state, auth.account_id, perm::MANAGE_CUSTOM_EMOJIS).await?;

    let mut shortcode = String::new();
    let mut image_bytes: Option<Vec<u8>> = None;
    let mut content_type = "image/png".to_string();

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "shortcode" => {
                shortcode = field.text().await.unwrap_or_default();
            }
            "image" => {
                content_type = field.content_type().unwrap_or("image/png").to_string();
                image_bytes = field.bytes().await.ok().map(|b| b.to_vec());
            }
            _ => {}
        }
    }

    if shortcode.is_empty() {
        return Err(AppError::Unprocessable("shortcode is required".into()));
    }
    let image_data =
        image_bytes.ok_or_else(|| AppError::Unprocessable("image is required".into()))?;

    // Upload to storage
    let ext = match content_type.as_str() {
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    };
    let key = format!("emoji/{}.{}", shortcode, ext);
    state
        .storage
        .store(&image_data, &key, &content_type)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("storage: {e}")))?;
    let url = state.storage.public_url(&key);

    let row = if let Some(row) = sqlx::query!(
        r#"UPDATE custom_emojis
           SET image_remote_url = $2, disabled = false, visible_in_picker = true, updated_at = now()
           WHERE shortcode = $1 AND domain IS NULL
           RETURNING id, shortcode, image_remote_url, visible_in_picker, disabled"#,
        shortcode,
        url,
    )
    .fetch_optional(&state.db)
    .await?
    {
        (
            row.id,
            row.shortcode,
            row.image_remote_url,
            row.visible_in_picker,
            row.disabled,
        )
    } else {
        let row = sqlx::query!(
            r#"INSERT INTO custom_emojis (shortcode, image_remote_url, visible_in_picker, created_at, updated_at)
               VALUES ($1, $2, true, now(), now())
               RETURNING id, shortcode, image_remote_url, visible_in_picker, disabled"#,
            shortcode, url,
        )
        .fetch_one(&state.db)
        .await?;
        (
            row.id,
            row.shortcode,
            row.image_remote_url,
            row.visible_in_picker,
            row.disabled,
        )
    };

    let url = row.2.unwrap_or_default();
    Ok(Json(AdminCustomEmoji {
        id: row.0.to_string(),
        shortcode: row.1,
        url: url.clone(),
        static_url: url,
        visible_in_picker: row.3,
        disabled: row.4,
        category: None,
    }))
}

// ── DELETE /api/v1/admin/custom_emojis/:id ───────────────────────────────

pub async fn delete_admin_custom_emoji(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_permission(&state, auth.account_id, perm::MANAGE_CUSTOM_EMOJIS).await?;
    sqlx::query!("DELETE FROM custom_emojis WHERE id = $1", id,)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::OK)
}

// ── PATCH /api/v1/admin/custom_emojis/:id ────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct PatchEmojiForm {
    pub shortcode: Option<String>,
    pub visible_in_picker: Option<bool>,
    pub disabled: Option<bool>,
}

pub async fn update_admin_custom_emoji(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Json(form): Json<PatchEmojiForm>,
) -> AppResult<Json<AdminCustomEmoji>> {
    require_permission(&state, auth.account_id, perm::MANAGE_CUSTOM_EMOJIS).await?;
    if let Some(sc) = &form.shortcode {
        sqlx::query!(
            "UPDATE custom_emojis SET shortcode = $1 WHERE id = $2",
            sc,
            id
        )
        .execute(&state.db)
        .await?;
    }
    if let Some(v) = form.visible_in_picker {
        sqlx::query!(
            "UPDATE custom_emojis SET visible_in_picker = $1 WHERE id = $2",
            v,
            id
        )
        .execute(&state.db)
        .await?;
    }
    if let Some(d) = form.disabled {
        sqlx::query!(
            "UPDATE custom_emojis SET disabled = $1 WHERE id = $2",
            d,
            id
        )
        .execute(&state.db)
        .await?;
    }
    let row = sqlx::query!(
        "SELECT id, shortcode, image_remote_url, visible_in_picker, disabled FROM custom_emojis WHERE id = $1",
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let url = row.image_remote_url.unwrap_or_default();
    Ok(Json(AdminCustomEmoji {
        id: row.id.to_string(),
        shortcode: row.shortcode,
        url: url.clone(),
        static_url: url,
        visible_in_picker: row.visible_in_picker,
        disabled: row.disabled,
        category: None,
    }))
}

// ── Admin Tags ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AdminTag {
    pub id: String,
    pub name: String,
    pub url: String,
    /// `REST::TagSerializer#history`, which the admin serializer extends.
    pub history: Vec<super::types::TagHistory>,
    pub trendable: bool,
    pub usable: bool,
    pub requires_review: bool,
    pub listable: bool,
}

fn admin_tag_url(domain: &str, name: &str) -> String {
    format!("https://{domain}/tags/{name}")
}

#[derive(Debug, Deserialize)]
pub struct UpdateAdminTagForm {
    pub trendable: Option<bool>,
    pub usable: Option<bool>,
    pub listable: Option<bool>,
}

#[derive(serde::Deserialize)]
pub struct AdminTagsParams {
    #[serde(flatten)]
    pub pagination: super::types::PaginationParams,
    pub name: Option<String>,
}

pub async fn list_admin_tags(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Query(params): Query<AdminTagsParams>,
) -> AppResult<Json<Vec<AdminTag>>> {
    require_permission(&state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let trendable_by_default = crate::settings::boolean(&state, "trendable_by_default").await;
    let domain = &instance.domain;
    let limit = params.pagination.limit_clamped(100, 100);
    let max_id = params
        .pagination
        .max_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let since_id = params
        .pagination
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let min_id = params
        .pagination
        .min_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let name_filter = params.name.as_deref().map(|s| s.to_lowercase());

    let rows = sqlx::query!(
        r#"SELECT id, name, trendable, usable, listable, reviewed_at
           FROM tags
           WHERE ($2::bigint IS NULL OR id < $2)
             AND ($3::bigint IS NULL OR id > $3)
             AND ($4::bigint IS NULL OR id > $4)
             AND ($5::text IS NULL OR name = $5)
           ORDER BY id DESC
           LIMIT $1"#,
        limit,
        max_id,
        since_id,
        min_id,
        name_filter,
    )
    .fetch_all(&state.db)
    .await?;
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let mut histories = super::tags::fetch_tags_histories(&state, &ids).await;

    Ok(Json(
        rows.into_iter()
            .map(|r| AdminTag {
                id: r.id.to_string(),
                history: histories.remove(&r.id).unwrap_or_default(),
                name: r.name.clone(),
                url: admin_tag_url(domain, &r.name),
                // `Tag#trendable`: the column, else `trendable_by_default`.
                trendable: r.trendable.unwrap_or(trendable_by_default),
                usable: r.usable.unwrap_or(true),
                listable: r.listable.unwrap_or(true),
                requires_review: r.reviewed_at.is_none(),
            })
            .collect(),
    ))
}

pub async fn get_admin_tag(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminTag>> {
    require_permission(&state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let trendable_by_default = crate::settings::boolean(&state, "trendable_by_default").await;
    let domain = &instance.domain;
    let r = sqlx::query!(
        "SELECT id, name, trendable, usable, listable, reviewed_at FROM tags WHERE id = $1",
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(AdminTag {
        id: r.id.to_string(),
        history: super::tags::fetch_tags_histories(&state, &[r.id])
            .await
            .remove(&r.id)
            .unwrap_or_default(),
        name: r.name.clone(),
        url: admin_tag_url(domain, &r.name),
        trendable: r.trendable.unwrap_or(trendable_by_default),
        usable: r.usable.unwrap_or(true),
        listable: r.listable.unwrap_or(true),
        requires_review: r.reviewed_at.is_none(),
    }))
}

pub async fn update_admin_tag(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Path(id): Path<i64>,
    Json(form): Json<UpdateAdminTagForm>,
) -> AppResult<Json<AdminTag>> {
    require_permission(&state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let trendable_by_default = crate::settings::boolean(&state, "trendable_by_default").await;
    let domain = &instance.domain;
    let r = sqlx::query!(
        r#"UPDATE tags SET
               trendable   = COALESCE($2, trendable),
               usable      = COALESCE($3, usable),
               listable    = COALESCE($4, listable),
               reviewed_at = now(),
               updated_at  = now()
           WHERE id = $1
           RETURNING id, name, trendable, usable, listable, reviewed_at"#,
        id,
        form.trendable,
        form.usable,
        form.listable,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    // `Tag`'s `update_index('tags', :self)`.
    crate::search::elasticsearch::indexing::tags(&state, &[r.id]).await;
    Ok(Json(AdminTag {
        id: r.id.to_string(),
        history: super::tags::fetch_tags_histories(&state, &[r.id])
            .await
            .remove(&r.id)
            .unwrap_or_default(),
        name: r.name.clone(),
        url: admin_tag_url(domain, &r.name),
        trendable: r.trendable.unwrap_or(trendable_by_default),
        usable: r.usable.unwrap_or(true),
        listable: r.listable.unwrap_or(true),
        requires_review: r.reviewed_at.is_none(),
    }))
}

pub(super) fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}
