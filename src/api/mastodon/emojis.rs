use super::convert::{custom_emoji_entity, EmojiRow};
use super::types::CustomEmoji;
use crate::{error::AppResult, state::AppState};
use axum::response::Json;

/// `GET /api/v1/custom_emojis`: `CustomEmoji.listed.includes(:category)`,
/// the local emoji that are enabled and shown in the picker, each with its
/// category when it has one.
pub async fn list_custom_emojis(state: AppState) -> AppResult<Json<Vec<CustomEmoji>>> {
    let rows = sqlx::query!(
        r#"SELECT ce.id, ce.shortcode, ce.domain, ce.image_file_name, ce.image_remote_url,
                  ce.image_storage_schema_version, ce.visible_in_picker,
                  ecc.name AS "category_name?", ecc.featured_emoji_id AS "featured_emoji_id?"
           FROM custom_emojis ce
           LEFT JOIN custom_emoji_categories ecc ON ecc.id = ce.category_id
           WHERE ce.domain IS NULL
             AND ce.disabled = false
             AND ce.visible_in_picker = true
           ORDER BY ce.shortcode"#,
    )
    .fetch_all(&state.db)
    .await?;

    let emojis = rows
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
            // `category_loaded? && object.category.present?`.
            let category = r.category_name.map(|name| (name, r.featured_emoji_id));
            custom_emoji_entity(&state, &row, category)
        })
        .collect();

    Ok(Json(emojis))
}
