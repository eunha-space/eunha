//! Mastodon's `CustomEmoji`: where its image is kept, the URLs it is shown
//! at, and the validations an upload passes.
//!
//! The image is a Paperclip attachment, `image`, with the columns
//! `image_file_name`, `image_content_type`, `image_file_size`,
//! `image_updated_at` and `image_storage_schema_version`, kept at
//! `custom_emojis/images/:id_partition/:style/:filename` — under `cache/` for
//! a remote emoji stored with schema version 1 — in two styles: the
//! `original` as uploaded, and `static`, a PNG of its first frame.
//!
//! A remote emoji is shown from `image_remote_url`, the server it came from:
//! eunha keeps no copy of it (`remote-account-images-not-downloaded`).

use std::io::Cursor;

use image::ImageFormat;

use crate::error::{AppError, AppResult};
use crate::media::Storage;

/// `CustomEmoji::LIMIT`.
pub const LIMIT: usize = 256 * 1024;
/// `CustomEmoji::MINIMUM_SHORTCODE_SIZE`.
pub const MINIMUM_SHORTCODE_SIZE: usize = 2;
/// `CustomEmoji::MAX_SHORTCODE_SIZE`, for a local emoji.
pub const MAX_SHORTCODE_SIZE: usize = 128;
/// `CustomEmoji::MAX_FEDERATED_SHORTCODE_SIZE`.
pub const MAX_FEDERATED_SHORTCODE_SIZE: usize = 2048;
/// `CustomEmoji::IMAGE_MIME_TYPES`.
pub const IMAGE_MIME_TYPES: [&str; 3] = ["image/png", "image/gif", "image/webp"];
/// `Attachmentable::GIF_MATRIX_LIMIT`.
const GIF_MATRIX_LIMIT: u64 = 921_600;
/// `Attachmentable::MAX_MATRIX_LIMIT`.
const MAX_MATRIX_LIMIT: u64 = 33_177_600;

/// The styles an emoji's image is kept in.
pub const STYLES: [&str; 2] = ["original", "static"];

/// What of a `custom_emojis` row its image's URLs are made from.
#[derive(Debug, Clone, Copy)]
pub struct ImageRef<'a> {
    pub id: i64,
    pub domain: Option<&'a str>,
    pub image_file_name: Option<&'a str>,
    pub image_remote_url: Option<&'a str>,
    pub image_storage_schema_version: Option<i32>,
}

impl ImageRef<'_> {
    fn local(&self) -> bool {
        self.domain.is_none()
    }

    /// `image.path(style)`, relative to the bucket, when the row has a file.
    #[must_use]
    pub fn key(&self, style: &str) -> Option<String> {
        let name = self.image_file_name.filter(|n| !n.is_empty())?;
        Some(key(
            self.id,
            self.local(),
            self.image_storage_schema_version,
            style,
            name,
        ))
    }

    /// `full_asset_url(image.url(style))` for a local emoji, and the URL it
    /// was fetched from for a remote one.
    #[must_use]
    pub fn url(&self, storage: &Storage, local_domain: &str, style: &str) -> String {
        if !self.local() {
            if let Some(url) = self.image_remote_url.filter(|u| !u.is_empty()) {
                return url.to_owned();
            }
        }
        match self.key(style) {
            Some(key) => storage.public_url(&key),
            // Paperclip's `default_url`, `/:attachment/:style/missing.png`.
            None => format!("https://{local_domain}/images/{style}/missing.png"),
        }
    }
}

/// `:prefix_path:class/:attachment/:id_partition/:style/:filename`.
#[must_use]
pub fn key(
    id: i64,
    local: bool,
    storage_schema_version: Option<i32>,
    style: &str,
    file_name: &str,
) -> String {
    let prefix = if storage_schema_version.unwrap_or(0) >= 1 && !local {
        "cache/"
    } else {
        ""
    };
    format!(
        "{prefix}custom_emojis/images/{}/{style}/{}",
        crate::media::int_to_path(id),
        style_file_name(file_name, style)
    )
}

/// The `:filename` a style is kept under: the original's, and for `static`
/// its basename with the style's `png` format.
fn style_file_name(file_name: &str, style: &str) -> String {
    if style == "original" {
        return file_name.to_owned();
    }
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _)| stem);
    format!("{stem}.png")
}

/// An image ready to be stored as an emoji's: the original's bytes and type,
/// and its `static` PNG.
pub struct Processed {
    pub content_type: String,
    pub original: Vec<u8>,
    pub static_png: Vec<u8>,
    pub file_name: String,
}

/// The content type an upload is taken to have: what its bytes are, as
/// Paperclip asks `file`, or else what the client said.
#[must_use]
pub fn content_type_of(data: &[u8], declared: &str) -> String {
    match image::guess_format(data) {
        Ok(ImageFormat::Png) => "image/png".into(),
        Ok(ImageFormat::Gif) => "image/gif".into(),
        Ok(ImageFormat::WebP) => "image/webp".into(),
        Ok(ImageFormat::Jpeg) => "image/jpeg".into(),
        _ => declared.trim().to_owned(),
    }
}

/// `Attachmentable#appropriate_extension` for the image types an emoji may
/// have.
fn extension_for(content_type: &str) -> &'static str {
    match content_type {
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "png",
    }
}

/// `Attachmentable#check_image_dimension`, which raises
/// `Mastodon::DimensionsValidationError` before any validation runs.
pub fn check_dimensions(data: &[u8], content_type: &str) -> AppResult<()> {
    let Ok(reader) = image::ImageReader::new(Cursor::new(data)).with_guessed_format() else {
        return Ok(());
    };
    let Ok((width, height)) = reader.into_dimensions() else {
        return Ok(());
    };
    let pixels = u64::from(width) * u64::from(height);
    if content_type == "image/gif" && pixels > GIF_MATRIX_LIMIT {
        return Err(AppError::Unprocessable(format!(
            "{width}x{height} GIF files are not supported"
        )));
    }
    if pixels > MAX_MATRIX_LIMIT {
        return Err(AppError::Unprocessable(format!(
            "{width}x{height} images are not supported"
        )));
    }
    Ok(())
}

/// The `static` style: the first frame, coalesced, as a PNG without the
/// original's metadata.
pub fn static_png(data: &[u8]) -> Option<Vec<u8>> {
    let image = image::load_from_memory(data).ok()?;
    let mut out = Vec::new();
    image
        .to_rgba8()
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .ok()?;
    Some(out)
}

/// `SecureRandom.hex(8)` and the extension the type calls for, as
/// `Attachmentable#obfuscate_file_name` and `#set_file_extension` name an
/// upload.
#[must_use]
pub fn obfuscated_file_name(content_type: &str) -> String {
    let bytes: [u8; 8] = rand::random();
    format!("{}.{}", hex::encode(bytes), extension_for(content_type))
}

/// The errors `CustomEmoji#valid?` finds in an image, as full messages, in
/// the order `validates_attachment :image, content_type:, presence:, size:`
/// adds them.
#[must_use]
pub fn image_errors(image: Option<(&[u8], &str)>) -> Vec<String> {
    let mut errors = Vec::new();
    let Some((data, content_type)) = image else {
        errors.push("Image can't be blank".into());
        return errors;
    };
    if !IMAGE_MIME_TYPES.contains(&content_type) {
        errors.push("Image content type is invalid".into());
        errors.push("Image is invalid".into());
    }
    if data.len() >= LIMIT {
        errors.push("Image file size must be less than 256 KB".into());
        errors.push("Image must be less than 256 KB".into());
    }
    errors
}

/// The errors `validates :shortcode` finds, but for uniqueness, which takes
/// the database: format, then length, then a local emoji's own maximum.
#[must_use]
pub fn shortcode_errors(shortcode: &str, local: bool) -> Vec<String> {
    let mut errors = Vec::new();
    let valid_format = shortcode.chars().count() >= MINIMUM_SHORTCODE_SIZE
        && shortcode
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid_format {
        errors.push("Shortcode is invalid".into());
    }
    let length = shortcode.chars().count();
    if length < MINIMUM_SHORTCODE_SIZE {
        errors.push(format!(
            "Shortcode is too short (minimum is {MINIMUM_SHORTCODE_SIZE} characters)"
        ));
    } else if length > MAX_FEDERATED_SHORTCODE_SIZE {
        errors.push(format!(
            "Shortcode is too long (maximum is {MAX_FEDERATED_SHORTCODE_SIZE} characters)"
        ));
    }
    if local && length > MAX_SHORTCODE_SIZE {
        errors.push(format!(
            "Shortcode is too long (maximum is {MAX_SHORTCODE_SIZE} characters)"
        ));
    }
    errors
}

/// Process an upload into what is stored: the original as it came, and its
/// static PNG.
pub fn process(data: Vec<u8>, content_type: &str) -> AppResult<Processed> {
    let static_png = static_png(&data).ok_or_else(|| {
        AppError::Unprocessable("Validation failed: Image could not be processed".into())
    })?;
    Ok(Processed {
        content_type: content_type.to_owned(),
        file_name: obfuscated_file_name(content_type),
        original: data,
        static_png,
    })
}

/// Store both styles of a processed image for the local emoji `id`.
pub async fn store(storage: &Storage, id: i64, image: &Processed) -> AppResult<()> {
    storage
        .store(
            &image.original,
            &key(id, true, Some(1), "original", &image.file_name),
            &image.content_type,
        )
        .await?;
    storage
        .store(
            &image.static_png,
            &key(id, true, Some(1), "static", &image.file_name),
            "image/png",
        )
        .await?;
    Ok(())
}

/// Remove every style of an emoji's image, as Paperclip does when the row is
/// destroyed or its image replaced.
pub async fn delete_files(storage: &Storage, image: ImageRef<'_>) {
    for style in STYLES {
        if let Some(key) = image.key(style) {
            let _ = storage.delete(&key).await;
        }
    }
}

/// What [`move_eunha_uploads`] did.
#[derive(Debug, Default)]
pub struct MoveReport {
    pub moved: u64,
    /// Rows whose image could not be read where eunha had put it.
    pub missing: Vec<(i64, String)>,
}

/// How many local emojis [`move_eunha_uploads`] has to move.
pub async fn eunha_uploads_waiting(db: &sqlx::PgPool) -> AppResult<i64> {
    Ok(sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM custom_emojis
           WHERE domain IS NULL AND image_file_name IS NULL
             AND image_remote_url IS NOT NULL AND image_remote_url <> ''"#,
    )
    .fetch_one(db)
    .await?)
}

/// Move the local emojis eunha uploaded before it stored them as Mastodon
/// does — at `emoji/<shortcode>.<ext>`, with the URL in `image_remote_url`
/// and no file columns — to Paperclip's layout, with a `static` style, and
/// write the columns Mastodon reads. Running it again changes nothing.
pub async fn move_eunha_uploads(db: &sqlx::PgPool, storage: &Storage) -> AppResult<MoveReport> {
    let rows = sqlx::query!(
        r#"SELECT id, image_remote_url AS "image_remote_url!"
           FROM custom_emojis
           WHERE domain IS NULL AND image_file_name IS NULL
             AND image_remote_url IS NOT NULL AND image_remote_url <> ''
           ORDER BY id"#,
    )
    .fetch_all(db)
    .await?;
    let mut report = MoveReport::default();
    for row in rows {
        let Some(old_key) = eunha_upload_key(&row.image_remote_url) else {
            report.missing.push((row.id, row.image_remote_url));
            continue;
        };
        let data = match storage.get(&old_key).await {
            Ok(data) if !data.is_empty() => data,
            _ => {
                report.missing.push((row.id, row.image_remote_url));
                continue;
            }
        };
        let content_type = content_type_of(&data, "");
        let Ok(image) = process(data, &content_type) else {
            report.missing.push((row.id, row.image_remote_url));
            continue;
        };
        store(storage, row.id, &image).await?;
        sqlx::query!(
            r#"UPDATE custom_emojis
               SET image_file_name = $2, image_content_type = $3, image_file_size = $4,
                   image_updated_at = now(), image_storage_schema_version = 1,
                   image_remote_url = NULL
               WHERE id = $1"#,
            row.id,
            image.file_name,
            image.content_type,
            image.original.len() as i32,
        )
        .execute(db)
        .await?;
        let _ = storage.delete(&old_key).await;
        report.moved += 1;
    }
    Ok(report)
}

/// The key eunha uploaded an emoji under, read off the URL it stored:
/// `emoji/<file>` at the end of its path.
fn eunha_upload_key(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let (rest, file) = path.rsplit_once('/')?;
    if !rest.ends_with("/emoji") || file.is_empty() {
        return None;
    }
    Some(format!("emoji/{file}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_emoji_live_where_paperclip_puts_them() {
        assert_eq!(
            key(7, true, Some(1), "original", "0123456789abcdef.gif"),
            "custom_emojis/images/000/000/007/original/0123456789abcdef.gif"
        );
        assert_eq!(
            key(7, true, Some(1), "static", "0123456789abcdef.gif"),
            "custom_emojis/images/000/000/007/static/0123456789abcdef.png"
        );
        // A remote emoji stored with schema version 1 is a cached copy.
        assert_eq!(
            key(7, false, Some(1), "original", "a.png"),
            "cache/custom_emojis/images/000/000/007/original/a.png"
        );
        assert_eq!(
            key(7, false, None, "original", "a.png"),
            "custom_emojis/images/000/000/007/original/a.png"
        );
    }

    #[test]
    fn shortcodes_are_validated_as_mastodon_validates_them() {
        assert!(shortcode_errors("blob_cat", true).is_empty());
        assert_eq!(
            shortcode_errors("a", true),
            [
                "Shortcode is invalid",
                "Shortcode is too short (minimum is 2 characters)"
            ]
        );
        assert_eq!(shortcode_errors("no-dash", true), ["Shortcode is invalid"]);
        let long = "a".repeat(129);
        assert_eq!(
            shortcode_errors(&long, true),
            ["Shortcode is too long (maximum is 128 characters)"]
        );
        assert!(shortcode_errors(&long, false).is_empty());
    }

    #[test]
    fn images_are_validated_as_paperclip_validates_them() {
        assert_eq!(image_errors(None), ["Image can't be blank"]);
        assert!(image_errors(Some((b"x", "image/png"))).is_empty());
        assert_eq!(
            image_errors(Some((b"x", "image/jpeg"))),
            ["Image content type is invalid", "Image is invalid"]
        );
        let big = vec![0; LIMIT];
        assert_eq!(
            image_errors(Some((&big, "image/gif"))),
            [
                "Image file size must be less than 256 KB",
                "Image must be less than 256 KB"
            ]
        );
    }

    #[test]
    fn eunha_uploads_are_recognised_by_their_path() {
        assert_eq!(
            eunha_upload_key("https://files.example/prefix/emoji/blob.png").as_deref(),
            Some("emoji/blob.png")
        );
        assert_eq!(
            eunha_upload_key("https://remote.example/emoji/blob.png?v=1").as_deref(),
            Some("emoji/blob.png")
        );
        assert_eq!(
            eunha_upload_key("https://files.example/custom_emojis/images/a.png"),
            None
        );
    }

    #[test]
    fn the_static_style_is_a_png() {
        let mut gif = Vec::new();
        image::DynamicImage::new_rgba8(4, 4)
            .write_to(&mut Cursor::new(&mut gif), ImageFormat::Gif)
            .unwrap();
        let png = static_png(&gif).unwrap();
        assert_eq!(image::guess_format(&png).unwrap(), ImageFormat::Png);
    }
}
