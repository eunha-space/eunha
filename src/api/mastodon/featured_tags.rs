use axum::{
    extract::{Extension, Path},
    Json,
};
use serde::Deserialize;

use super::types::{FeaturedTag, Tag};
use crate::{
    error::{AppError, AppResult},
    middleware::{AuthenticatedUser, ResolvedInstance},
    state::AppState,
};

fn featured_tag_url(domain: &str, username: &str, name: &str) -> String {
    format!("https://{domain}/@{username}/tagged/{name}")
}

fn tag_url(domain: &str, name: &str) -> String {
    format!("https://{domain}/tags/{name}")
}

// ── GET /api/v1/featured_tags ─────────────────────────────────────────────

pub async fn list_featured_tags(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<FeaturedTag>>> {
    auth.require_scope("read:accounts")?;
    let domain = &instance.domain;

    let username = sqlx::query_scalar!(
        "SELECT username FROM accounts WHERE id = $1",
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?;

    let rows = sqlx::query!(
        r#"SELECT ft.id, t.name, ft.statuses_count, ft.last_status_at
           FROM featured_tags ft
           JOIN tags t ON t.id = ft.tag_id
           WHERE ft.account_id = $1
           ORDER BY ft.statuses_count DESC"#,
        auth.account_id,
    )
    .fetch_all(&state.db)
    .await?;

    let tags = rows
        .into_iter()
        .map(|r| FeaturedTag {
            id: r.id.to_string(),
            name: r.name.clone(),
            url: format!("https://{}/@{}/tagged/{}", domain, username, r.name),
            statuses_count: r.statuses_count.to_string(),
            last_status_at: r.last_status_at.map(|t| t.format("%Y-%m-%d").to_string()),
        })
        .collect();

    Ok(Json(tags))
}

// ── POST /api/v1/featured_tags ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct FeaturedTagForm {
    pub name: String,
}

pub async fn feature_tag(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<FeaturedTagForm>,
) -> AppResult<Json<FeaturedTag>> {
    auth.require_scope("write:accounts")?;
    let domain = &instance.domain;
    let written = form.name.clone();
    let name = form.name.to_lowercase();
    let name = name.trim_start_matches('#');

    // Mastodon validates presence + hashtag format (no whitespace/punctuation).
    if name.is_empty() {
        return Err(AppError::Unprocessable(
            "Validation failed: Name can't be blank".into(),
        ));
    }
    if name.chars().any(|c| !(c.is_alphanumeric() || c == '_')) {
        return Err(AppError::Unprocessable(
            "Validation failed: Name is not a valid hashtag".into(),
        ));
    }

    let username = sqlx::query_scalar!(
        "SELECT username FROM accounts WHERE id = $1",
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?;

    let tag_id = crate::tags::find_or_create(&state.db, &written)
        .await?
        .ok_or_else(|| AppError::Unprocessable("Validation failed: Tag is invalid".into()))?;

    // Cap at 10 featured tags (Mastodon FeaturedTag::LIMIT), but only when
    // featuring a new tag — re-featuring an existing one is idempotent.
    let already_featured = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM featured_tags WHERE account_id = $1 AND tag_id = $2)",
        auth.account_id,
        tag_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false);
    if !already_featured {
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM featured_tags WHERE account_id = $1",
            auth.account_id,
        )
        .fetch_one(&state.db)
        .await?
        .unwrap_or(0);
        if count >= 10 {
            return Err(AppError::Unprocessable(
                "Validation failed: You have already reached the limit of 10 featured hashtags"
                    .into(),
            ));
        }
    }

    let row = sqlx::query!(
        r#"INSERT INTO featured_tags
             (account_id, tag_id, name, statuses_count, last_status_at, created_at, updated_at)
           VALUES ($1, $2, $3,
             -- `before_create :reset_data`: `visible_tagged_account_statuses`.
             (SELECT count(*) FROM statuses s
              JOIN statuses_tags st ON st.status_id = s.id AND st.tag_id = $2
              WHERE s.account_id = $1 AND s.deleted_at IS NULL AND s.visibility IN (0, 1)),
             (SELECT s.created_at FROM statuses s
              JOIN statuses_tags st ON st.status_id = s.id AND st.tag_id = $2
              WHERE s.account_id = $1 AND s.deleted_at IS NULL AND s.visibility IN (0, 1)
              ORDER BY s.id DESC LIMIT 1),
             now(), now())
           ON CONFLICT (account_id, tag_id) DO UPDATE SET name = EXCLUDED.name
           RETURNING id, statuses_count, last_status_at"#,
        auth.account_id,
        tag_id,
        name,
    )
    .fetch_one(&state.db)
    .await?;

    Ok(Json(FeaturedTag {
        id: row.id.to_string(),
        name: name.to_string(),
        url: featured_tag_url(domain, &username, name),
        statuses_count: row.statuses_count.to_string(),
        last_status_at: row.last_status_at.map(|t| t.format("%Y-%m-%d").to_string()),
    }))
}

// ── DELETE /api/v1/featured_tags/:id ─────────────────────────────────────

pub async fn unfeature_tag(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:accounts")?;
    let deleted = sqlx::query!(
        "DELETE FROM featured_tags WHERE id = $1 AND account_id = $2",
        id,
        auth.account_id,
    )
    .execute(&state.db)
    .await?;

    if deleted.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }

    Ok(Json(serde_json::json!({})))
}

// ── POST /api/v1/tags/:name/feature ──────────────────────────────────────

pub async fn feature_tag_by_name(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> AppResult<Json<Tag>> {
    auth.require_scope("write:accounts")?;
    let domain = &instance.domain;
    let written = name.clone();
    let name = name.to_lowercase();
    let name = name.trim_start_matches('#');

    let tag_id = crate::tags::find_or_create(&state.db, &written)
        .await?
        .ok_or_else(|| AppError::Unprocessable("Validation failed: Tag is invalid".into()))?;

    // Cap at 10 featured tags (Mastodon FeaturedTag::LIMIT), unless already featured.
    let already_featured = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM featured_tags WHERE account_id = $1 AND tag_id = $2)",
        auth.account_id,
        tag_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false);
    if !already_featured {
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM featured_tags WHERE account_id = $1",
            auth.account_id,
        )
        .fetch_one(&state.db)
        .await?
        .unwrap_or(0);
        if count >= 10 {
            return Err(AppError::Unprocessable(
                "Validation failed: You have already reached the limit of 10 featured hashtags"
                    .into(),
            ));
        }
    }

    sqlx::query!(
        r#"INSERT INTO featured_tags
             (account_id, tag_id, name, statuses_count, last_status_at, created_at, updated_at)
           VALUES ($1, $2, $3,
             -- `before_create :reset_data`: `visible_tagged_account_statuses`.
             (SELECT count(*) FROM statuses s
              JOIN statuses_tags st ON st.status_id = s.id AND st.tag_id = $2
              WHERE s.account_id = $1 AND s.deleted_at IS NULL AND s.visibility IN (0, 1)),
             (SELECT s.created_at FROM statuses s
              JOIN statuses_tags st ON st.status_id = s.id AND st.tag_id = $2
              WHERE s.account_id = $1 AND s.deleted_at IS NULL AND s.visibility IN (0, 1)
              ORDER BY s.id DESC LIMIT 1),
             now(), now())
           ON CONFLICT (account_id, tag_id) DO UPDATE SET name = EXCLUDED.name"#,
        auth.account_id,
        tag_id,
        name,
    )
    .execute(&state.db)
    .await?;

    let following = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM tag_follows WHERE account_id = $1 AND tag_id = $2)",
        auth.account_id,
        tag_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false);

    let history = super::tags::fetch_tag_history(&state, tag_id).await;

    Ok(Json(Tag {
        id: tag_id.to_string(),
        url: tag_url(domain, name),
        name: name.to_string(),
        history,
        following: Some(following),
        featuring: Some(true),
    }))
}

// ── POST /api/v1/tags/:name/unfeature ────────────────────────────────────

pub async fn unfeature_tag_by_name(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> AppResult<Json<Tag>> {
    auth.require_scope("write:accounts")?;
    let domain = &instance.domain;
    let name = name.to_lowercase();

    let tag = sqlx::query!("SELECT id FROM tags WHERE name = $1", name,)
        .fetch_optional(&state.db)
        .await?;

    let Some(tag) = tag else {
        return Ok(Json(Tag {
            id: String::new(),
            url: tag_url(domain, &name),
            name,
            history: vec![],
            following: Some(false),
            featuring: Some(false),
        }));
    };

    sqlx::query!(
        "DELETE FROM featured_tags WHERE account_id = $1 AND tag_id = $2",
        auth.account_id,
        tag.id,
    )
    .execute(&state.db)
    .await?;

    let following = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM tag_follows WHERE account_id = $1 AND tag_id = $2)",
        auth.account_id,
        tag.id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false);

    let history = super::tags::fetch_tag_history(&state, tag.id).await;

    Ok(Json(Tag {
        id: tag.id.to_string(),
        url: tag_url(domain, &name),
        name,
        history,
        following: Some(following),
        featuring: Some(false),
    }))
}

// ── GET /api/v1/featured_tags/suggestions ────────────────────────────────

pub async fn featured_tag_suggestions(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<super::types::Tag>>> {
    auth.require_scope("read:accounts")?;
    let domain = &instance.domain;

    let rows = sqlx::query!(
        r#"SELECT t.id, t.name
           FROM tags t
           JOIN statuses_tags st ON st.tag_id = t.id
           JOIN statuses s ON s.id = st.status_id
           WHERE s.account_id = $1 AND s.deleted_at IS NULL
           GROUP BY t.id, t.name
           ORDER BY COUNT(*) DESC
           LIMIT 10"#,
        auth.account_id,
    )
    .fetch_all(&state.db)
    .await?;

    let tags = rows
        .into_iter()
        .map(|r| super::types::Tag {
            id: r.id.to_string(),
            url: format!("https://{}/tags/{}", domain, r.name),
            name: r.name,
            history: vec![],
            following: None,
            featuring: None,
        })
        .collect();

    Ok(Json(tags))
}
