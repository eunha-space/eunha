use axum::{
    extract::{Extension, Path},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use super::{
    extractors::{rails, Params},
    types::{Filter, FilterKeyword, FilterStatus, FilterV1},
};
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

pub(crate) async fn publish_filters_changed(state: &AppState, account_id: i64) {
    state.streaming.filters_changed(account_id).await;
}

// ── Shared helpers ─────────────────────────────────────────────────────────

async fn fetch_filter(state: &AppState, filter_id: i64, account_id: i64) -> AppResult<Filter> {
    let f = sqlx::query!(
        r#"SELECT id, phrase, context, expires_at,
                  CASE action WHEN 0 THEN 'warn' WHEN 1 THEN 'hide' WHEN 2 THEN 'blur' ELSE 'warn' END AS "action!"
           FROM custom_filters WHERE id = $1 AND account_id = $2"#,
        filter_id, account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let keywords = sqlx::query!(
        "SELECT id, keyword, whole_word FROM custom_filter_keywords WHERE custom_filter_id = $1 ORDER BY id",
        f.id,
    )
    .fetch_all(&state.db)
    .await?;

    let filter_statuses = sqlx::query!(
        "SELECT id, status_id FROM custom_filter_statuses WHERE custom_filter_id = $1 ORDER BY id",
        f.id,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Filter {
        id: f.id.to_string(),
        title: f.phrase.clone(),
        context: f.context.clone(),
        expires_at: f.expires_at.map(super::convert::mastodon_date),
        filter_action: f.action.clone(),
        keywords: keywords
            .into_iter()
            .map(|k| FilterKeyword {
                id: k.id.to_string(),
                keyword: k.keyword,
                whole_word: k.whole_word,
            })
            .collect(),
        statuses: filter_statuses
            .into_iter()
            .map(|r| serde_json::json!({ "id": r.id.to_string(), "status_id": r.status_id.to_string() }))
            .collect(),
    })
}

// ── GET /api/v2/filters ───────────────────────────────────────────────────

pub async fn get_filters_v2(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<Filter>>> {
    auth.require_scope("read:filters")?;
    let filters = sqlx::query!(
        r#"SELECT id, phrase, context, expires_at,
                  CASE action WHEN 0 THEN 'warn' WHEN 1 THEN 'hide' WHEN 2 THEN 'blur' ELSE 'warn' END AS "action!"
           FROM custom_filters WHERE account_id = $1 ORDER BY id"#,
        auth.account_id,
    )
    .fetch_all(&state.db)
    .await?;

    if filters.is_empty() {
        return Ok(Json(vec![]));
    }

    let filter_ids: Vec<i64> = filters.iter().map(|f| f.id).collect();

    let all_keywords = sqlx::query!(
        "SELECT custom_filter_id, id, keyword, whole_word FROM custom_filter_keywords WHERE custom_filter_id = ANY($1::bigint[]) ORDER BY id",
        &filter_ids,
    )
    .fetch_all(&state.db)
    .await?;

    let all_statuses = sqlx::query!(
        "SELECT custom_filter_id, id, status_id FROM custom_filter_statuses WHERE custom_filter_id = ANY($1::bigint[]) ORDER BY id",
        &filter_ids,
    )
    .fetch_all(&state.db)
    .await?;

    let mut keywords_map: std::collections::HashMap<i64, Vec<FilterKeyword>> =
        std::collections::HashMap::new();
    for k in all_keywords {
        keywords_map
            .entry(k.custom_filter_id)
            .or_default()
            .push(FilterKeyword {
                id: k.id.to_string(),
                keyword: k.keyword,
                whole_word: k.whole_word,
            });
    }

    let mut statuses_map: std::collections::HashMap<i64, Vec<serde_json::Value>> =
        std::collections::HashMap::new();
    for r in all_statuses {
        statuses_map.entry(r.custom_filter_id).or_default().push(
            serde_json::json!({ "id": r.id.to_string(), "status_id": r.status_id.to_string() }),
        );
    }

    let result = filters
        .into_iter()
        .map(|f| Filter {
            id: f.id.to_string(),
            title: f.phrase,
            context: f.context,
            expires_at: f.expires_at.map(super::convert::mastodon_date),
            filter_action: f.action,
            keywords: keywords_map.remove(&f.id).unwrap_or_default(),
            statuses: statuses_map.remove(&f.id).unwrap_or_default(),
        })
        .collect();

    Ok(Json(result))
}

// ── GET /api/v2/filters/:id ───────────────────────────────────────────────

pub async fn get_filter_v2(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Filter>> {
    auth.require_scope("read:filters")?;
    fetch_filter(&state, id, auth.account_id).await.map(Json)
}

// ── POST /api/v2/filters ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateFilterForm {
    #[serde(default, deserialize_with = "rails::string")]
    pub title: String,
    #[serde(default, deserialize_with = "rails::strings")]
    pub context: Vec<String>,
    #[serde(default, deserialize_with = "rails::opt_int")]
    pub expires_in: Option<i64>,
    #[serde(default, deserialize_with = "rails::opt_string")]
    pub filter_action: Option<String>,
    #[serde(default, deserialize_with = "rails::nested_attributes")]
    pub keywords_attributes: Option<Vec<KeywordAttr>>,
}

/// Maximum filter title length (Mastodon `CustomFilter::TITLE_LENGTH_LIMIT`).
pub(crate) const FILTER_TITLE_MAX: usize = 256;
/// Valid filter contexts (Mastodon `CustomFilter::VALID_CONTEXTS`).
pub(crate) const VALID_FILTER_CONTEXTS: &[&str] =
    &["home", "notifications", "public", "thread", "account"];
/// Valid filter actions (Mastodon `CustomFilter` action enum).
const VALID_FILTER_ACTIONS: &[&str] = &["warn", "hide", "blur"];

/// Validate a v2 filter submission the way Mastodon's CustomFilter model does:
/// title presence/length, non-empty context restricted to the valid set, and a
/// recognized filter action. Used by both create and update.
/// `custom_filter_keywords.whole_word`'s column default, which a keyword
/// created without one takes.
const WHOLE_WORD_DEFAULT: bool = true;

fn validate_filter_form(form: &CreateFilterForm) -> AppResult<()> {
    if form.title.trim().is_empty() {
        return Err(AppError::Unprocessable("Title can't be blank".into()));
    }
    if form.title.chars().count() > FILTER_TITLE_MAX {
        return Err(AppError::Unprocessable(format!(
            "Title is too long (maximum is {FILTER_TITLE_MAX} characters)"
        )));
    }
    if form.context.is_empty() {
        return Err(AppError::Unprocessable("Context can't be blank".into()));
    }
    if let Some(bad) = form
        .context
        .iter()
        .find(|c| !VALID_FILTER_CONTEXTS.contains(&c.as_str()))
    {
        return Err(AppError::Unprocessable(format!(
            "Context '{bad}' is not a valid filter context"
        )));
    }
    if let Some(action) = form.filter_action.as_deref() {
        if !VALID_FILTER_ACTIONS.contains(&action) {
            return Err(AppError::Unprocessable(format!(
                "Filter action '{action}' is not valid"
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct KeywordAttr {
    #[serde(default, deserialize_with = "rails::opt_int")]
    pub id: Option<i64>,
    #[serde(default, deserialize_with = "rails::opt_string")]
    pub keyword: Option<String>,
    #[serde(default, deserialize_with = "rails::opt_bool")]
    pub whole_word: Option<bool>,
    #[serde(rename = "_destroy", default, deserialize_with = "rails::opt_bool")]
    pub destroy: Option<bool>,
}

pub async fn create_filter_v2(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<CreateFilterForm>,
) -> AppResult<(StatusCode, Json<Filter>)> {
    auth.require_scope("write:filters")?;
    validate_filter_form(&form)?;
    let action =
        crate::db::models::filter_action::from_str(form.filter_action.as_deref().unwrap_or("warn"));
    let filter_id = sqlx::query_scalar!(
        r#"INSERT INTO custom_filters (account_id, phrase, context, action, expires_at, created_at, updated_at)
           VALUES ($1, $2, $3, $4,
                  CASE WHEN $5::bigint IS NULL THEN NULL
                       ELSE now() + ($5 * interval '1 second')
                  END, now(), now())
           RETURNING id"#,
        auth.account_id,
        form.title,
        &form.context,
        action,
        form.expires_in,
    )
    .fetch_one(&state.db)
    .await?;

    if let Some(keywords) = form.keywords_attributes {
        for kw in keywords {
            if kw.destroy == Some(true) {
                continue;
            }
            if let Some(keyword) = kw.keyword {
                sqlx::query!(
                    "INSERT INTO custom_filter_keywords (custom_filter_id, keyword, whole_word, created_at, updated_at) VALUES ($1, $2, $3, now(), now())",
                    filter_id,
                    keyword,
                    kw.whole_word.unwrap_or(WHOLE_WORD_DEFAULT),
                )
                .execute(&state.db)
                .await?;
            }
        }
    }

    let filter = fetch_filter(&state, filter_id, auth.account_id).await?;
    publish_filters_changed(&state, auth.account_id).await;
    Ok((StatusCode::OK, Json(filter)))
}

// ── PUT /api/v2/filters/:id ───────────────────────────────────────────────

pub async fn update_filter_v2(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<CreateFilterForm>,
) -> AppResult<Json<Filter>> {
    auth.require_scope("write:filters")?;
    validate_filter_form(&form)?;
    let action =
        crate::db::models::filter_action::from_str(form.filter_action.as_deref().unwrap_or("warn"));
    let updated = sqlx::query_scalar!(
        r#"UPDATE custom_filters
           SET phrase = $3,
               context = $4,
               action = $5,
               expires_at = CASE WHEN $6::bigint IS NULL THEN NULL
                                 ELSE now() + ($6 * interval '1 second')
                            END,
               updated_at = now()
           WHERE id = $1 AND account_id = $2
           RETURNING id"#,
        id,
        auth.account_id,
        form.title,
        &form.context,
        action,
        form.expires_in,
    )
    .fetch_optional(&state.db)
    .await?;

    if updated.is_none() {
        return Err(AppError::NotFound);
    }

    if let Some(keywords) = form.keywords_attributes {
        for kw in keywords {
            match (kw.id, kw.destroy) {
                (Some(kid), Some(true)) => {
                    sqlx::query!(
                        "DELETE FROM custom_filter_keywords WHERE id = $1 AND custom_filter_id = $2",
                        kid, id,
                    )
                    .execute(&state.db)
                    .await?;
                }
                (Some(kid), _) => {
                    if let Some(keyword) = kw.keyword {
                        sqlx::query!(
                            "UPDATE custom_filter_keywords SET keyword = $1, whole_word = COALESCE($2, whole_word), updated_at = now() WHERE id = $3 AND custom_filter_id = $4",
                            keyword,
                            kw.whole_word,
                            kid,
                            id,
                        )
                        .execute(&state.db)
                        .await?;
                    }
                }
                (None, _) => {
                    if let Some(keyword) = kw.keyword {
                        if kw.destroy != Some(true) {
                            sqlx::query!(
                                "INSERT INTO custom_filter_keywords (custom_filter_id, keyword, whole_word, created_at, updated_at) VALUES ($1, $2, $3, now(), now())",
                                id,
                                keyword,
                                kw.whole_word.unwrap_or(WHOLE_WORD_DEFAULT),
                            )
                            .execute(&state.db)
                            .await?;
                        }
                    }
                }
            }
        }
    }

    let filter = fetch_filter(&state, id, auth.account_id).await?;
    publish_filters_changed(&state, auth.account_id).await;
    Ok(Json(filter))
}

// ── DELETE /api/v2/filters/:id ────────────────────────────────────────────

pub async fn delete_filter_v2(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:filters")?;
    let deleted = sqlx::query_scalar!(
        "DELETE FROM custom_filters WHERE id = $1 AND account_id = $2 RETURNING id",
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?;

    if deleted.is_none() {
        return Err(AppError::NotFound);
    }

    publish_filters_changed(&state, auth.account_id).await;
    Ok(Json(serde_json::json!({})))
}

// ── GET /api/v2/filters/:id/keywords ─────────────────────────────────────

pub async fn get_filter_keywords(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<FilterKeyword>>> {
    auth.require_scope("read:filters")?;
    let exists = sqlx::query_scalar!(
        "SELECT 1 FROM custom_filters WHERE id = $1 AND account_id = $2",
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?;

    if exists.is_none() {
        return Err(AppError::NotFound);
    }

    let keywords = sqlx::query!(
        "SELECT id, keyword, whole_word FROM custom_filter_keywords WHERE custom_filter_id = $1 ORDER BY id",
        id,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|k| FilterKeyword {
        id: k.id.to_string(),
        keyword: k.keyword,
        whole_word: k.whole_word,
    })
    .collect();

    Ok(Json(keywords))
}

// ── POST /api/v2/filters/:id/keywords ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateKeywordForm {
    #[serde(default, deserialize_with = "rails::string")]
    pub keyword: String,
    #[serde(default, deserialize_with = "rails::opt_bool")]
    pub whole_word: Option<bool>,
}

pub async fn create_filter_keyword(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<CreateKeywordForm>,
) -> AppResult<(StatusCode, Json<FilterKeyword>)> {
    auth.require_scope("write:filters")?;
    if form.keyword.trim().is_empty() {
        return Err(AppError::Unprocessable("Keyword can't be blank".into()));
    }
    let exists = sqlx::query_scalar!(
        "SELECT 1 FROM custom_filters WHERE id = $1 AND account_id = $2",
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?;

    if exists.is_none() {
        return Err(AppError::NotFound);
    }

    let kid = sqlx::query_scalar!(
        "INSERT INTO custom_filter_keywords (custom_filter_id, keyword, whole_word, created_at, updated_at) VALUES ($1, $2, $3, now(), now()) RETURNING id",
        id,
        form.keyword,
        form.whole_word.unwrap_or(WHOLE_WORD_DEFAULT),
    )
    .fetch_one(&state.db)
    .await?;

    publish_filters_changed(&state, auth.account_id).await;
    Ok((
        StatusCode::OK,
        Json(FilterKeyword {
            id: kid.to_string(),
            keyword: form.keyword,
            whole_word: form.whole_word.unwrap_or(WHOLE_WORD_DEFAULT),
        }),
    ))
}

// ── GET /api/v2/filters/keywords/:id ──────────────────────────────────────

pub async fn get_filter_keyword(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<FilterKeyword>> {
    auth.require_scope("read:filters")?;
    let kw = sqlx::query!(
        r#"SELECT fk.id, fk.keyword, fk.whole_word
           FROM custom_filter_keywords fk
           JOIN custom_filters f ON f.id = fk.custom_filter_id
           WHERE fk.id = $1 AND f.account_id = $2"#,
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    Ok(Json(FilterKeyword {
        id: kw.id.to_string(),
        keyword: kw.keyword,
        whole_word: kw.whole_word,
    }))
}

// ── PUT /api/v2/filters/keywords/:id ──────────────────────────────────────

pub async fn update_filter_keyword(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<CreateKeywordForm>,
) -> AppResult<Json<FilterKeyword>> {
    auth.require_scope("write:filters")?;
    if form.keyword.trim().is_empty() {
        return Err(AppError::Unprocessable("Keyword can't be blank".into()));
    }
    let updated = sqlx::query!(
        r#"UPDATE custom_filter_keywords fk
           SET keyword = $2, whole_word = COALESCE($3, fk.whole_word), updated_at = now()
           FROM custom_filters f
           WHERE fk.id = $1 AND fk.custom_filter_id = f.id AND f.account_id = $4
           RETURNING fk.id, fk.keyword, fk.whole_word"#,
        id,
        form.keyword,
        form.whole_word,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    publish_filters_changed(&state, auth.account_id).await;
    Ok(Json(FilterKeyword {
        id: updated.id.to_string(),
        keyword: updated.keyword,
        whole_word: updated.whole_word,
    }))
}

// ── DELETE /api/v2/filters/keywords/:id ───────────────────────────────────

pub async fn delete_filter_keyword(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:filters")?;
    let deleted = sqlx::query_scalar!(
        r#"DELETE FROM custom_filter_keywords fk
           USING custom_filters f
           WHERE fk.id = $1 AND fk.custom_filter_id = f.id AND f.account_id = $2
           RETURNING fk.id"#,
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?;

    if deleted.is_none() {
        return Err(AppError::NotFound);
    }

    publish_filters_changed(&state, auth.account_id).await;
    Ok(Json(serde_json::json!({})))
}

// ── GET /api/v2/filters/:id/statuses ─────────────────────────────────────

pub async fn get_filter_statuses(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<FilterStatus>>> {
    auth.require_scope("read:filters")?;
    let exists = sqlx::query_scalar!(
        "SELECT 1 FROM custom_filters WHERE id = $1 AND account_id = $2",
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?;

    if exists.is_none() {
        return Err(AppError::NotFound);
    }

    let rows = sqlx::query!(
        "SELECT id, status_id FROM custom_filter_statuses WHERE custom_filter_id = $1 ORDER BY id",
        id,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| FilterStatus {
                id: r.id.to_string(),
                status_id: r.status_id.to_string(),
            })
            .collect(),
    ))
}

// ── POST /api/v2/filters/:id/statuses ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AddFilterStatusForm {
    #[serde(default, deserialize_with = "rails::string")]
    pub status_id: String,
}

pub async fn add_filter_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<AddFilterStatusForm>,
) -> AppResult<(StatusCode, Json<FilterStatus>)> {
    auth.require_scope("write:filters")?;
    let exists = sqlx::query_scalar!(
        "SELECT 1 FROM custom_filters WHERE id = $1 AND account_id = $2",
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?;

    if exists.is_none() {
        return Err(AppError::NotFound);
    }

    let status_id: i64 = form.status_id.parse().map_err(|_| AppError::NotFound)?;

    let row_id = sqlx::query_scalar!(
        "INSERT INTO custom_filter_statuses (custom_filter_id, status_id, created_at, updated_at) VALUES ($1, $2, now(), now()) RETURNING id",
        id, status_id,
    )
    .fetch_one(&state.db)
    .await?;

    publish_filters_changed(&state, auth.account_id).await;
    Ok((
        StatusCode::OK,
        Json(FilterStatus {
            id: row_id.to_string(),
            status_id: status_id.to_string(),
        }),
    ))
}

// ── GET /api/v2/filters/statuses/:id ──────────────────────────────────────

pub async fn get_filter_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<FilterStatus>> {
    auth.require_scope("read:filters")?;
    let row = sqlx::query!(
        r#"SELECT fs.id, fs.status_id
           FROM custom_filter_statuses fs
           JOIN custom_filters f ON f.id = fs.custom_filter_id
           WHERE fs.id = $1 AND f.account_id = $2"#,
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    Ok(Json(FilterStatus {
        id: row.id.to_string(),
        status_id: row.status_id.to_string(),
    }))
}

// ── DELETE /api/v2/filters/statuses/:id ───────────────────────────────────

pub async fn delete_filter_status(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:filters")?;
    let deleted = sqlx::query_scalar!(
        r#"DELETE FROM custom_filter_statuses fs
           USING custom_filters f
           WHERE fs.id = $1 AND fs.custom_filter_id = f.id AND f.account_id = $2
           RETURNING fs.id"#,
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?;

    if deleted.is_none() {
        return Err(AppError::NotFound);
    }

    publish_filters_changed(&state, auth.account_id).await;
    Ok(Json(serde_json::json!({})))
}

// ── GET /api/v1/filters ───────────────────────────────────────────────────

pub async fn get_filters_v1(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<FilterV1>>> {
    auth.require_scope("read:filters")?;
    let rows = sqlx::query!(
        r#"SELECT fk.id, fk.keyword, fk.whole_word,
                  cf.context, cf.expires_at,
                  CASE cf.action WHEN 0 THEN 'warn' WHEN 1 THEN 'hide' WHEN 2 THEN 'blur' ELSE 'warn' END AS "action!"
           FROM custom_filter_keywords fk
           JOIN custom_filters cf ON cf.id = fk.custom_filter_id
           WHERE cf.account_id = $1
           ORDER BY fk.id"#,
        auth.account_id,
    )
    .fetch_all(&state.db)
    .await?;

    let result = rows
        .into_iter()
        .map(|r| FilterV1 {
            id: r.id.to_string(),
            phrase: r.keyword,
            context: r.context,
            whole_word: r.whole_word,
            expires_at: r.expires_at.map(super::convert::mastodon_date),
            irreversible: r.action == "hide",
        })
        .collect();

    Ok(Json(result))
}

// ── GET /api/v1/filters/:id ───────────────────────────────────────────────

pub async fn get_filter_v1(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<FilterV1>> {
    auth.require_scope("read:filters")?;
    let r = sqlx::query!(
        r#"SELECT fk.id, fk.keyword, fk.whole_word,
                  cf.context, cf.expires_at,
                  CASE cf.action WHEN 0 THEN 'warn' WHEN 1 THEN 'hide' WHEN 2 THEN 'blur' ELSE 'warn' END AS "action!"
           FROM custom_filter_keywords fk
           JOIN custom_filters cf ON cf.id = fk.custom_filter_id
           WHERE fk.id = $1 AND cf.account_id = $2"#,
        id, auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    Ok(Json(FilterV1 {
        id: r.id.to_string(),
        phrase: r.keyword,
        context: r.context,
        whole_word: r.whole_word,
        expires_at: r.expires_at.map(super::convert::mastodon_date),
        irreversible: r.action == "hide",
    }))
}

// ── POST /api/v1/filters ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateFilterV1Form {
    #[serde(default, deserialize_with = "rails::string")]
    pub phrase: String,
    #[serde(default, deserialize_with = "rails::strings")]
    pub context: Vec<String>,
    #[serde(default, deserialize_with = "rails::opt_bool")]
    pub irreversible: Option<bool>,
    #[serde(default, deserialize_with = "rails::opt_bool")]
    pub whole_word: Option<bool>,
    #[serde(default, deserialize_with = "rails::opt_int")]
    pub expires_in: Option<i64>,
}

pub async fn create_filter_v1(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<CreateFilterV1Form>,
) -> AppResult<(StatusCode, Json<FilterV1>)> {
    auth.require_scope("write:filters")?;
    // The keyword the phrase becomes, which `CustomFilterKeyword` requires.
    if form.phrase.trim().is_empty() {
        return Err(AppError::Unprocessable("Keyword can't be blank".into()));
    }
    let action = crate::db::models::filter_action::from_str(if form.irreversible == Some(true) {
        "hide"
    } else {
        "warn"
    });
    let filter_id = sqlx::query_scalar!(
        r#"INSERT INTO custom_filters (account_id, phrase, context, action, expires_at, created_at, updated_at)
           VALUES ($1, $2, $3, $4,
                  CASE WHEN $5::bigint IS NULL THEN NULL
                       ELSE now() + ($5 * interval '1 second')
                  END, now(), now())
           RETURNING id"#,
        auth.account_id,
        form.phrase,
        &form.context,
        action,
        form.expires_in,
    )
    .fetch_one(&state.db)
    .await?;

    let whole_word = form.whole_word.unwrap_or(WHOLE_WORD_DEFAULT);
    let keyword_id = sqlx::query_scalar!(
        "INSERT INTO custom_filter_keywords (custom_filter_id, keyword, whole_word, created_at, updated_at) VALUES ($1, $2, $3, now(), now()) RETURNING id",
        filter_id, form.phrase, whole_word,
    )
    .fetch_one(&state.db)
    .await?;

    let f = sqlx::query!(
        r#"SELECT context, expires_at,
                  CASE action WHEN 0 THEN 'warn' WHEN 1 THEN 'hide' WHEN 2 THEN 'blur' ELSE 'warn' END AS "action!"
           FROM custom_filters WHERE id = $1"#,
        filter_id,
    )
    .fetch_one(&state.db)
    .await?;

    publish_filters_changed(&state, auth.account_id).await;
    Ok((
        StatusCode::OK,
        Json(FilterV1 {
            id: keyword_id.to_string(),
            phrase: form.phrase,
            context: f.context,
            whole_word,
            expires_at: f.expires_at.map(super::convert::mastodon_date),
            irreversible: f.action == "hide",
        }),
    ))
}

// ── PUT /api/v1/filters/:id ───────────────────────────────────────────────

pub async fn update_filter_v1(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<CreateFilterV1Form>,
) -> AppResult<Json<FilterV1>> {
    auth.require_scope("write:filters")?;
    // The keyword the phrase becomes, which `CustomFilterKeyword` requires.
    if form.phrase.trim().is_empty() {
        return Err(AppError::Unprocessable("Keyword can't be blank".into()));
    }
    let action = crate::db::models::filter_action::from_str(if form.irreversible == Some(true) {
        "hide"
    } else {
        "warn"
    });

    // id is a keyword ID; find it and its parent filter (ownership check via filter)
    let existing = sqlx::query!(
        r#"SELECT fk.id AS keyword_id, cf.id AS filter_id
           FROM custom_filter_keywords fk
           JOIN custom_filters cf ON cf.id = fk.custom_filter_id
           WHERE fk.id = $1 AND cf.account_id = $2"#,
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    sqlx::query!(
        r#"UPDATE custom_filters
           SET phrase = $3, context = $4, action = $5,
               expires_at = CASE WHEN $6::bigint IS NULL THEN NULL
                                 ELSE now() + ($6 * interval '1 second')
                            END,
               updated_at = now()
           WHERE id = $1 AND account_id = $2"#,
        existing.filter_id,
        auth.account_id,
        form.phrase,
        &form.context,
        action,
        form.expires_in,
    )
    .execute(&state.db)
    .await?;

    let whole_word = sqlx::query_scalar!(
        "UPDATE custom_filter_keywords SET keyword = $2, whole_word = COALESCE($3, whole_word), updated_at = now() WHERE id = $1 RETURNING whole_word",
        id, form.phrase, form.whole_word,
    )
    .fetch_one(&state.db)
    .await?;

    let f = sqlx::query!(
        r#"SELECT context, expires_at,
                  CASE action WHEN 0 THEN 'warn' WHEN 1 THEN 'hide' WHEN 2 THEN 'blur' ELSE 'warn' END AS "action!"
           FROM custom_filters WHERE id = $1"#,
        existing.filter_id,
    )
    .fetch_one(&state.db)
    .await?;

    publish_filters_changed(&state, auth.account_id).await;
    Ok(Json(FilterV1 {
        id: id.to_string(),
        phrase: form.phrase,
        context: f.context,
        whole_word,
        expires_at: f.expires_at.map(super::convert::mastodon_date),
        irreversible: f.action == "hide",
    }))
}

// ── DELETE /api/v1/filters/:id ────────────────────────────────────────────

pub async fn delete_filter_v1(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:filters")?;

    // id is a keyword ID; verify ownership via the parent filter
    let existing = sqlx::query!(
        r#"SELECT fk.id AS keyword_id, cf.id AS filter_id
           FROM custom_filter_keywords fk
           JOIN custom_filters cf ON cf.id = fk.custom_filter_id
           WHERE fk.id = $1 AND cf.account_id = $2"#,
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?;

    if existing.is_none() {
        return Err(AppError::NotFound);
    }
    let existing = existing.unwrap();

    sqlx::query!("DELETE FROM custom_filter_keywords WHERE id = $1", id)
        .execute(&state.db)
        .await?;

    // Clean up the parent filter if it has no remaining keywords or statuses
    let has_remaining = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM custom_filter_keywords WHERE custom_filter_id = $1
                       UNION ALL
                       SELECT 1 FROM custom_filter_statuses WHERE custom_filter_id = $1)",
        existing.filter_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false);

    if !has_remaining {
        sqlx::query!(
            "DELETE FROM custom_filters WHERE id = $1",
            existing.filter_id
        )
        .execute(&state.db)
        .await?;
    }

    publish_filters_changed(&state, auth.account_id).await;
    Ok(Json(serde_json::json!({})))
}
