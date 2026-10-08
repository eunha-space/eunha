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

/// `SoftwareVersionsDimension`: eunha's version as the instance API gives
/// it, PostgreSQL's, the Redis-compatible store's, the search cluster's when
/// search is on and answers, and FFmpeg's when `ffprobe` runs. Eunha has no
/// Ruby and no libvips to report.
pub(super) async fn software_versions(state: &AppState) -> AppResult<Vec<serde_json::Value>> {
    let version = |key: &str, human_key: &str, value: Option<String>| serde_json::json!({ "key": key, "human_key": human_key, "value": value, "human_value": value });
    let mut rows = vec![version(
        "mastodon",
        "Mastodon",
        Some(crate::version::compatible_string()),
    )];

    let pg_version: String = sqlx::query_scalar!("SELECT version()")
        .fetch_one(&state.db)
        .await?
        .unwrap_or_default();
    rows.push(version(
        "postgresql",
        "PostgreSQL",
        Some(postgresql_version(&pg_version)),
    ));

    let store = redis_info(state).await;
    rows.push(version("redis", store.name(), store.version()));

    if let Some((distribution, number)) = search_version(state).await {
        rows.push(version("elasticsearch", distribution, Some(number)));
    }
    if let Some(ffmpeg) = ffmpeg_version().await {
        rows.push(version("ffmpeg", "FFmpeg", Some(ffmpeg)));
    }
    Ok(rows)
}

/// `VERSION().match(/\A(?:PostgreSQL |)([^\s]+).*\z/)[1]`.
fn postgresql_version(version: &str) -> String {
    let rest = version.strip_prefix("PostgreSQL ").unwrap_or(version);
    rest.split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// What `StoreHelper` reads of `redis.info`, when `INFO` is allowed.
struct RedisInfo(Option<String>);

impl RedisInfo {
    fn field(&self, field: &str) -> Option<String> {
        self.0
            .as_deref()
            .and_then(|info| parse_redis_info_field(info, &format!("{field}:")))
    }

    /// `store_name`.
    fn name(&self) -> &'static str {
        if self.field("valkey_version").is_some() {
            "Valkey"
        } else if self.field("dragonfly_version").is_some() {
            "Dragonfly"
        } else {
            "Redis"
        }
    }

    /// `store_version`.
    fn version(&self) -> Option<String> {
        self.field("valkey_version")
            .or_else(|| self.field("dragonfly_version"))
            .or_else(|| self.field("redis_version"))
    }
}

async fn redis_info(state: &AppState) -> RedisInfo {
    let mut redis = state.redis.clone();
    RedisInfo(redis::cmd("INFO").query_async(&mut redis).await.ok())
}

/// `elasticsearch_version`: the cluster's distribution and version, when
/// search is on and the cluster answers.
async fn search_version(state: &AppState) -> Option<(&'static str, String)> {
    let client = state.search.as_ref()?;
    let info = client.send(reqwest::Method::GET, "/", None).await.ok()?;
    let number = info["version"]["number"].as_str()?.to_owned();
    let distribution = if info["version"]["distribution"] == "opensearch" {
        "OpenSearch"
    } else {
        "Elasticsearch"
    };
    Some((distribution, number))
}

/// `ffmpeg_version`: `ffprobe -show_program_version -v 0 -of json`.
async fn ffmpeg_version() -> Option<String> {
    let output = tokio::process::Command::new("ffprobe")
        .args(["-show_program_version", "-v", "0", "-of", "json"])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    json["program_version"]["version"]
        .as_str()
        .map(str::to_owned)
}

/// `SpaceUsageDimension`: the database, the Redis-compatible store (only
/// when it is this instance's alone), the media, and the search indexes
/// when search is on and answers.
pub(super) async fn space_usage(
    state: &AppState,
    locale: crate::locale::Locale,
) -> AppResult<Vec<serde_json::Value>> {
    let human = metrics::number_to_human_size;
    let size = |key: &str, human_key: &str, value: i64| {
        serde_json::json!({
            "key": key, "human_key": human_key,
            "value": value.to_string(), "unit": "bytes",
            "human_value": human(value),
        })
    };

    let pg_size: i64 = sqlx::query_scalar!("SELECT pg_database_size(current_database())")
        .fetch_one(&state.db)
        .await?
        .unwrap_or(0);
    let mut rows = vec![size("postgresql", "PostgreSQL", pg_size)];

    let store = redis_info(state).await;
    let used_memory = (state.config.redis_process_metrics
        && !state.redis_keys.is_shared()
        && state.config.redis_coordination_url.is_none())
    .then(|| store.field("used_memory"))
    .flatten()
    .and_then(|v| v.parse::<i64>().ok());
    rows.push(match used_memory {
        Some(bytes) => size("redis", store.name(), bytes),
        None => serde_json::json!({
            "key": "redis", "human_key": store.name(),
            "value": null, "unit": "bytes",
            "human_value": "Unavailable for shared Redis",
        }),
    });

    // `MediaAttachment.combined_media_file_size`, custom emoji, preview
    // cards, avatars and headers, archive takeouts and site uploads.
    let media_size: i64 = sqlx::query_scalar!(
        r#"SELECT (
             COALESCE((SELECT SUM(COALESCE(file_file_size, 0) + COALESCE(thumbnail_file_size, 0)) FROM media_attachments), 0)
           + COALESCE((SELECT SUM(image_file_size) FROM custom_emojis), 0)
           + COALESCE((SELECT SUM(image_file_size) FROM preview_cards), 0)
           + COALESCE((SELECT SUM(COALESCE(avatar_file_size, 0) + COALESCE(header_file_size, 0)) FROM accounts), 0)
           + COALESCE((SELECT SUM(dump_file_size) FROM backups), 0)
           + COALESCE((SELECT SUM(file_file_size) FROM site_uploads), 0)
           )::bigint AS "size!""#
    )
    .fetch_one(&state.db)
    .await?;
    rows.push(size(
        "media",
        locale.t("admin.dashboard.media_storage"),
        media_size,
    ));

    if let Some((distribution, bytes)) = search_size(state).await {
        rows.push(size("search", distribution, bytes));
    }
    Ok(rows)
}

/// `search_size`: the primaries' store size of the instance's indexes (all
/// of the cluster's without an index prefix).
async fn search_size(state: &AppState) -> Option<(&'static str, i64)> {
    let client = state.search.as_ref()?;
    let (distribution, _) = search_version(state).await?;
    let pattern = client.index_name("*");
    let stats = client
        .send(reqwest::Method::GET, &format!("{pattern}/_stats"), None)
        .await
        .ok()?;
    let bytes = stats["indices"]
        .as_object()?
        .values()
        .filter_map(|index| index["primaries"]["store"]["size_in_bytes"].as_i64())
        .sum();
    Some((distribution, bytes))
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

#[cfg(test)]
mod software_version_tests {
    #[test]
    fn postgresql_versions_are_read_as_mastodon_reads_them() {
        assert_eq!(
            super::postgresql_version("PostgreSQL 18.0 on aarch64-apple-darwin, compiled by clang"),
            "18.0"
        );
        assert_eq!(super::postgresql_version("16.4 (Debian 16.4-1)"), "16.4");
    }
}
