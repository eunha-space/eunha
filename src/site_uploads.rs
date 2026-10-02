//! Mastodon's `SiteUpload`: the images an administrator uploads in the
//! server settings (the thumbnail, the mascot, the app icon and the favicon),
//! one `site_uploads` row each, stored where Paperclip stores them
//! (`site_uploads/files/:id_partition/:style/:filename`) with the styles
//! `SiteUpload::STYLES` renders.

use std::io::Cursor;

use image::{imageops::FilterType, DynamicImage, ImageFormat};
use serde::Serialize;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// `Form::AdminSettings::UPLOAD_KEYS`.
pub const VARS: [&str; 4] = ["thumbnail", "mascot", "app_icon", "favicon"];

/// `SiteUpload::FAVICON_SIZES`.
pub const FAVICON_SIZES: [u32; 3] = [16, 32, 48];
/// `SiteUpload::APPLE_ICON_SIZES`.
const APPLE_ICON_SIZES: [u32; 11] = [57, 60, 72, 76, 114, 120, 144, 152, 167, 180, 1024];
/// `SiteUpload::ANDROID_ICON_SIZES`.
pub const ANDROID_ICON_SIZES: [u32; 9] = [36, 48, 72, 96, 144, 192, 256, 384, 512];

/// A `site_uploads` row.
#[derive(Debug, Clone)]
pub struct SiteUpload {
    pub id: i64,
    pub var: String,
    pub file_file_name: Option<String>,
    pub file_content_type: Option<String>,
    pub file_file_size: Option<i32>,
    pub blurhash: Option<String>,
    pub meta: Option<serde_json::Value>,
    pub updated_at: chrono::NaiveDateTime,
}

/// A style of an upload: its name in the path, and how it is rendered.
struct Style {
    name: String,
    /// `geometry: "WxH#"`: resized to fill and cropped, as PNG.
    fill: Option<(u32, u32)>,
}

/// `SiteUpload::STYLES[var]`, without the original.
fn styles(var: &str) -> Vec<Style> {
    let square = |size: u32| Style {
        name: size.to_string(),
        fill: Some((size, size)),
    };
    match var {
        "app_icon" => {
            let mut sizes: Vec<u32> = APPLE_ICON_SIZES.to_vec();
            for size in ANDROID_ICON_SIZES {
                if !sizes.contains(&size) {
                    sizes.push(size);
                }
            }
            sizes.into_iter().map(square).collect()
        }
        "favicon" => FAVICON_SIZES.into_iter().map(square).collect(),
        "thumbnail" => vec![
            Style {
                name: "@1x".into(),
                fill: Some((1200, 630)),
            },
            Style {
                name: "@2x".into(),
                fill: Some((2400, 1260)),
            },
        ],
        _ => vec![],
    }
}

impl SiteUpload {
    /// The file name a style is stored under: the original's, with the
    /// style's `png` format as its extension, as Paperclip's `:filename`
    /// interpolates it.
    fn file_name_for(&self, style: &str) -> Option<String> {
        let name = self.file_file_name.as_deref()?;
        if style == "original" || styles(&self.var).iter().all(|s| s.name != style) {
            return Some(name.to_owned());
        }
        let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
        Some(format!("{stem}.png"))
    }

    /// `file.path(style)`, relative to the bucket.
    fn key(&self, style: &str) -> Option<String> {
        Some(format!(
            "site_uploads/files/{}/{}/{}",
            crate::media::int_to_path(self.id),
            style,
            self.file_name_for(style)?
        ))
    }

    /// `full_asset_url(file.url(style))`.
    pub fn url(&self, state: &AppState, style: &str) -> Option<String> {
        self.key(style).map(|key| state.storage.public_url(&key))
    }

    fn keys(&self) -> Vec<String> {
        std::iter::once("original".to_owned())
            .chain(styles(&self.var).into_iter().map(|s| s.name))
            .filter_map(|style| self.key(&style))
            .collect()
    }

    /// What the settings API shows of an upload.
    pub fn entity(&self, state: &AppState) -> Entity {
        Entity {
            id: self.id.to_string(),
            var: self.var.clone(),
            url: self.url(state, "original"),
            content_type: self.file_content_type.clone(),
            file_size: self.file_file_size,
            blurhash: self.blurhash.clone(),
            meta: self.meta.clone(),
            updated_at: crate::api::mastodon::convert::mastodon_date(self.updated_at),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Entity {
    pub id: String,
    pub var: String,
    pub url: Option<String>,
    pub content_type: Option<String>,
    pub file_size: Option<i32>,
    pub blurhash: Option<String>,
    pub meta: Option<serde_json::Value>,
    pub updated_at: String,
}

/// `SiteUpload.find_by(var:)`, when it has a file.
pub async fn find(state: &AppState, var: &str) -> AppResult<Option<SiteUpload>> {
    Ok(sqlx::query_as!(
        SiteUpload,
        r#"SELECT id, var, file_file_name, file_content_type, file_file_size, blurhash,
                  meta, updated_at
           FROM site_uploads WHERE var = $1 AND file_file_name IS NOT NULL"#,
        var,
    )
    .fetch_optional(&state.db)
    .await?)
}

/// Every upload with a file, by `var`.
pub async fn all(state: &AppState) -> AppResult<Vec<SiteUpload>> {
    Ok(sqlx::query_as!(
        SiteUpload,
        r#"SELECT id, var, file_file_name, file_content_type, file_file_size, blurhash,
                  meta, updated_at
           FROM site_uploads WHERE file_file_name IS NOT NULL ORDER BY var"#,
    )
    .fetch_all(&state.db)
    .await?)
}

/// The rendered styles of an upload, and what `set_meta` and the blurhash
/// transcoder record.
struct Rendered {
    original_extension: &'static str,
    styles: Vec<(String, Vec<u8>)>,
    width: u32,
    height: u32,
    blurhash: Option<String>,
}

fn render(var: &str, content_type: &str, data: &[u8]) -> Result<Rendered, String> {
    let format = ImageFormat::from_mime_type(content_type)
        .ok_or_else(|| "File content type is invalid".to_owned())?;
    let image = image::load_from_memory_with_format(data, format)
        .map_err(|_| "File could not be processed".to_owned())?;
    let mut rendered = Rendered {
        original_extension: crate::media::ext_for_content_type(content_type),
        styles: vec![],
        width: image.width(),
        height: image.height(),
        blurhash: None,
    };
    for style in styles(var) {
        let Some((width, height)) = style.fill else {
            continue;
        };
        let resized = image.resize_to_fill(width, height, FilterType::Lanczos3);
        if style.name == "@1x" {
            // `blurhash: { x_comp: 4, y_comp: 4 }` on the `@1x` style.
            let small = resized.thumbnail(100, 100);
            rendered.blurhash = blurhash::encode(
                4,
                4,
                small.width(),
                small.height(),
                small.to_rgba8().as_raw(),
            )
            .ok();
        }
        rendered.styles.push((style.name, png(&resized)?));
    }
    Ok(rendered)
}

fn png(image: &DynamicImage) -> Result<Vec<u8>, String> {
    let mut out = Cursor::new(Vec::new());
    image
        .write_to(&mut out, ImageFormat::Png)
        .map_err(|_| "File could not be processed".to_owned())?;
    Ok(out.into_inner())
}

/// `validates_attachment_content_type :file, content_type: %r{\Aimage/.*\z}`,
/// then processing; a failure is the validation message Mastodon's form shows
/// against the setting.
pub fn validate_content_type(content_type: &str) -> Result<(), String> {
    if content_type.starts_with("image/") {
        Ok(())
    } else {
        Err("File content type is invalid".to_owned())
    }
}

/// `upload.file = file; upload.save`: store the original and its styles and
/// record them in the row for `var`, replacing what it had.
pub async fn save(
    state: &AppState,
    var: &str,
    content_type: &str,
    data: Vec<u8>,
) -> Result<SiteUpload, SaveError> {
    validate_content_type(content_type).map_err(SaveError::Invalid)?;
    let (var_owned, ct) = (var.to_owned(), content_type.to_owned());
    let size = data.len();
    let rendered = crate::tenants::spawn_blocking(move || {
        let rendered = render(&var_owned, &ct, &data);
        rendered.map(|r| (r, data))
    })
    .await
    .map_err(|e| SaveError::Internal(AppError::Internal(e.into())))?
    .map_err(SaveError::Invalid)?;
    let (rendered, data) = rendered;

    let previous = sqlx::query_as!(
        SiteUpload,
        r#"SELECT id, var, file_file_name, file_content_type, file_file_size, blurhash,
                  meta, updated_at
           FROM site_uploads WHERE var = $1"#,
        var,
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|e| SaveError::Internal(e.into()))?;

    // `Attachmentable`'s randomized file name.
    let file_name = format!(
        "{}.{}",
        &uuid::Uuid::new_v4().simple().to_string()[..16],
        rendered.original_extension
    );
    let meta = serde_json::json!({ "width": rendered.width, "height": rendered.height });
    let row = sqlx::query_as!(
        SiteUpload,
        r#"INSERT INTO site_uploads
             (var, file_file_name, file_content_type, file_file_size, file_updated_at,
              blurhash, meta, created_at, updated_at)
           VALUES ($1, $2, $3, $4, now(), $5, $6, now(), now())
           ON CONFLICT (var) DO UPDATE SET
             file_file_name = EXCLUDED.file_file_name,
             file_content_type = EXCLUDED.file_content_type,
             file_file_size = EXCLUDED.file_file_size,
             file_updated_at = EXCLUDED.file_updated_at,
             blurhash = EXCLUDED.blurhash,
             meta = EXCLUDED.meta,
             updated_at = EXCLUDED.updated_at
           RETURNING id, var, file_file_name, file_content_type, file_file_size, blurhash,
                     meta, updated_at"#,
        var,
        file_name,
        content_type,
        i32::try_from(size).unwrap_or(i32::MAX),
        rendered.blurhash,
        meta,
    )
    .fetch_one(&state.db)
    .await
    .map_err(|e| SaveError::Internal(e.into()))?;

    if let Some(key) = row.key("original") {
        state
            .storage
            .store(&data, &key, content_type)
            .await
            .map_err(SaveError::Internal)?;
    }
    for (style, bytes) in &rendered.styles {
        if let Some(key) = row.key(style) {
            state
                .storage
                .store(bytes, &key, "image/png")
                .await
                .map_err(SaveError::Internal)?;
        }
    }
    if let Some(previous) = previous {
        if previous.file_file_name != row.file_file_name {
            remove_files(state, &previous).await;
        }
    }
    Ok(row)
}

#[derive(Debug)]
pub enum SaveError {
    /// A validation message about the file.
    Invalid(String),
    Internal(AppError),
}

async fn remove_files(state: &AppState, upload: &SiteUpload) {
    for key in upload.keys() {
        if let Err(error) = state.storage.delete(&key).await {
            tracing::warn!(%error, key, "could not remove a site upload's file");
        }
    }
}

/// `SiteUpload.find(id).destroy!`: the row and its files.
pub async fn destroy(state: &AppState, id: i64) -> AppResult<()> {
    let upload = sqlx::query_as!(
        SiteUpload,
        r#"SELECT id, var, file_file_name, file_content_type, file_file_size, blurhash,
                  meta, updated_at
           FROM site_uploads WHERE id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    sqlx::query!("DELETE FROM site_uploads WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    remove_files(state, &upload).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upload(var: &str) -> SiteUpload {
        SiteUpload {
            id: 3,
            var: var.into(),
            file_file_name: Some("abcdef.jpg".into()),
            file_content_type: Some("image/jpeg".into()),
            file_file_size: Some(1),
            blurhash: None,
            meta: None,
            updated_at: chrono::NaiveDateTime::default(),
        }
    }

    #[test]
    fn styles_are_png_beside_the_original() {
        let thumbnail = upload("thumbnail");
        assert_eq!(
            thumbnail.key("@1x").unwrap(),
            "site_uploads/files/000/000/003/@1x/abcdef.png"
        );
        assert_eq!(
            thumbnail.key("original").unwrap(),
            "site_uploads/files/000/000/003/original/abcdef.jpg"
        );
        assert_eq!(upload("app_icon").keys().len(), 1 + 18);
        assert_eq!(upload("mascot").keys().len(), 1);
    }
}
