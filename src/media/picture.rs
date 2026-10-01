//! Local image uploads, left the way Mastodon's `Paperclip::LazyThumbnail`
//! leaves them.
//!
//! Mastodon never stores what a local account uploads. Every style — a media
//! attachment's original and small, an avatar, a header — goes through
//! `Vips::Image.thumbnail`, which turns the image upright by its EXIF
//! orientation, and then everything but the ICC profile is removed. Doing only
//! the second half is worse than doing neither: a phone stores a portrait photo
//! as landscape pixels and a tag saying to turn them, so dropping the tag
//! without turning the pixels serves the photo on its side.
//!
//! Re-encoding also takes whatever else a file carries along — GPS in XMP as
//! well as EXIF, IPTC, comments, a motion photo's video after the JPEG's end —
//! which is why the original is re-encoded even when nothing about its pixels
//! changes. The one exception is a WebP that needs no transform: the encoder
//! here writes only lossless WebP, which would multiply a lossy photo's size,
//! so its metadata chunks are removed instead.

use std::io::Cursor;

use image::{
    codecs::{jpeg::JpegEncoder, png::PngEncoder, webp::WebPEncoder},
    imageops::FilterType,
    metadata::Orientation,
    DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader,
};

/// `Vips::Image.thumbnail`'s default kernel.
const FILTER: FilterType = FilterType::Lanczos3;

/// Mastodon's JPEG quality (`Q: 90`, and `-quality 90` before libvips).
const JPEG_QUALITY: u8 = 90;

/// How a style sizes its image.
#[derive(Clone, Copy, Debug)]
pub enum Fit {
    /// Shrink to at most this many pixels, keeping the aspect ratio; never
    /// enlarge. A style's `pixels:`.
    Pixels(u64),
    /// Scale and centre-crop to exactly this size. A style's `WxH#` geometry.
    Cover(u32, u32),
}

/// One stored rendition of an upload.
pub struct Rendition {
    pub bytes: Vec<u8>,
    pub format: ImageFormat,
    /// The pixels `bytes` encodes, for blurhashing.
    pub image: DynamicImage,
}

impl Rendition {
    pub fn width(&self) -> u32 {
        self.image.width()
    }

    pub fn height(&self) -> u32 {
        self.image.height()
    }

    pub fn content_type(&self) -> &'static str {
        self.format.to_mime_type()
    }
}

/// A decoded upload, already upright.
pub struct Picture {
    image: DynamicImage,
    format: ImageFormat,
    icc_profile: Option<Vec<u8>>,
    rotated: bool,
}

impl Picture {
    /// Decode an upload and apply its orientation. `None` when the bytes are
    /// not an image this build can decode.
    ///
    /// CPU-bound: call it from the blocking pool, never on a Tokio worker.
    pub fn decode(data: &[u8]) -> Option<Self> {
        let reader = ImageReader::new(Cursor::new(data))
            .with_guessed_format()
            .ok()?;
        let format = reader.format()?;
        let mut decoder = reader.into_decoder().ok()?;
        let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
        let icc_profile = decoder.icc_profile().ok().flatten();
        let mut image = DynamicImage::from_decoder(decoder).ok()?;
        image.apply_orientation(orientation);
        Some(Self {
            image,
            format,
            icc_profile,
            rotated: orientation != Orientation::NoTransforms,
        })
    }

    pub fn width(&self) -> u32 {
        self.image.width()
    }

    pub fn height(&self) -> u32 {
        self.image.height()
    }

    /// The upright pixels.
    pub fn image(&self) -> &DynamicImage {
        &self.image
    }

    /// The format the bytes were in.
    pub fn format(&self) -> ImageFormat {
        self.format
    }

    /// The upload as it should be stored in place of what was sent: upright,
    /// fitted, in its own format and without its metadata.
    ///
    /// `None` for a format whose re-encoding would lose what matters about it —
    /// an animated GIF avatar would stop moving — so the caller stores the
    /// upload as it came. GIF has no orientation to apply.
    pub fn original(&self, source: &[u8], fit: Fit) -> Option<Rendition> {
        let fitted = fitted(&self.image, fit);
        match self.format {
            ImageFormat::Jpeg | ImageFormat::Png => {
                self.encode(fitted.unwrap_or_else(|| self.image.clone()), self.format)
            }
            ImageFormat::WebP => match fitted {
                None if !self.rotated => Some(Rendition {
                    bytes: strip_webp_metadata(source)?,
                    format: ImageFormat::WebP,
                    image: self.image.clone(),
                }),
                fitted => self.encode(fitted.unwrap_or_else(|| self.image.clone()), self.format),
            },
            _ => None,
        }
    }

    /// A derived rendition — a thumbnail — fitted and encoded as `format`.
    pub fn rendition(&self, fit: Fit, format: ImageFormat) -> Option<Rendition> {
        self.encode(
            fitted(&self.image, fit).unwrap_or_else(|| self.image.clone()),
            format,
        )
    }

    /// The format a thumbnail keeps: the upload's own, as Mastodon's small
    /// style does, where this build can write it.
    pub fn thumbnail_format(&self) -> ImageFormat {
        match self.format {
            ImageFormat::Png | ImageFormat::WebP => self.format,
            _ => ImageFormat::Jpeg,
        }
    }

    fn encode(&self, image: DynamicImage, format: ImageFormat) -> Option<Rendition> {
        let mut bytes = Vec::new();
        let icc_profile = self.icc_profile.clone();
        let written = match format {
            ImageFormat::Jpeg => write(
                &image,
                JpegEncoder::new_with_quality(&mut bytes, JPEG_QUALITY),
                icc_profile,
            ),
            ImageFormat::Png => write(&image, PngEncoder::new(&mut bytes), icc_profile),
            ImageFormat::WebP => write(&image, WebPEncoder::new_lossless(&mut bytes), icc_profile),
            _ => return None,
        };
        written.then_some(Rendition {
            bytes,
            format,
            image,
        })
    }
}

fn write(
    image: &DynamicImage,
    mut encoder: impl ImageEncoder,
    icc_profile: Option<Vec<u8>>,
) -> bool {
    // A profile the encoder refuses leaves the colours slightly off, which is
    // no reason to refuse the upload.
    if let Some(icc_profile) = icc_profile {
        let _ = encoder.set_icc_profile(icc_profile);
    }
    image.write_with_encoder(encoder).is_ok()
}

/// `image` resized for `fit`, or `None` when it already fits.
fn fitted(image: &DynamicImage, fit: Fit) -> Option<DynamicImage> {
    let (width, height) = (image.width(), image.height());
    match fit {
        Fit::Pixels(limit) => {
            let pixels = u64::from(width) * u64::from(height);
            if pixels <= limit {
                return None;
            }
            // Mastodon's `PixelGeometryParser`.
            let scale = (limit as f64 / pixels as f64).sqrt();
            let target_width = ((f64::from(width) * scale).round() as u32).max(1);
            let target_height = ((f64::from(height) * scale).round() as u32).max(1);
            Some(image.resize_exact(target_width, target_height, FILTER))
        }
        Fit::Cover(target_width, target_height) => {
            if (width, height) == (target_width, target_height) {
                return None;
            }
            Some(image.resize_to_fill(target_width, target_height, FILTER))
        }
    }
}

/// Remove the metadata from an upload this build could not decode, where its
/// container can at least be read. Nothing is turned upright — there are no
/// pixels to turn — but nothing it carries is published either.
pub fn strip_metadata(data: &[u8]) -> Vec<u8> {
    let stripped = match image::guess_format(data) {
        Ok(ImageFormat::Jpeg) => strip_jpeg_metadata(data),
        Ok(ImageFormat::Png) => strip_png_metadata(data),
        Ok(ImageFormat::WebP) => strip_webp_metadata(data),
        _ => None,
    };
    stripped.unwrap_or_else(|| data.to_vec())
}

fn strip_jpeg_metadata(data: &[u8]) -> Option<Vec<u8>> {
    use img_parts::jpeg::{markers, Jpeg};

    let mut jpeg = Jpeg::from_bytes(data.to_vec().into()).ok()?;
    // APP1 is both EXIF and XMP; APP13 is Photoshop's IPTC. APP2 carries the
    // ICC profile and APP14 Adobe's colour transform, which decoding needs.
    for marker in [markers::APP1, markers::APP13, markers::COM] {
        jpeg.remove_segments_by_marker(marker);
    }
    Some(jpeg.encoder().bytes().to_vec())
}

fn strip_png_metadata(data: &[u8]) -> Option<Vec<u8>> {
    use img_parts::png::Png;

    let mut png = Png::from_bytes(data.to_vec().into()).ok()?;
    for kind in [*b"eXIf", *b"tEXt", *b"zTXt", *b"iTXt", *b"tIME"] {
        png.remove_chunks_by_type(kind);
    }
    Some(png.encoder().bytes().to_vec())
}

/// Drop a WebP's `EXIF` and `XMP ` chunks and clear the `VP8X` flags that
/// announce them, leaving the bitstream — and any alpha or animation — as it
/// was. Written by hand because `img_parts` drops the `VP8X` chunk of an
/// image that has alpha or animation but no ICC profile.
fn strip_webp_metadata(data: &[u8]) -> Option<Vec<u8>> {
    const VP8X_EXIF: u8 = 0x08;
    const VP8X_XMP: u8 = 0x04;

    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WEBP" {
        return None;
    }
    let riff_end = usize::try_from(u32::from_le_bytes(data[4..8].try_into().ok()?))
        .ok()?
        .checked_add(8)?
        .min(data.len());

    let mut out = Vec::with_capacity(riff_end);
    out.extend_from_slice(b"RIFF\0\0\0\0WEBP");
    let mut at = 12;
    while at + 8 <= riff_end {
        let id = &data[at..at + 4];
        let size =
            usize::try_from(u32::from_le_bytes(data[at + 4..at + 8].try_into().ok()?)).ok()?;
        let payload_end = (at + 8).checked_add(size)?;
        if payload_end > riff_end {
            return None;
        }
        // Chunks are padded to an even length; a writer may omit the last pad.
        let end = (payload_end + (size & 1)).min(riff_end);
        match id {
            b"EXIF" | b"XMP " => {}
            b"VP8X" if size >= 1 => {
                let start = out.len();
                out.extend_from_slice(&data[at..end]);
                out[start + 8] &= !(VP8X_EXIF | VP8X_XMP);
            }
            _ => out.extend_from_slice(&data[at..end]),
        }
        at = end;
    }
    let riff_size = u32::try_from(out.len() - 8).ok()?;
    out[4..8].copy_from_slice(&riff_size.to_le_bytes());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    /// A TIFF-structured EXIF block holding only an Orientation tag.
    fn exif_orientation(value: u8) -> Vec<u8> {
        let mut exif = b"MM\0\x2a\0\0\0\x08".to_vec();
        exif.extend_from_slice(&[0, 1]); // one entry
        exif.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, value, 0, 0]);
        exif.extend_from_slice(&[0, 0, 0, 0]); // no next IFD
        exif
    }

    const RED: Rgb<u8> = Rgb([255, 0, 0]);
    const BLUE: Rgb<u8> = Rgb([0, 0, 255]);

    /// 64×32, red on the left half and blue on the right, as a camera held
    /// upright stores it: sideways, with orientation 6 saying to turn it
    /// clockwise.
    fn sideways_jpeg() -> Vec<u8> {
        let image = RgbImage::from_fn(64, 32, |x, _| if x < 32 { RED } else { BLUE });
        let mut bytes = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut bytes, 95);
        encoder.set_exif_metadata(exif_orientation(6)).unwrap();
        encoder.encode_image(&image).unwrap();
        bytes
    }

    fn exif_of(bytes: &[u8]) -> Option<Vec<u8>> {
        ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap()
            .exif_metadata()
            .unwrap()
    }

    fn close(pixel: Rgb<u8>, expected: Rgb<u8>) -> bool {
        pixel
            .0
            .iter()
            .zip(expected.0)
            .all(|(a, b)| a.abs_diff(b) < 48)
    }

    #[test]
    fn the_fixture_is_sideways_without_orientation() {
        let raw = image::load_from_memory(&sideways_jpeg()).unwrap();
        assert_eq!((raw.width(), raw.height()), (64, 32));
    }

    #[test]
    fn an_original_is_stored_upright_without_its_exif() {
        let source = sideways_jpeg();
        let picture = Picture::decode(&source).unwrap();
        let original = picture.original(&source, Fit::Pixels(8_294_400)).unwrap();

        assert_eq!(original.format, ImageFormat::Jpeg);
        assert_eq!(exif_of(&original.bytes), None);

        let stored = image::load_from_memory(&original.bytes).unwrap().to_rgb8();
        assert_eq!(stored.dimensions(), (32, 64));
        // Turned clockwise, the left half is now the top.
        assert!(close(*stored.get_pixel(16, 8), RED));
        assert!(close(*stored.get_pixel(16, 56), BLUE));
    }

    #[test]
    fn a_thumbnail_is_upright_and_within_its_pixels() {
        let source = sideways_jpeg();
        let picture = Picture::decode(&source).unwrap();
        let small = picture
            .rendition(Fit::Pixels(512), ImageFormat::Jpeg)
            .unwrap();
        assert_eq!((small.width(), small.height()), (16, 32));
    }

    #[test]
    fn an_avatar_is_upright_and_cropped_square() {
        let source = sideways_jpeg();
        let picture = Picture::decode(&source).unwrap();
        let avatar = picture.original(&source, Fit::Cover(20, 20)).unwrap();
        assert_eq!((avatar.width(), avatar.height()), (20, 20));
        assert_eq!(exif_of(&avatar.bytes), None);
        let stored = image::load_from_memory(&avatar.bytes).unwrap().to_rgb8();
        assert!(close(*stored.get_pixel(10, 2), RED));
        assert!(close(*stored.get_pixel(10, 17), BLUE));
    }

    #[test]
    fn a_png_thumbnail_keeps_its_transparency() {
        let image = image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 0, 0]));
        let mut source = Vec::new();
        PngEncoder::new(&mut source)
            .write_image(image.as_raw(), 4, 4, image::ExtendedColorType::Rgba8)
            .unwrap();
        let picture = Picture::decode(&source).unwrap();
        let small = picture
            .rendition(Fit::Pixels(230_400), picture.thumbnail_format())
            .unwrap();
        assert_eq!(small.content_type(), "image/png");
        assert_eq!(small.image.to_rgba8().get_pixel(0, 0).0[3], 0);
    }

    #[test]
    fn a_webp_needing_nothing_keeps_its_bitstream_but_not_its_metadata() {
        const VP8X_FLAGS: u8 = 0x08 | 0x04;
        let image = RgbImage::from_pixel(8, 8, RED);
        let mut encoded = Vec::new();
        WebPEncoder::new_lossless(&mut encoded)
            .write_image(image.as_raw(), 8, 8, image::ExtendedColorType::Rgb8)
            .unwrap();
        // Rebuild it as an extended WebP carrying EXIF and XMP.
        let bitstream = &encoded[12..];
        let mut body = b"WEBP".to_vec();
        body.extend_from_slice(b"VP8X");
        body.extend_from_slice(&10u32.to_le_bytes());
        body.extend_from_slice(&[VP8X_FLAGS, 0, 0, 0, 7, 0, 0, 7, 0, 0]);
        body.extend_from_slice(bitstream);
        for (id, payload) in [
            (b"EXIF", exif_orientation(1)),
            (b"XMP ", b"<gps/>".to_vec()),
        ] {
            body.extend_from_slice(id);
            body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(&payload);
            if payload.len() % 2 == 1 {
                body.push(0);
            }
        }
        let mut source = b"RIFF".to_vec();
        source.extend_from_slice(&(body.len() as u32).to_le_bytes());
        source.extend_from_slice(&body);

        let picture = Picture::decode(&source).unwrap();
        let original = picture.original(&source, Fit::Pixels(8_294_400)).unwrap();

        assert!(!original
            .bytes
            .windows(4)
            .any(|w| w == b"EXIF" || w == b"XMP "));
        assert!(original
            .bytes
            .windows(bitstream.len())
            .any(|w| w == bitstream));
        assert_eq!(original.bytes[20] & VP8X_FLAGS, 0);
        let decoded = image::load_from_memory(&original.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (8, 8));
    }

    #[test]
    fn stripping_without_decoding_removes_exif() {
        let stripped = strip_metadata(&sideways_jpeg());
        assert_eq!(exif_of(&stripped), None);
    }
}
