//! `Api::V1::Admin::Trends::{Tags,Statuses,Links}Controller` and
//! `Links::PreviewCardProvidersController`: trends as moderators review them.

use axum::{
    extract::{Extension, Path},
    http::{HeaderMap, Uri},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use super::super::extractors::Params;
use super::super::trends::TrendParams;
use super::{perm, PageParams};
use crate::{
    error::{AppError, AppResult},
    middleware::{AuthenticatedUser, ResolvedInstance},
    moderation::role,
    state::AppState,
};

/// `current_user&.can?(:manage_taxonomies)`: staff see every trending item,
/// awaiting review or not, and what a reviewer needs; anyone else gets the
/// public list.
async fn reviewer(state: &AppState, auth: &AuthenticatedUser) -> AppResult<bool> {
    Ok(role::acting(&state.db, auth.account_id)
        .await?
        .can(&[perm::MANAGE_TAXONOMIES]))
}

fn merge(entity: impl serde::Serialize, extra: Value) -> Value {
    let mut value = serde_json::to_value(entity).unwrap_or(Value::Null);
    if let (Value::Object(map), Value::Object(extra)) = (&mut value, extra) {
        map.extend(extra);
    }
    value
}

// ── GET /api/v1/admin/trends/tags ─────────────────────────────────────────

pub async fn admin_trending_tags(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    axum::extract::Query(params): axum::extract::Query<TrendParams>,
    headers: axum::http::HeaderMap,
) -> AppResult<Json<Vec<Value>>> {
    auth.require_scope("admin:read")?;
    let staff = reviewer(&state, &auth).await?;
    if !staff && !crate::settings::boolean(&state, "trends").await {
        return Ok(Json(vec![]));
    }
    // Staff get the whole `Trends::Query`; anyone else the public one,
    // in their languages first.
    let languages = if staff {
        vec![]
    } else {
        crate::trends::preferred_languages(&state, Some(auth.account_id), &headers).await
    };
    let limit = params.limit.unwrap_or(10).clamp(1, 20);
    let offset = params.offset.unwrap_or(0).max(0);
    let rows = super::super::trends::tags_query(
        &state,
        &instance.domain,
        limit,
        offset,
        Some(auth.account_id),
        staff,
        &languages,
    )
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(tag, review)| {
                if staff {
                    // `REST::Admin::TagSerializer`.
                    merge(
                        tag,
                        json!({
                            "trendable": review.trendable,
                            "usable": review.usable,
                            "requires_review": review.requires_review,
                            "listable": review.listable,
                        }),
                    )
                } else {
                    merge(tag, json!({}))
                }
            })
            .collect(),
    ))
}

/// The `REST::Admin::TagSerializer` of one tag.
async fn admin_tag(state: &AppState, domain: &str, id: i64) -> AppResult<Value> {
    let trendable_by_default = crate::settings::boolean(state, "trendable_by_default").await;
    let r = sqlx::query!(
        r#"SELECT id, name, COALESCE(trendable, $2) AS "trendable!", COALESCE(usable, true) AS "usable!",
                  COALESCE(listable, true) AS "listable!", (reviewed_at IS NULL) AS "requires_review!"
           FROM tags WHERE id = $1"#,
        id,
        trendable_by_default,
    )
    .fetch_one(&state.db)
    .await?;
    let history = super::super::tags::fetch_tags_histories(state, &[id])
        .await
        .remove(&id)
        .unwrap_or_default();
    Ok(json!({
        "id": r.id.to_string(),
        "name": r.name,
        "url": format!("https://{domain}/tags/{}", urlencoding::encode(&r.name.to_lowercase())),
        "history": history,
        "trendable": r.trendable,
        "usable": r.usable,
        "requires_review": r.requires_review,
        "listable": r.listable,
    }))
}

async fn review_tag(
    state: &AppState,
    domain: &str,
    auth: &AuthenticatedUser,
    id: i64,
    trendable: bool,
) -> AppResult<Json<Value>> {
    auth.require_scope("admin:write")?;
    // `authorize :tag, :review?`
    super::require_permission(state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let updated = sqlx::query!(
        "UPDATE tags SET trendable = $2, reviewed_at = now(), updated_at = now() WHERE id = $1",
        id,
        trendable
    )
    .execute(&state.db)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(Json(admin_tag(state, domain, id).await?))
}

pub async fn admin_approve_trending_tag(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    review_tag(&state, &instance.domain, &auth, id, true).await
}

pub async fn admin_reject_trending_tag(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    review_tag(&state, &instance.domain, &auth, id, false).await
}

// ── GET /api/v1/admin/trends/statuses ─────────────────────────────────────

pub async fn admin_trending_statuses(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    axum::extract::Query(params): axum::extract::Query<TrendParams>,
    headers: axum::http::HeaderMap,
) -> AppResult<Json<Vec<Value>>> {
    auth.require_scope("admin:read")?;
    let staff = reviewer(&state, &auth).await?;
    if !staff && !crate::settings::boolean(&state, "trends").await {
        return Ok(Json(vec![]));
    }
    // Staff get the whole `Trends::Query`; anyone else the public one,
    // in their languages first.
    let languages = if staff {
        vec![]
    } else {
        crate::trends::preferred_languages(&state, Some(auth.account_id), &headers).await
    };
    let limit = params.limit.unwrap_or(20).clamp(1, 40);
    let offset = params.offset.unwrap_or(0).max(0);
    let rows = super::super::trends::statuses_query(
        &state,
        limit,
        offset,
        Some(auth.account_id),
        staff,
        &languages,
    )
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(status, pending)| {
                if staff {
                    // `REST::Admin::Trends::StatusSerializer`.
                    merge(status, json!({ "requires_review": pending }))
                } else {
                    merge(status, json!({}))
                }
            })
            .collect(),
    ))
}

async fn review_status(
    state: &AppState,
    auth: &AuthenticatedUser,
    id: i64,
    trendable: bool,
) -> AppResult<Json<Value>> {
    auth.require_scope("admin:write")?;
    // `authorize [:admin, :status], :review?` (`manage_taxonomies`).
    super::require_permission(state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let updated = sqlx::query!(
        "UPDATE statuses SET trendable = $2, updated_at = now() WHERE id = $1",
        id,
        trendable
    )
    .execute(&state.db)
    .await?;
    if updated.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    let s = sqlx::query_as!(
        crate::db::models::Status,
        "SELECT * FROM statuses WHERE id = $1",
        id
    )
    .fetch_one(&state.db)
    .await?;
    let account = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        s.account_id
    )
    .fetch_one(&state.db)
    .await?;
    let media = super::super::status_serialize::fetch_status_media(state, s.id).await?;
    let reblog = super::super::status_serialize::fetch_reblog_data(state, &s).await?;
    let status =
        super::super::status_serialize::build_status(state, &s, &account, media, reblog, None)
            .await?;
    let pending = s.trendable.is_none() && account.reviewed_at.is_none();
    Ok(Json(merge(status, json!({ "requires_review": pending }))))
}

pub async fn admin_approve_trending_status(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    review_status(&state, &auth, id, true).await
}

pub async fn admin_reject_trending_status(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    review_status(&state, &auth, id, false).await
}

// ── GET /api/v1/admin/trends/links ────────────────────────────────────────

pub async fn admin_trending_links(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    axum::extract::Query(params): axum::extract::Query<TrendParams>,
    headers: axum::http::HeaderMap,
) -> AppResult<Json<Vec<Value>>> {
    auth.require_scope("admin:read")?;
    let staff = reviewer(&state, &auth).await?;
    if !staff && !crate::settings::boolean(&state, "trends").await {
        return Ok(Json(vec![]));
    }
    // Staff get the whole `Trends::Query`; anyone else the public one,
    // in their languages first.
    let languages = if staff {
        vec![]
    } else {
        crate::trends::preferred_languages(&state, Some(auth.account_id), &headers).await
    };
    let limit = params.limit.unwrap_or(10).clamp(1, 40);
    let offset = params.offset.unwrap_or(0).max(0);
    let rows = super::super::trends::links_query(&state, limit, offset, staff, &languages).await?;
    Ok(Json(
        rows.into_iter()
            .map(|(card, id, pending)| {
                if staff {
                    // `REST::Admin::Trends::LinkSerializer`.
                    merge(
                        card,
                        json!({ "id": id.to_string(), "requires_review": pending }),
                    )
                } else {
                    merge(card, json!({}))
                }
            })
            .collect(),
    ))
}

async fn review_link(
    state: &AppState,
    auth: &AuthenticatedUser,
    id: i64,
    trendable: bool,
) -> AppResult<Json<Value>> {
    auth.require_scope("admin:write")?;
    // `authorize :preview_card, :review?`
    super::require_permission(state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let card = sqlx::query!(
        r#"UPDATE preview_cards SET trendable = $2, updated_at = now() WHERE id = $1
           RETURNING url, title, description, author_name, author_url, provider_name,
                     provider_url, html, width, height, embed_url, blurhash, language,
                     CASE type WHEN 1 THEN 'photo' WHEN 2 THEN 'video' WHEN 3 THEN 'rich' ELSE 'link' END AS "card_type!""#,
        id,
        trendable
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    // Once a card has its own say, it no longer waits on its provider.
    Ok(Json(json!({
        "id": id.to_string(),
        "url": card.url,
        "title": card.title,
        "description": card.description,
        "type": card.card_type,
        "author_name": card.author_name,
        "author_url": card.author_url,
        "provider_name": card.provider_name,
        "provider_url": card.provider_url,
        "html": card.html,
        "width": card.width,
        "height": card.height,
        "image": null,
        "embed_url": card.embed_url,
        "blurhash": card.blurhash,
        "language": card.language,
        "history": crate::moderation::history::as_json(state, "links", id).await,
        "requires_review": false,
    })))
}

pub async fn admin_approve_trending_link(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    review_link(&state, &auth, id, true).await
}

pub async fn admin_reject_trending_link(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    review_link(&state, &auth, id, false).await
}

// ── /api/v1/admin/trends/links/publishers ─────────────────────────────────

struct ProviderRow {
    id: i64,
    domain: String,
    trendable: Option<bool>,
    reviewed_at: Option<chrono::NaiveDateTime>,
    requested_review_at: Option<chrono::NaiveDateTime>,
}

/// `REST::Admin::Trends::Links::PreviewCardProviderSerializer`.
fn provider_entity(r: ProviderRow) -> Value {
    json!({
        "id": r.id,
        "domain": r.domain,
        "trendable": r.trendable,
        "reviewed_at": r.reviewed_at.map(super::super::convert::mastodon_date),
        "requested_review_at": r.requested_review_at.map(super::super::convert::mastodon_date),
        "requires_review": r.reviewed_at.is_none(),
    })
}

pub async fn admin_list_publishers(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Params(p): Params<PageParams>,
) -> AppResult<Response> {
    auth.require_scope("admin:read")?;
    super::require_permission(&state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let mut rows = sqlx::query_as!(
        ProviderRow,
        r#"SELECT id, domain, trendable, reviewed_at, requested_review_at FROM preview_card_providers
           WHERE ($1::bigint IS NULL OR id < $1) AND ($2::bigint IS NULL OR id > $2)
             AND ($3::bigint IS NULL OR id > $3)
           ORDER BY CASE WHEN $3::bigint IS NULL THEN -id ELSE id END LIMIT $4"#,
        p.max_id.map(|i| i.0),
        p.since_id.map(|i| i.0),
        p.min_id.map(|i| i.0),
        p.limit(100, 200),
    )
    .fetch_all(&state.db)
    .await?;
    if p.min_id.is_some() {
        rows.reverse();
    }
    let first = rows.first().map(|r| r.id.to_string());
    let last = rows.last().map(|r| r.id.to_string());
    let headers =
        super::super::link_headers(&req_headers, &uri, first.as_deref().zip(last.as_deref()));
    let body: Vec<Value> = rows.into_iter().map(provider_entity).collect();
    Ok((headers, Json(body)).into_response())
}

async fn review_publisher(
    state: &AppState,
    auth: &AuthenticatedUser,
    id: i64,
    trendable: bool,
) -> AppResult<Json<Value>> {
    auth.require_scope("admin:write")?;
    super::require_permission(state, auth.account_id, perm::MANAGE_TAXONOMIES).await?;
    let row = sqlx::query_as!(
        ProviderRow,
        r#"UPDATE preview_card_providers SET trendable = $2, reviewed_at = now(), updated_at = now()
           WHERE id = $1
           RETURNING id, domain, trendable, reviewed_at, requested_review_at"#,
        id,
        trendable
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(Json(provider_entity(row)))
}

pub async fn admin_approve_publisher(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    review_publisher(&state, &auth, id, true).await
}

pub async fn admin_reject_publisher(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    review_publisher(&state, &auth, id, false).await
}
