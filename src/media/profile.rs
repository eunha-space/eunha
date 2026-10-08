//! A local account's avatar and header, as `Account::Avatar` and
//! `Account::Header` take and store them: only JPEG, PNG, GIF and WebP under
//! 8 MB, fitted by their Paperclip styles, and a GIF's first frame kept as a
//! PNG `static` style besides.

use crate::email_subscriptions::ValidationErrors;
use crate::media::picture::{Fit, Picture};
use image::ImageFormat;

/// `AVATAR_IMAGE_MIME_TYPES` and `HEADER_IMAGE_MIME_TYPES`.
pub const MIME_TYPES: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp"];
/// `AVATAR_LIMIT` and `HEADER_LIMIT`.
pub const LIMIT: usize = 8 * 1024 * 1024;
/// The body a request carrying both may need, with room to be refused by
/// [`validate`] rather than by the server.
pub const BODY_LIMIT: usize = 2 * LIMIT + 1024 * 1024;
/// `Account::Avatar::MAX_DESCRIPTION_LENGTH` and the header's.
pub const MAX_DESCRIPTION_LENGTH: usize = 150;

/// Which of the two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Avatar,
    Header,
}

/// `AVATAR_DIMENSIONS`: 400×400, cropped (`400x400#`).
const AVATAR_SIDE: u32 = 400;
/// `HEADER_MAX_PIXELS`: 1500×500.
const HEADER_PIXELS: u64 = 750_000;

impl Kind {
    fn fit(self) -> Fit {
        match self {
            Kind::Avatar => Fit::Cover(AVATAR_SIDE, AVATAR_SIDE),
            Kind::Header => Fit::Pixels(HEADER_PIXELS),
        }
    }

    /// The attribute names the validations file their errors under.
    fn attributes(self) -> [&'static str; 4] {
        match self {
            Kind::Avatar => [
                "avatar",
                "avatar_content_type",
                "avatar_file_size",
                "avatar_description",
            ],
            Kind::Header => [
                "header",
                "header_content_type",
                "header_file_size",
                "header_description",
            ],
        }
    }

    /// `avatar_styles` and `header_styles`'s `static`: where its PNG is
    /// kept, beside the original at `original_key`.
    pub fn static_key(original_key: &str) -> String {
        let stem = original_key
            .rsplit_once('.')
            .map_or(original_key, |(stem, _)| stem);
        format!("{}.png", stem.replacen("/original/", "/static/", 1))
    }
}

/// A content type without its parameters, lowercased.
pub fn essence(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// `validates_attachment_content_type` and `validates_attachment_size`,
/// which kt-paperclip files under the attachment's own name as well.
pub fn validate(kind: Kind, content_type: &str, size: usize, errors: &mut ValidationErrors) {
    let [base, content_type_attr, size_attr, _] = kind.attributes();
    if !MIME_TYPES.contains(&essence(content_type).as_str()) {
        errors.add(content_type_attr, "invalid", "is invalid");
        errors.add(base, "invalid", "is invalid");
    }
    if size >= LIMIT {
        errors.add(size_attr, "less_than", "must be less than 8 MB");
        errors.add(base, "less_than", "must be less than 8 MB");
    }
}

/// `validates :avatar_description, length: { maximum: 150 }`.
pub fn validate_description(kind: Kind, description: &str, errors: &mut ValidationErrors) {
    if description.chars().count() > MAX_DESCRIPTION_LENGTH {
        errors.add(
            kind.attributes()[3],
            "too_long",
            "is too long (maximum is 150 characters)",
        );
    }
}

/// `FastGeometryParser` failing to read the file, which
/// `post_process_style` records against the attachment.
pub fn not_identified(kind: Kind, errors: &mut ValidationErrors) {
    errors.add(
        kind.attributes()[0],
        "invalid",
        "Paperclip::Errors::NotIdentifiedByImageMagickError",
    );
}

/// An avatar or header as it is stored.
pub struct Stored {
    pub bytes: Vec<u8>,
    /// What `bytes` are, read from them rather than from what the client
    /// said they were.
    pub content_type: &'static str,
    /// The PNG of a GIF's first frame, for its `static` style.
    pub static_png: Option<Vec<u8>>,
}

/// `Paperclip::LazyThumbnail` with the `original` style (and, for a GIF, the
/// `static` one): upright, fitted and without its metadata. `None` when the
/// bytes are not an image this build reads, which is never stored.
pub async fn process(kind: Kind, data: Vec<u8>) -> anyhow::Result<Option<Stored>> {
    let fit = kind.fit();
    let decoded = crate::tenants::spawn_blocking(move || {
        let picture = Picture::decode(&data)?;
        if picture.format() == ImageFormat::Gif {
            let static_png = picture.rendition(fit, ImageFormat::Png)?.bytes;
            return Some((
                data,
                picture.width(),
                picture.height(),
                None,
                Some(static_png),
            ));
        }
        let stored = picture.original(&data, fit)?;
        let content_type = stored.content_type();
        Some((
            stored.bytes,
            picture.width(),
            picture.height(),
            Some(content_type),
            None,
        ))
    })
    .await
    .map_err(|e| anyhow::anyhow!("image processing did not finish: {e}"))?;
    let Some((bytes, width, height, content_type, static_png)) = decoded else {
        return Ok(None);
    };
    if let Some(content_type) = content_type {
        return Ok(Some(Stored {
            bytes,
            content_type,
            static_png: None,
        }));
    }
    // A GIF keeps its animation: ffmpeg, as `LazyThumbnail#make` runs it.
    let filter = gif_filter(kind, width, height);
    let bytes = crate::media::transcode::gif(&bytes, &filter).await?;
    Ok(Some(Stored {
        bytes,
        content_type: "image/gif",
        static_png,
    }))
}

/// `LazyThumbnail#make`'s filter for a GIF of `width`×`height`: shrunk to
/// the style's geometry, never enlarged, and an avatar cropped square.
fn gif_filter(kind: Kind, width: u32, height: u32) -> String {
    let (w, h) = (f64::from(width), f64::from(height));
    let (target_w, target_h) = match kind {
        Kind::Avatar => (f64::from(AVATAR_SIDE), f64::from(AVATAR_SIDE)),
        // `PixelGeometryParser`.
        Kind::Header => {
            let pixels = HEADER_PIXELS as f64;
            (
                (pixels * (w / h)).sqrt().round(),
                (pixels * (h / w)).sqrt().round(),
            )
        }
    };
    let scaled = if w <= target_w && h <= target_h {
        None
    } else {
        let scale = (target_w / w).min(target_h / h);
        Some(((w * scale).round() as u32, (h * scale).round() as u32))
    };
    match (kind, scaled) {
        (Kind::Avatar, None) => {
            "scale=iw:ih:force_original_aspect_ratio=increase,crop='min(iw,ih)':'min(iw,ih)'"
                .to_owned()
        }
        (Kind::Avatar, Some((tw, th))) => {
            let side = tw.min(th);
            format!("scale={tw}:{th}:force_original_aspect_ratio=increase,crop={side}:{side}")
        }
        (Kind::Header, None) => "scale=iw:ih:force_original_aspect_ratio=decrease".to_owned(),
        (Kind::Header, Some((tw, th))) => {
            format!("scale={tw}:{th}:force_original_aspect_ratio=decrease")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gif_avatar_is_shrunk_and_cropped_square() {
        assert_eq!(
            gif_filter(Kind::Avatar, 800, 400),
            "scale=400:200:force_original_aspect_ratio=increase,crop=200:200"
        );
        assert_eq!(
            gif_filter(Kind::Avatar, 300, 200),
            "scale=iw:ih:force_original_aspect_ratio=increase,crop='min(iw,ih)':'min(iw,ih)'"
        );
    }

    #[test]
    fn a_gif_header_is_shrunk_to_its_pixels() {
        assert_eq!(
            gif_filter(Kind::Header, 3000, 1000),
            "scale=1500:500:force_original_aspect_ratio=decrease"
        );
        assert_eq!(
            gif_filter(Kind::Header, 600, 200),
            "scale=iw:ih:force_original_aspect_ratio=decrease"
        );
    }

    #[test]
    fn the_static_style_is_a_png_beside_the_original() {
        assert_eq!(
            Kind::static_key("accounts/avatars/000/000/001/original/abc.gif"),
            "accounts/avatars/000/000/001/static/abc.png"
        );
    }

    #[test]
    fn only_four_types_under_eight_megabytes() {
        let mut errors = ValidationErrors::default();
        validate(Kind::Avatar, "image/svg+xml", LIMIT, &mut errors);
        assert_eq!(
            errors.message(),
            "Validation failed: Avatar content type is invalid, Avatar is invalid, \
             Avatar file size must be less than 8 MB, Avatar must be less than 8 MB"
        );
        let mut errors = ValidationErrors::default();
        validate(Kind::Header, "image/webp", LIMIT - 1, &mut errors);
        assert!(errors.is_empty());
    }
}
