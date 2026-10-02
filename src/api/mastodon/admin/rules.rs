//! `Admin::RulesController`: the server rules, their translations and their
//! order. Like upstream's controller, none of it is logged.

use axum::{
    extract::{Extension, Path},
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::{FlexBool, FlexId, Params};
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::role::flag,
    state::AppState,
};

/// `Rule::TEXT_SIZE_LIMIT`.
const TEXT_SIZE_LIMIT: usize = 300;

#[derive(Debug, Serialize)]
pub struct AdminRuleTranslation {
    pub id: String,
    pub language: String,
    pub text: String,
    pub hint: String,
}

/// A rule as the admin pages show it: `REST::RuleSerializer`'s fields, with
/// its priority and its translations as rows.
#[derive(Debug, Serialize)]
pub struct AdminRule {
    pub id: String,
    pub text: String,
    pub hint: String,
    pub priority: i32,
    pub translations: Vec<AdminRuleTranslation>,
    pub created_at: String,
    pub updated_at: String,
}

/// `RulePolicy`: every action is `manage_rules`.
async fn authorize(state: &AppState, auth: &AuthenticatedUser, write: bool) -> AppResult<()> {
    auth.require_scope(if write { "admin:write" } else { "admin:read" })?;
    super::require_permission(state, auth.account_id, flag::MANAGE_RULES).await
}

struct RuleRow {
    id: i64,
    text: String,
    hint: String,
    priority: i32,
    created_at: chrono::NaiveDateTime,
    updated_at: chrono::NaiveDateTime,
}

async fn entities(state: &AppState, rows: Vec<RuleRow>) -> AppResult<Vec<AdminRule>> {
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    // `has_many :translations, -> { order(language: :asc) }`.
    let translations = sqlx::query!(
        r#"SELECT id, rule_id, language, text, hint FROM rule_translations
           WHERE rule_id = ANY($1) ORDER BY language ASC"#,
        &ids,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| AdminRule {
            id: r.id.to_string(),
            translations: translations
                .iter()
                .filter(|t| t.rule_id == r.id)
                .map(|t| AdminRuleTranslation {
                    id: t.id.to_string(),
                    language: t.language.clone(),
                    text: t.text.clone(),
                    hint: t.hint.clone(),
                })
                .collect(),
            text: r.text,
            hint: r.hint,
            priority: r.priority,
            created_at: super::super::convert::mastodon_date(r.created_at),
            updated_at: super::super::convert::mastodon_date(r.updated_at),
        })
        .collect())
}

/// `Rule.find`, discarded or not, as `set_rule` finds it.
async fn find(state: &AppState, id: i64) -> AppResult<RuleRow> {
    sqlx::query_as!(
        RuleRow,
        "SELECT id, text, hint, priority, created_at, updated_at FROM rules WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

async fn entity(state: &AppState, id: i64) -> AppResult<AdminRule> {
    let row = find(state, id).await?;
    Ok(entities(state, vec![row]).await?.remove(0))
}

/// `GET /api/v1/admin/rules`: `Rule.ordered.includes(:translations)`.
pub async fn list_admin_rules(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminRule>>> {
    authorize(&state, &auth, false).await?;
    let rows = sqlx::query_as!(
        RuleRow,
        r#"SELECT id, text, hint, priority, created_at, updated_at FROM rules
           WHERE deleted_at IS NULL ORDER BY priority ASC, id ASC"#
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(entities(&state, rows).await?))
}

/// `GET /api/v1/admin/rules/:id`: `Rules#edit`.
pub async fn get_admin_rule(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminRule>> {
    let rule = entity(&state, id).await?;
    authorize(&state, &auth, false).await?;
    Ok(Json(rule))
}

#[derive(Debug, Deserialize)]
pub struct TranslationForm {
    pub id: Option<FlexId>,
    pub language: Option<String>,
    pub text: Option<String>,
    pub hint: Option<String>,
    #[serde(rename = "_destroy")]
    pub destroy: Option<FlexBool>,
}

#[derive(Debug, Deserialize)]
pub struct RuleForm {
    pub text: Option<String>,
    pub hint: Option<String>,
    pub priority: Option<FlexId>,
    /// `translations_attributes`, by either name.
    #[serde(alias = "translations")]
    pub translations_attributes: Option<Vec<TranslationForm>>,
}

/// What a save does to one translation.
enum Change {
    Create {
        language: String,
        text: String,
        hint: String,
    },
    Update {
        id: i64,
        language: Option<String>,
        text: String,
        hint: Option<String>,
    },
    Destroy(i64),
}

/// `accepts_nested_attributes_for :translations, reject_if: text blank,
/// allow_destroy: true`, then `RuleTranslation`'s validations, as the rule's
/// own errors (`Translations language has already been taken`).
async fn translation_changes(
    state: &AppState,
    rule_id: Option<i64>,
    forms: Vec<TranslationForm>,
    errors: &mut Vec<String>,
) -> AppResult<Vec<Change>> {
    let existing: Vec<(i64, String)> = match rule_id {
        Some(rule_id) => sqlx::query!(
            "SELECT id, language FROM rule_translations WHERE rule_id = $1",
            rule_id
        )
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .map(|r| (r.id, r.language))
        .collect(),
        None => vec![],
    };
    let mut changes = vec![];
    for form in forms {
        let id = form.id.map(|i| i.0);
        if let Some(id) = id {
            if !existing.iter().any(|(e, _)| *e == id) {
                // Nested attributes refuse an id the record does not own.
                return Err(AppError::NotFound);
            }
            if form.destroy.is_some_and(|d| d.0) {
                changes.push(Change::Destroy(id));
                continue;
            }
        }
        let text = form.text.unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }
        if text.chars().count() > TEXT_SIZE_LIMIT {
            errors.push(format!(
                "Translations text is too long (maximum is {TEXT_SIZE_LIMIT} characters)"
            ));
        }
        match id {
            Some(id) => {
                if form
                    .language
                    .as_deref()
                    .is_some_and(|l| l.trim().is_empty())
                {
                    errors.push("Translations language can't be blank".into());
                }
                changes.push(Change::Update {
                    id,
                    language: form.language,
                    text,
                    hint: form.hint,
                });
            }
            None => {
                let language = form.language.unwrap_or_default();
                if language.trim().is_empty() {
                    errors.push("Translations language can't be blank".into());
                }
                changes.push(Change::Create {
                    language,
                    text,
                    hint: form.hint.unwrap_or_default(),
                });
            }
        }
    }
    // `validates :language, uniqueness: { scope: :rule_id }`, across what the
    // rule will have after the save.
    let mut languages: Vec<String> = existing
        .iter()
        .filter(|(id, _)| {
            !changes.iter().any(|c| match c {
                Change::Destroy(d) => d == id,
                Change::Update {
                    id: u,
                    language: Some(_),
                    ..
                } => u == id,
                _ => false,
            })
        })
        .map(|(_, l)| l.clone())
        .collect();
    for change in &changes {
        let language = match change {
            Change::Create { language, .. } => Some(language),
            Change::Update {
                language: Some(language),
                ..
            } => Some(language),
            _ => None,
        };
        if let Some(language) = language {
            if languages.contains(language) {
                errors.push("Translations language has already been taken".into());
            } else {
                languages.push(language.clone());
            }
        }
    }
    Ok(changes)
}

fn validate_text(text: &str, errors: &mut Vec<String>) {
    if text.trim().is_empty() {
        errors.push("Text can't be blank".into());
    } else if text.chars().count() > TEXT_SIZE_LIMIT {
        errors.push(format!(
            "Text is too long (maximum is {TEXT_SIZE_LIMIT} characters)"
        ));
    }
}

fn refuse(errors: Vec<String>) -> AppResult<()> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(AppError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )))
    }
}

async fn apply(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    rule_id: i64,
    changes: Vec<Change>,
) -> AppResult<()> {
    // Destroys first, so a language given up can be taken by another row.
    for change in &changes {
        if let Change::Destroy(id) = change {
            sqlx::query!("DELETE FROM rule_translations WHERE id = $1", id)
                .execute(&mut **tx)
                .await?;
        }
    }
    for change in changes {
        match change {
            Change::Destroy(_) => {}
            Change::Create {
                language,
                text,
                hint,
            } => {
                sqlx::query!(
                    r#"INSERT INTO rule_translations (rule_id, language, text, hint, created_at, updated_at)
                       VALUES ($1, $2, $3, $4, now(), now())"#,
                    rule_id,
                    language,
                    text,
                    hint,
                )
                .execute(&mut **tx)
                .await?;
            }
            Change::Update {
                id,
                language,
                text,
                hint,
            } => {
                sqlx::query!(
                    r#"UPDATE rule_translations
                       SET language = COALESCE($2, language), text = $3,
                           hint = COALESCE($4, hint), updated_at = now()
                       WHERE id = $1"#,
                    id,
                    language,
                    text,
                    hint,
                )
                .execute(&mut **tx)
                .await?;
            }
        }
    }
    Ok(())
}

/// `POST /api/v1/admin/rules`: `Rules#create`.
pub async fn create_admin_rule(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<RuleForm>,
) -> AppResult<Json<AdminRule>> {
    authorize(&state, &auth, true).await?;
    let text = form.text.unwrap_or_default();
    let mut errors = vec![];
    validate_text(&text, &mut errors);
    let changes = translation_changes(
        &state,
        None,
        form.translations_attributes.unwrap_or_default(),
        &mut errors,
    )
    .await?;
    refuse(errors)?;
    let mut tx = state.db.begin().await?;
    let id = sqlx::query_scalar!(
        r#"INSERT INTO rules (text, hint, priority, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now()) RETURNING id"#,
        text,
        form.hint.unwrap_or_default(),
        form.priority.map_or(0, |p| p.0 as i32),
    )
    .fetch_one(&mut *tx)
    .await?;
    apply(&mut tx, id, changes).await?;
    tx.commit().await?;
    Ok(Json(entity(&state, id).await?))
}

/// `PATCH /api/v1/admin/rules/:id`: `Rules#update`.
pub async fn update_admin_rule(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<RuleForm>,
) -> AppResult<Json<AdminRule>> {
    let current = find(&state, id).await?;
    authorize(&state, &auth, true).await?;
    let text = form.text.unwrap_or(current.text);
    let mut errors = vec![];
    validate_text(&text, &mut errors);
    let changes = translation_changes(
        &state,
        Some(id),
        form.translations_attributes.unwrap_or_default(),
        &mut errors,
    )
    .await?;
    refuse(errors)?;
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        r#"UPDATE rules SET text = $2, hint = $3, priority = $4, updated_at = now()
           WHERE id = $1"#,
        id,
        text,
        form.hint.unwrap_or(current.hint),
        form.priority.map_or(current.priority, |p| p.0 as i32),
    )
    .execute(&mut *tx)
    .await?;
    apply(&mut tx, id, changes).await?;
    tx.commit().await?;
    Ok(Json(entity(&state, id).await?))
}

/// `DELETE /api/v1/admin/rules/:id`: `Rules#destroy`, which discards the rule
/// so the reports citing it still read.
pub async fn delete_admin_rule(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    find(&state, id).await?;
    authorize(&state, &auth, true).await?;
    // `discard` leaves an already discarded rule as it was.
    sqlx::query!(
        "UPDATE rules SET deleted_at = now(), updated_at = now() WHERE id = $1 AND deleted_at IS NULL",
        id
    )
    .execute(&state.db)
    .await?;
    Ok(Json(serde_json::json!({})))
}

/// `Rule#move!(offset)`: take the rule out of `Rule.ordered`, put it back
/// `offset` places on, and number them all from zero. Ruby's `insert` counts a
/// negative place from the end, so moving the first rule up makes it the
/// last; moving the last one down leaves the order as it was.
async fn move_rule(state: &AppState, id: i64, offset: i64) -> AppResult<()> {
    let mut ids: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM rules WHERE deleted_at IS NULL ORDER BY priority ASC, id ASC"
    )
    .fetch_all(&state.db)
    .await?;
    let Some(position) = ids.iter().position(|r| *r == id) else {
        // A discarded rule is not in `Rule.ordered`; Ruby would fail on it.
        return Err(AppError::NotFound);
    };
    ids.remove(position);
    let target = position as i64 + offset;
    if target < 0 {
        ids.push(id);
    } else if target as usize > ids.len() {
        ids.insert(position, id);
    } else {
        ids.insert(target as usize, id);
    }
    let mut tx = state.db.begin().await?;
    for (index, rule) in ids.iter().enumerate() {
        sqlx::query!(
            "UPDATE rules SET priority = $2, updated_at = now() WHERE id = $1 AND priority <> $2",
            rule,
            index as i32,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// `POST /api/v1/admin/rules/:id/move_up`.
pub async fn move_admin_rule_up(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Vec<AdminRule>>> {
    find(&state, id).await?;
    authorize(&state, &auth, true).await?;
    move_rule(&state, id, -1).await?;
    list_admin_rules(state, Extension(auth)).await
}

/// `POST /api/v1/admin/rules/:id/move_down`.
pub async fn move_admin_rule_down(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Vec<AdminRule>>> {
    find(&state, id).await?;
    authorize(&state, &auth, true).await?;
    move_rule(&state, id, 1).await?;
    list_admin_rules(state, Extension(auth)).await
}
