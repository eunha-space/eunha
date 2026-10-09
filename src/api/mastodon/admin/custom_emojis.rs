//! Custom emoji administration, which Mastodon has only as server-rendered
//! admin pages (`Admin::CustomEmojisController` and
//! `Form::CustomEmojiBatch`), over the REST API
//! (`admin-custom-emoji-rest-api`). An upload is validated as `CustomEmoji`
//! validates one and stored where Paperclip stores it.

use axum::{
    extract::{Extension, Multipart, Path},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use super::{perm, require_permission};
use crate::{
    api::mastodon::convert::EmojiRow,
    custom_emoji,
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::action_log::{self, Target},
    state::AppState,
};

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

/// A local emoji and its category's name.
async fn load(state: &AppState, id: Option<i64>) -> AppResult<Vec<AdminCustomEmoji>> {
    let rows = sqlx::query!(
        r#"SELECT ce.id, ce.shortcode, ce.domain, ce.image_file_name, ce.image_remote_url,
                  ce.image_storage_schema_version, ce.visible_in_picker, ce.disabled,
                  c.name AS "category?"
           FROM custom_emojis ce
           LEFT JOIN custom_emoji_categories c ON c.id = ce.category_id
           WHERE ce.domain IS NULL AND ($1::bigint IS NULL OR ce.id = $1)
           ORDER BY ce.shortcode"#,
        id,
    )
    .fetch_all(&state.db)
    .await?;
    let domain = &state.instance.domain;
    Ok(rows
        .into_iter()
        .map(|r| {
            let row = EmojiRow {
                id: r.id,
                shortcode: r.shortcode,
                domain: r.domain,
                image_file_name: r.image_file_name,
                image_remote_url: r.image_remote_url,
                image_storage_schema_version: r.image_storage_schema_version,
                visible_in_picker: r.visible_in_picker,
            };
            AdminCustomEmoji {
                id: row.id.to_string(),
                url: row.image().url(&state.storage, domain, "original"),
                static_url: row.image().url(&state.storage, domain, "static"),
                shortcode: row.shortcode,
                visible_in_picker: row.visible_in_picker,
                disabled: r.disabled,
                category: r.category,
            }
        })
        .collect())
}

async fn load_one(state: &AppState, id: i64) -> AppResult<AdminCustomEmoji> {
    load(state, Some(id))
        .await?
        .into_iter()
        .next()
        .ok_or(AppError::NotFound)
}

/// `validates :shortcode, uniqueness: { scope: :domain }`.
async fn shortcode_taken(
    state: &AppState,
    shortcode: &str,
    except: Option<i64>,
) -> AppResult<bool> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM custom_emojis
                         WHERE shortcode = $1 AND domain IS NULL
                           AND ($2::bigint IS NULL OR id <> $2)) AS "taken!""#,
        shortcode,
        except,
    )
    .fetch_one(&state.db)
    .await?)
}

/// The shortcode's errors, uniqueness first, as `validates :shortcode`
/// declares them.
async fn shortcode_errors(
    state: &AppState,
    shortcode: &str,
    except: Option<i64>,
) -> AppResult<Vec<String>> {
    let mut errors = Vec::new();
    if shortcode_taken(state, shortcode, except).await? {
        errors.push("Shortcode has already been taken".to_owned());
    }
    errors.extend(custom_emoji::shortcode_errors(shortcode, true));
    Ok(errors)
}

fn invalid(errors: Vec<String>) -> AppResult<()> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(AppError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )))
    }
}

// ── GET /api/v1/admin/custom_emojis ──────────────────────────────────────

pub async fn list_admin_custom_emojis(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<AdminCustomEmoji>>> {
    require_permission(&state, auth.account_id, perm::MANAGE_CUSTOM_EMOJIS).await?;
    Ok(Json(load(&state, None).await?))
}

// ── POST /api/v1/admin/custom_emojis ─────────────────────────────────────

/// `Admin::CustomEmojisController#create`: `CustomEmoji.new(shortcode:,
/// image:, visible_in_picker:)`, saved if valid, and `log_action :create`.
pub async fn create_admin_custom_emoji(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    mut multipart: Multipart,
) -> AppResult<Json<AdminCustomEmoji>> {
    require_permission(&state, auth.account_id, perm::MANAGE_CUSTOM_EMOJIS).await?;

    let mut shortcode = String::new();
    let mut image: Option<(Vec<u8>, String)> = None;
    let mut visible_in_picker = true;
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "shortcode" => shortcode = field.text().await.unwrap_or_default(),
            "image" => {
                let declared = field.content_type().unwrap_or("").to_string();
                if let Ok(bytes) = field.bytes().await {
                    if !bytes.is_empty() {
                        let content_type = custom_emoji::content_type_of(&bytes, &declared);
                        image = Some((bytes.to_vec(), content_type));
                    }
                }
            }
            "visible_in_picker" => {
                let value = field.text().await.unwrap_or_default();
                // `ActiveModel::Type::Boolean`.
                visible_in_picker = !matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "0" | "f" | "false" | "off"
                );
            }
            _ => {}
        }
    }

    // `Attachmentable#check_image_dimension` raises before validation.
    if let Some((data, content_type)) = &image {
        custom_emoji::check_dimensions(data, content_type)?;
    }
    let mut errors =
        custom_emoji::image_errors(image.as_ref().map(|(d, t)| (d.as_slice(), t.as_str())));
    errors.extend(shortcode_errors(&state, &shortcode, None).await?);
    invalid(errors)?;
    let Some((data, content_type)) = image else {
        return Err(AppError::Unprocessable(
            "Validation failed: Image can't be blank".into(),
        ));
    };
    let processed = custom_emoji::process(data, &content_type)?;

    let mut tx = state.db.begin().await?;
    let id = sqlx::query_scalar!(
        r#"INSERT INTO custom_emojis
             (shortcode, visible_in_picker, image_file_name, image_content_type,
              image_file_size, image_updated_at, image_storage_schema_version,
              created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, now(), 1, now(), now())
           RETURNING id"#,
        shortcode,
        visible_in_picker,
        processed.file_name,
        processed.content_type,
        processed.original.len() as i32,
    )
    .fetch_one(&mut *tx)
    .await?;
    custom_emoji::store(&state.storage, id, &processed).await?;
    action_log::log(
        &mut *tx,
        auth.account_id,
        "create",
        &Target::custom_emoji(id, &shortcode),
    )
    .await?;
    tx.commit().await?;

    Ok(Json(load_one(&state, id).await?))
}

// ── DELETE /api/v1/admin/custom_emojis/:id ───────────────────────────────

/// `Form::CustomEmojiBatch#delete!`: the emoji, its image's files, and
/// `log_action :destroy`.
pub async fn delete_admin_custom_emoji(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    require_permission(&state, auth.account_id, perm::MANAGE_CUSTOM_EMOJIS).await?;
    let Some(row) = sqlx::query!(
        "SELECT shortcode, domain, image_file_name, image_storage_schema_version
         FROM custom_emojis WHERE id = $1",
        id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(StatusCode::OK);
    };
    sqlx::query!("DELETE FROM custom_emojis WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    custom_emoji::delete_files(
        &state.storage,
        custom_emoji::ImageRef {
            id,
            domain: row.domain.as_deref(),
            image_file_name: row.image_file_name.as_deref(),
            image_remote_url: None,
            image_storage_schema_version: row.image_storage_schema_version,
        },
    )
    .await;
    action_log::log(
        &state.db,
        auth.account_id,
        "destroy",
        &Target::custom_emoji(id, &row.shortcode),
    )
    .await?;
    Ok(StatusCode::OK)
}

// ── PATCH /api/v1/admin/custom_emojis/:id ────────────────────────────────

/// Read from Rails-style params: a form, a JSON body or the query string.
#[derive(Debug, Deserialize)]
pub struct PatchEmojiForm {
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_string"
    )]
    pub shortcode: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    pub visible_in_picker: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    pub disabled: Option<bool>,
}

/// What `Form::CustomEmojiBatch` does to one emoji: list or unlist it
/// (`log_action :update`), enable or disable it (`:enable`, `:disable`).
/// A new shortcode, which Mastodon's pages cannot give, is validated as
/// `CustomEmoji` validates one and logged as an update.
pub async fn update_admin_custom_emoji(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    crate::api::mastodon::extractors::Params(form): crate::api::mastodon::extractors::Params<
        PatchEmojiForm,
    >,
) -> AppResult<Json<AdminCustomEmoji>> {
    require_permission(&state, auth.account_id, perm::MANAGE_CUSTOM_EMOJIS).await?;
    let current = sqlx::query!(
        "SELECT shortcode FROM custom_emojis WHERE id = $1 AND domain IS NULL",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let mut shortcode = current.shortcode;
    if let Some(new) = form.shortcode.filter(|new| *new != shortcode) {
        invalid(shortcode_errors(&state, &new, Some(id)).await?)?;
        sqlx::query!(
            "UPDATE custom_emojis SET shortcode = $1, updated_at = now() WHERE id = $2",
            new,
            id
        )
        .execute(&state.db)
        .await?;
        shortcode = new;
        log(&state, auth.account_id, "update", id, &shortcode).await?;
    }
    if let Some(v) = form.visible_in_picker {
        sqlx::query!(
            "UPDATE custom_emojis SET visible_in_picker = $1, updated_at = now() WHERE id = $2",
            v,
            id
        )
        .execute(&state.db)
        .await?;
        log(&state, auth.account_id, "update", id, &shortcode).await?;
    }
    if let Some(d) = form.disabled {
        sqlx::query!(
            "UPDATE custom_emojis SET disabled = $1, updated_at = now() WHERE id = $2",
            d,
            id
        )
        .execute(&state.db)
        .await?;
        let action = if d { "disable" } else { "enable" };
        log(&state, auth.account_id, action, id, &shortcode).await?;
    }
    Ok(Json(load_one(&state, id).await?))
}

async fn log(
    state: &AppState,
    account_id: i64,
    action: &str,
    id: i64,
    shortcode: &str,
) -> AppResult<()> {
    action_log::log(
        &state.db,
        account_id,
        action,
        &Target::custom_emoji(id, shortcode),
    )
    .await
}
