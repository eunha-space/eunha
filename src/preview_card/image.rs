//! A preview card's image, as `PreviewCard`'s Paperclip attachment keeps it:
//! downloaded by `remotable_attachment :image, 8.megabytes`, shrunk by
//! `Paperclip::LazyThumbnail` to at most 230,400 pixels, a GIF turned into a
//! JPEG, and blurhashed by `Paperclip::BlurhashTranscoder`.

use image::ImageFormat;
use url::Url;

use crate::media::picture::{Fit, Picture};
use crate::state::AppState;

/// `PreviewCard::LIMIT`.
pub const LIMIT: usize = 8 * 1024 * 1024;

/// The `original` style's `pixels:` (640x360).
const PIXELS: u64 = 230_400;

/// `Attachmentable::GIF_MATRIX_LIMIT` and `MAX_MATRIX_LIMIT`.
const GIF_MATRIX_LIMIT: u64 = 921_600;
const MAX_MATRIX_LIMIT: u64 = 33_177_600;

/// What assigning `image_remote_url` does to the card's image.
pub enum Outcome {
    /// No URL, or not one that could be fetched: the image the card has, if
    /// any, stays.
    Keep,
    /// The download or the image failed: the card loses its image.
    Clear,
    /// A new image, ready to store.
    New(Processed),
}

/// An image as it will be stored.
pub struct Processed {
    pub bytes: Vec<u8>,
    /// What the stored bytes are, for the object's `Content-Type`.
    pub stored_content_type: &'static str,
    /// `image_content_type`: what was downloaded. Paperclip keeps a GIF's type
    /// after its original style has been converted to a JPEG.
    pub content_type: &'static str,
    /// `image_file_name`.
    pub file_name: String,
    pub width: u32,
    pub height: u32,
    pub blurhash: Option<String>,
}

/// `download_image!(url)` followed by the attachment's processing.
pub async fn fetch(state: &AppState, url: Option<&str>) -> Outcome {
    let Some(url) = url.filter(|u| !super::is_blank(u)) else {
        return Outcome::Keep;
    };
    let Ok(parsed) = Url::parse(url) else {
        return Outcome::Keep;
    };
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none_or(str::is_empty) {
        return Outcome::Keep;
    }
    if crate::federation::safe_fetch::validate_url(url).is_err() {
        return Outcome::Clear;
    }
    let Ok(response) = state.fetch.get(url).timeout(super::TIMEOUT).send().await else {
        return Outcome::Clear;
    };
    if !response.status().is_success() {
        return Outcome::Clear;
    }
    let original_filename = original_filename(&response);
    let Ok(bytes) = super::read_body(response, LIMIT, false).await else {
        return Outcome::Clear;
    };
    match crate::tenants::spawn_blocking(move || process(&bytes, &original_filename)).await {
        Ok(Some(processed)) => Outcome::New(processed),
        _ => Outcome::Clear,
    }
}

/// `ResponseWithLimitAdapter#truncated_filename`, reduced to the extension
/// that is all of it Paperclip keeps.
fn original_filename(response: &reqwest::Response) -> String {
    static DISPOSITION: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#"filename="([^"]*)""#).unwrap());
    response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| DISPOSITION.captures(v))
        .map(|c| c[1].to_owned())
        .filter(|f| !f.is_empty())
        .or_else(|| {
            response
                .url()
                .path()
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "data".to_owned())
}

/// Ruby's `File.extname`, without its dot.
fn extension(filename: &str) -> &str {
    match filename.rfind('.') {
        Some(0) | None => "",
        Some(i) => &filename[i + 1..],
    }
}

/// `Attachmentable#appropriate_extension`: the file's own extension when
/// its type allows it, else the type's first.
fn appropriate_extension(content_type: &str, original: &str) -> &'static str {
    let allowed: &[&'static str] = match content_type {
        "image/jpeg" => &["jpeg", "jpg", "jpe", "jfif"],
        "image/png" => &["png"],
        "image/gif" => &["gif"],
        "image/webp" => &["webp"],
        _ => &[],
    };
    let chosen = allowed
        .iter()
        .find(|e| **e == original)
        .or_else(|| allowed.first())
        .copied()
        .unwrap_or("");
    if matches!(chosen, "jpe" | "jfif") {
        "jpeg"
    } else {
        chosen
    }
}

/// Validate and process a downloaded image. `None` when Mastodon would
/// have refused it: not a JPEG, PNG, GIF or WebP, too large, or too many
/// pixels.
///
/// CPU-bound: call it from the blocking pool.
pub fn process(bytes: &[u8], original_filename: &str) -> Option<Processed> {
    if bytes.len() >= LIMIT {
        return None;
    }
    let format = image::guess_format(bytes).ok()?;
    let content_type = match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Png => "image/png",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => return None,
    };
    let (width, height) = image::ImageReader::with_format(std::io::Cursor::new(bytes), format)
        .into_dimensions()
        .ok()?;
    let pixels = u64::from(width) * u64::from(height);
    if (format == ImageFormat::Gif && pixels > GIF_MATRIX_LIMIT) || pixels > MAX_MATRIX_LIMIT {
        return None;
    }
    let picture = Picture::decode(bytes)?;

    // `LazyThumbnail#needs_convert?`: too many pixels, or a GIF, which the
    // style turns into a JPEG. Otherwise the file is stored as it came.
    let (stored, stored_content_type, image) = if pixels > PIXELS || format == ImageFormat::Gif {
        let target = if format == ImageFormat::Gif {
            ImageFormat::Jpeg
        } else {
            format
        };
        let rendition = picture.rendition(Fit::Pixels(PIXELS), target)?;
        let stored_content_type = rendition.content_type();
        (rendition.bytes, stored_content_type, rendition.image)
    } else {
        (bytes.to_vec(), content_type, picture.image().clone())
    };

    // `BlurhashTranscoder`: four by four components, from a thumbnail of
    // the stored image 100 pixels across.
    let thumbnail = image.thumbnail(100, 100);
    let blurhash = blurhash::encode(
        4,
        4,
        thumbnail.width(),
        thumbnail.height(),
        thumbnail.to_rgba8().as_raw(),
    )
    .ok();

    let file_name = format!(
        "{}.{}",
        hex::encode(rand::random::<[u8; 8]>()),
        appropriate_extension(content_type, extension(original_filename)),
    );
    Some(Processed {
        width: image.width(),
        height: image.height(),
        bytes: stored,
        stored_content_type,
        content_type,
        file_name,
        blurhash,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};

    fn encoded(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
        let image = DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        }));
        let mut out = std::io::Cursor::new(Vec::new());
        image.write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    #[test]
    fn a_small_image_is_stored_as_it_came() {
        let png = encoded(64, 32, ImageFormat::Png);
        let p = process(&png, "cover.PNG").unwrap();
        assert_eq!(p.bytes, png);
        assert_eq!((p.width, p.height), (64, 32));
        assert_eq!(p.content_type, "image/png");
        assert!(p.file_name.ends_with(".png"), "{}", p.file_name);
        assert_eq!(p.file_name.len(), 16 + 4);
        assert!(p.blurhash.is_some());
    }

    #[test]
    fn a_large_image_is_shrunk_to_640_by_360_worth_of_pixels() {
        let jpeg = encoded(1280, 720, ImageFormat::Jpeg);
        let p = process(&jpeg, "photo.jpg").unwrap();
        assert_eq!((p.width, p.height), (640, 360));
        assert_eq!(p.stored_content_type, "image/jpeg");
        assert!(p.file_name.ends_with(".jpg"));
    }

    #[test]
    fn a_gif_becomes_a_jpeg_but_keeps_its_name_and_type() {
        let gif = encoded(20, 10, ImageFormat::Gif);
        let p = process(&gif, "anim").unwrap();
        assert_eq!(p.content_type, "image/gif");
        assert_eq!(p.stored_content_type, "image/jpeg");
        assert!(p.file_name.ends_with(".gif"));
        assert_eq!(image::guess_format(&p.bytes).unwrap(), ImageFormat::Jpeg);
    }

    #[test]
    fn other_files_and_huge_gifs_are_refused() {
        assert!(process(b"<html></html>", "x.html").is_none());
        assert!(process(&encoded(1281, 720, ImageFormat::Gif), "x.gif").is_none());
    }

    #[test]
    fn extensions_follow_paperclip() {
        assert_eq!(appropriate_extension("image/jpeg", "jpg"), "jpg");
        assert_eq!(appropriate_extension("image/jpeg", "jfif"), "jpeg");
        assert_eq!(appropriate_extension("image/jpeg", "png"), "jpeg");
        assert_eq!(appropriate_extension("image/webp", ""), "webp");
        assert_eq!(extension("a.b.jpg"), "jpg");
        assert_eq!(extension(".hidden"), "");
    }
}
