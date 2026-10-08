//! `Api::V1::MediaController` and `Api::V2::MediaController`, and
//! `MediaAttachment`'s processing of what they are given: a file, and for
//! audio and video a thumbnail, validated, processed and stored as
//! Paperclip stores them.

use super::{
    convert::media_from_db,
    extractors::{Part, Parts},
};
use crate::email_subscriptions::ValidationErrors;
use crate::media::picture::{Fit, Picture};
use crate::media::transcode::{self, VideoMetadata};
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};
use axum::{
    extract::{Extension, Path},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use image::ImageFormat;
use serde_json::{json, Map, Value};

/// `MediaAttachment::IMAGE_STYLES[:original]`: 3840×2160.
const ORIGINAL_PIXELS: u64 = 8_294_400;
/// `MediaAttachment::IMAGE_STYLES[:small]`: 640×360.
const SMALL_PIXELS: u64 = 230_400;

/// `MediaAttachment::IMAGE_MIME_TYPES`.
pub const IMAGE_MIME_TYPES: &[&str] = &[
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/heic",
    "image/heif",
    "image/webp",
    "image/avif",
];
/// `MediaAttachment::VIDEO_MIME_TYPES`.
pub const VIDEO_MIME_TYPES: &[&str] = &["video/webm", "video/mp4", "video/quicktime", "video/ogg"];
/// `MediaAttachment::AUDIO_MIME_TYPES`.
pub const AUDIO_MIME_TYPES: &[&str] = &[
    "audio/wave",
    "audio/wav",
    "audio/x-wav",
    "audio/x-pn-wave",
    "audio/vnd.wave",
    "audio/ogg",
    "audio/vorbis",
    "audio/mpeg",
    "audio/mp3",
    "audio/webm",
    "audio/flac",
    "audio/aac",
    "audio/m4a",
    "audio/x-m4a",
    "audio/mp4",
    "audio/3gpp",
    "video/x-ms-asf",
];
/// `MediaAttachment::IMAGE_LIMIT`.
pub const IMAGE_LIMIT: usize = 16 * 1024 * 1024;
/// `MediaAttachment::VIDEO_LIMIT`, for video, gifv and audio.
pub const VIDEO_LIMIT: usize = 99 * 1024 * 1024;
/// The body an upload may need: a file at `VIDEO_LIMIT` and a thumbnail at
/// `IMAGE_LIMIT`, with room to be refused by the validations rather than by
/// the server.
pub const UPLOAD_BODY_LIMIT: usize = VIDEO_LIMIT + IMAGE_LIMIT + 1024 * 1024;
/// The body of an update, which may carry a thumbnail.
pub const UPDATE_BODY_LIMIT: usize = IMAGE_LIMIT + 1024 * 1024;
/// `MediaAttachment::MAX_VIDEO_MATRIX_LIMIT`: 3840×2160.
const MAX_VIDEO_MATRIX_LIMIT: i64 = 8_294_400;
/// `MediaAttachment::MAX_VIDEO_FRAME_RATE`.
const MAX_VIDEO_FRAME_RATE: i64 = 120;
/// `Attachmentable::MAX_MATRIX_LIMIT`: 7680×4320.
const MAX_MATRIX_LIMIT: u64 = 33_177_600;
/// `Attachmentable::GIF_MATRIX_LIMIT`: 1280×720.
const GIF_MATRIX_LIMIT: u64 = 921_600;
/// `MediaAttachment::MAX_DESCRIPTION_LENGTH`.
const MAX_DESCRIPTION_LENGTH: usize = 10_000;

/// `processing`'s enum: `{ queued: 0, in_progress: 1, complete: 2,
/// failed: 3 }`.
const QUEUED: i32 = 0;
const IN_PROGRESS: i32 = 1;
const COMPLETE: i32 = 2;
const FAILED: i32 = 3;

/// `processing_error`.
const PROCESSING_ERROR: &str = "Error processing thumbnail for uploaded media";

/// Why a request was refused.
enum MediaError {
    App(AppError),
    /// `ActiveRecord::RecordInvalid` or `Mastodon::ValidationError`, which
    /// `Api::BaseController` answers with a 422 and the message.
    Invalid(String),
    /// A `Paperclip::Error` while processing, answered with
    /// `processing_error` and a 500.
    Processing,
}

impl<E: Into<AppError>> From<E> for MediaError {
    fn from(error: E) -> Self {
        Self::App(error.into())
    }
}

impl IntoResponse for MediaError {
    fn into_response(self) -> Response {
        match self {
            Self::App(error) => error.into_response(),
            Self::Invalid(message) => AppError::Unprocessable(message).into_response(),
            Self::Processing => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": PROCESSING_ERROR })),
            )
                .into_response(),
        }
    }
}

fn invalid(errors: ValidationErrors) -> MediaError {
    MediaError::Invalid(errors.message())
}

/// A content type without its parameters, lowercased.
fn essence(content_type: &str) -> String {
    crate::media::profile::essence(content_type)
}

/// An uploaded file.
struct Upload {
    content_type: String,
    data: Vec<u8>,
    file_name: Option<String>,
}

/// `media_attachment_params` and `updateable_media_attachment_params`.
#[derive(Default)]
struct MediaParams {
    file: Option<Upload>,
    thumbnail: Option<Upload>,
    description: Option<String>,
    focus: Option<String>,
}

fn media_params(parts: Vec<(String, Part)>) -> MediaParams {
    let mut params = MediaParams::default();
    for (name, part) in parts {
        let upload = |part: Part| match part {
            Part::File {
                content_type,
                data,
                file_name,
            } if !data.is_empty() => Some(Upload {
                content_type: essence(&content_type),
                data,
                file_name,
            }),
            _ => None,
        };
        match name.as_str() {
            "file" => params.file = upload(part),
            "thumbnail" => params.thumbnail = upload(part),
            "description" => params.description = Some(part.text()),
            "focus" => params.focus = Some(part.text()),
            _ => {}
        }
    }
    params
}

/// `set_type_and_extension`: what a file is taken for until processing
/// says otherwise.
fn type_for(content_type: &str) -> i32 {
    if VIDEO_MIME_TYPES.contains(&content_type) {
        TYPE_VIDEO
    } else if AUDIO_MIME_TYPES.contains(&content_type) {
        TYPE_AUDIO
    } else {
        TYPE_IMAGE
    }
}

/// `audio_or_video?`.
fn audio_or_video(kind: i32) -> bool {
    kind == TYPE_AUDIO || kind == TYPE_VIDEO
}

/// The validations of a file and a thumbnail, in the order `MediaAttachment`
/// declares them; `kind` is the attachment's type, for the thumbnail's
/// `absence` when there is no file.
fn validate(
    file: Option<&Upload>,
    thumbnail: Option<&Upload>,
    description: Option<&str>,
    kind: i32,
    creating: bool,
) -> ValidationErrors {
    let mut errors = ValidationErrors::default();
    if let Some(file) = file {
        let supported = IMAGE_MIME_TYPES
            .iter()
            .chain(VIDEO_MIME_TYPES)
            .chain(AUDIO_MIME_TYPES)
            .any(|t| *t == file.content_type);
        if !supported {
            errors.add("file_content_type", "invalid", "is invalid");
            errors.add("file", "invalid", "is invalid");
        }
        // `larger_media_format?` at validation, when a GIF is still an image.
        if audio_or_video(kind) {
            if file.data.len() >= VIDEO_LIMIT {
                errors.add("file_file_size", "less_than", "must be less than 99 MB");
                errors.add("file", "less_than", "must be less than 99 MB");
            }
        } else if file.data.len() >= IMAGE_LIMIT {
            errors.add("file_file_size", "less_than", "must be less than 16 MB");
            errors.add("file", "less_than", "must be less than 16 MB");
        }
    }
    if let Some(thumbnail) = thumbnail {
        if !IMAGE_MIME_TYPES.contains(&thumbnail.content_type.as_str()) {
            errors.add("thumbnail_content_type", "invalid", "is invalid");
            errors.add("thumbnail", "invalid", "is invalid");
        }
        if thumbnail.data.len() >= IMAGE_LIMIT {
            errors.add(
                "thumbnail_file_size",
                "less_than",
                "must be less than 16 MB",
            );
            errors.add("thumbnail", "less_than", "must be less than 16 MB");
        }
    }
    if description.is_some_and(|d| d.chars().count() > MAX_DESCRIPTION_LENGTH) {
        errors.add(
            "description",
            "too_long",
            "is too long (maximum is 10000 characters)",
        );
    }
    if creating && file.is_none() {
        errors.add("file", "blank", "can't be blank");
    }
    if thumbnail.is_some() && !audio_or_video(kind) {
        errors.add("thumbnail", "present", "must be blank");
    }
    errors
}

/// `Attachmentable#check_image_dimension`, which raises.
fn check_image_dimension(upload: &Upload) -> Result<(), MediaError> {
    if !upload.content_type.starts_with("image") {
        return Ok(());
    }
    let Some((width, height)) = image::ImageReader::new(std::io::Cursor::new(&upload.data))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok())
    else {
        return Ok(());
    };
    let pixels = u64::from(width) * u64::from(height);
    if upload.content_type == "image/gif" && pixels > GIF_MATRIX_LIMIT {
        return Err(MediaError::Invalid(format!(
            "{width}x{height} GIF files are not supported"
        )));
    }
    if pixels > MAX_MATRIX_LIMIT {
        return Err(MediaError::Invalid(format!(
            "{width}x{height} images are not supported"
        )));
    }
    Ok(())
}

/// `MediaAttachment#check_video_dimensions`, which raises. Only a video is
/// checked: a GIF is an image until it is transcoded.
fn check_video_dimensions(movie: &VideoMetadata) -> Result<(), MediaError> {
    if !movie.valid() {
        return Ok(());
    }
    let (Some(width), Some(height), Some(frame_rate)) =
        (movie.width, movie.height, movie.frame_rate_floor())
    else {
        return Err(MediaError::Invalid("Video has no video stream".into()));
    };
    if width * height > MAX_VIDEO_MATRIX_LIMIT {
        return Err(MediaError::Invalid(format!(
            "{width}x{height} videos are not supported"
        )));
    }
    if frame_rate > MAX_VIDEO_FRAME_RATE {
        return Err(MediaError::Invalid(format!(
            "{frame_rate}fps videos are not supported"
        )));
    }
    Ok(())
}

/// `FastGeometryParser` failing to read a file, which `post_process_style`
/// records against the attachment.
fn not_identified(attribute: &'static str) -> MediaError {
    let mut errors = ValidationErrors::default();
    errors.add(
        attribute,
        "invalid",
        "Paperclip::Errors::NotIdentifiedByImageMagickError",
    );
    invalid(errors)
}

/// `MediaAttachment#focus=`: `"x,y"`, each read as Ruby's `to_f` reads it.
fn parse_focus(point: &str) -> Option<Value> {
    if point.trim().is_empty() {
        return None;
    }
    let mut values = point.split(',').map(ruby_to_f);
    let x = values.next().unwrap_or(0.0);
    let y = values.next();
    Some(json!({ "x": x, "y": y }))
}

/// `String#to_f`: the leading number, or 0.
fn ruby_to_f(s: &str) -> f64 {
    let s = s.trim_start();
    let mut end = 0;
    let bytes = s.as_bytes();
    let mut seen_digit = false;
    let mut seen_dot = false;
    while end < bytes.len() {
        match bytes[end] {
            b'+' | b'-' if end == 0 => {}
            b'0'..=b'9' => seen_digit = true,
            b'.' if !seen_dot => seen_dot = true,
            _ => break,
        }
        end += 1;
    }
    if !seen_digit {
        return 0.0;
    }
    s[..end].trim_end_matches('.').parse().unwrap_or(0.0)
}

/// `SecureRandom.hex(8)`, which `obfuscate_file_name` names a file with.
fn random_stem() -> String {
    uuid::Uuid::new_v4().as_bytes()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `Attachmentable#appropriate_extension`: the upload's own extension when
/// it is one of its type's, else the type's first.
fn extension(content_type: &str, file_name: Option<&str>) -> String {
    let extensions = mime_guess::get_mime_extensions_str(content_type).unwrap_or_default();
    let own = file_name
        .and_then(|name| name.rsplit_once('.'))
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| extensions.contains(&ext.as_str()));
    let extension = own.unwrap_or_else(|| {
        match content_type {
            "image/jpeg" => "jpeg",
            "image/png" => "png",
            "image/gif" => "gif",
            "image/webp" => "webp",
            "video/mp4" => "mp4",
            "audio/mpeg" => "mp3",
            other => crate::media::ext_for_content_type(other),
        }
        .to_owned()
    });
    match extension.as_str() {
        "jpe" | "jfif" => "jpeg".to_owned(),
        _ => extension,
    }
}

fn file_key(media_id: i64, style: &str, name: &str) -> String {
    format!(
        "media_attachments/files/{}/{style}/{name}",
        crate::media::int_to_path(media_id)
    )
}

fn thumbnail_key(media_id: i64, name: &str) -> String {
    format!(
        "media_attachments/thumbnails/{}/original/{name}",
        crate::media::int_to_path(media_id)
    )
}

/// `{width, height, size, aspect}`, `MediaAttachment#image_geometry`.
fn image_geometry(w: u32, h: u32) -> Value {
    json!({
        "width": w,
        "height": h,
        "size": format!("{w}x{h}"),
        "aspect": f64::from(w) / f64::from(h),
    })
}

/// What a media attachment's row records of its files.
#[derive(Default)]
struct Files {
    kind: i32,
    file_file_name: Option<String>,
    file_content_type: Option<String>,
    file_file_size: Option<i32>,
    thumbnail: Option<StoredThumbnail>,
    meta: Map<String, Value>,
    blurhash: Option<String>,
    processing: i32,
    /// The original kept to be transcoded by the queue, and as what.
    job: Option<(String, &'static str, String)>,
}

struct StoredThumbnail {
    file_name: String,
    content_type: &'static str,
    size: i32,
}

// ── POST /api/v1/media, POST /api/v2/media ────────────────────────────────

/// `Api::V1::MediaController#create`: processed before it answers, 200.
pub async fn upload_media(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Parts(parts): Parts,
) -> Response {
    create(&state, &auth, parts, false)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

/// `Api::V2::MediaController#create`: video and audio are processed by the
/// queue (`delay_processing`), answered with 202 until they are.
pub async fn upload_media_v2(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Parts(parts): Parts,
) -> Response {
    create(&state, &auth, parts, true)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

async fn create(
    state: &AppState,
    auth: &AuthenticatedUser,
    parts: Vec<(String, Part)>,
    delay: bool,
) -> Result<Response, MediaError> {
    auth.require_scope("write:media")?;
    let params = media_params(parts);
    let kind = params
        .file
        .as_ref()
        .map_or(TYPE_IMAGE, |file| type_for(&file.content_type));

    // `before_file_validate`, which raises: the images' dimensions, then a
    // video's.
    for upload in [params.file.as_ref(), params.thumbnail.as_ref()]
        .into_iter()
        .flatten()
    {
        check_image_dimension(upload)?;
    }
    let movie = match &params.file {
        Some(file) if kind == TYPE_VIDEO || kind == TYPE_AUDIO => {
            Some(transcode::probe(&file.data).await)
        }
        _ => None,
    };
    if kind == TYPE_VIDEO {
        if let Some(movie) = &movie {
            check_video_dimensions(movie)?;
        }
    }
    let errors = validate(
        params.file.as_ref(),
        params.thumbnail.as_ref(),
        params.description.as_deref(),
        kind,
        true,
    );
    if !errors.is_empty() {
        return Err(invalid(errors));
    }
    let Some(file) = params.file else {
        unreachable!("validated as present");
    };

    // Mastodon 4.7.2 no longer lets libvips load HEIF ("Temporarily disable
    // HEIF support"), so processing a HEIC, HEIF or AVIF image fails,
    // whatever type it was declared as, and `rescue Paperclip::Error`
    // answers it. The types are still advertised.
    if kind == TYPE_IMAGE && is_heif(&file.data) {
        tracing::warn!("refusing a HEIF upload, as Mastodon 4.7.2 does");
        return Err(MediaError::Processing);
    }

    let media_id = crate::snowflake::next_id();
    // `delay_processing?`: only the larger formats, and a GIF is not one
    // until it has been processed.
    let delayed = delay && audio_or_video(kind);
    let mut files = match kind {
        TYPE_VIDEO => {
            video_files(state, media_id, file, movie.unwrap_or_default(), delayed).await?
        }
        TYPE_AUDIO => {
            audio_files(state, media_id, file, movie.unwrap_or_default(), delayed).await?
        }
        _ if file.content_type == "image/gif" => gif_files(state, media_id, file).await?,
        _ => image_files(state, media_id, file).await?,
    };
    if let Some(thumbnail) = params.thumbnail {
        let processed = process_thumbnail(thumbnail.data)
            .await
            .ok_or_else(|| not_identified("thumbnail"))?;
        replace_thumbnail(state, media_id, &mut files, processed).await?;
    }
    if let Some(focus) = params.focus.as_deref().and_then(parse_focus) {
        files.meta.insert("focus".into(), focus);
    }

    let attachment = sqlx::query_as!(
        crate::db::models::MediaAttachment,
        r#"INSERT INTO media_attachments
             (id, account_id, "type", description, processing,
              file_file_name, file_content_type, file_file_size, file_updated_at,
              file_storage_schema_version, file_meta, blurhash,
              thumbnail_file_name, thumbnail_content_type, thumbnail_file_size,
              thumbnail_updated_at, thumbnail_storage_schema_version,
              created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now(), 1, $9, $10, $11, $12, $13,
                   CASE WHEN $14 THEN now() END,
                   CASE WHEN $14 THEN 1 END,
                   now(), now())
           RETURNING *"#,
        media_id,
        auth.account_id,
        files.kind,
        params.description,
        files.processing,
        files.file_file_name,
        files.file_content_type,
        files.file_file_size,
        Value::Object(files.meta),
        files.blurhash,
        files.thumbnail.as_ref().map(|t| t.file_name.clone()),
        files.thumbnail.as_ref().map(|t| t.content_type),
        files.thumbnail.as_ref().map(|t| t.size),
        files.thumbnail.is_some(),
    )
    .fetch_one(&state.db)
    .await?;

    if let Some((source_key, media_type, content_type)) = files.job {
        sqlx::query!(
            r#"INSERT INTO eunha.media_processing_jobs
                 (media_id, media_type, source_key, content_type)
               VALUES ($1, $2, $3, $4)"#,
            media_id,
            media_type,
            source_key,
            content_type,
        )
        .execute(&state.db)
        .await?;
        state.queues.media.notify_one();
    }

    // v1 answers 200 whatever; v2 202 while processing waits in the queue.
    let status = if delay && not_processed(&attachment) {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(media_from_db(&state.urls, &attachment))).into_response())
}

/// `not_processed?`.
fn not_processed(m: &crate::db::models::MediaAttachment) -> bool {
    m.processing.is_some_and(|p| p != COMPLETE)
}

/// An image: `IMAGE_STYLES`, through `lazy_thumbnail` and the blurhash of
/// its small style.
async fn image_files(state: &AppState, media_id: i64, file: Upload) -> Result<Files, MediaError> {
    let data = file.data;
    let processed = crate::tenants::spawn_blocking(move || process_image(&data))
        .await
        .map_err(|e| anyhow::anyhow!("image processing did not finish: {e}"))?
        .ok_or_else(|| not_identified("file"))?;
    let name = format!(
        "{}.{}",
        random_stem(),
        extension(processed.content_type, file.file_name.as_deref())
    );
    state
        .storage
        .store(
            &processed.original,
            &file_key(media_id, "original", &name),
            processed.content_type,
        )
        .await?;
    state
        .storage
        .store(
            &processed.small.bytes,
            &file_key(media_id, "small", &name),
            processed.small.content_type,
        )
        .await?;
    let mut meta = Map::new();
    meta.insert("original".into(), processed.original_meta);
    meta.insert("small".into(), processed.small.meta);
    Ok(Files {
        kind: TYPE_IMAGE,
        file_file_name: Some(name),
        file_content_type: Some(processed.content_type.to_owned()),
        file_file_size: Some(processed.original.len() as i32),
        meta,
        blurhash: Some(processed.small.blurhash),
        processing: COMPLETE,
        ..Files::default()
    })
}

/// A GIF: `GifTranscoder`, which makes an animated one a gifv, an MP4 with
/// a PNG of its first frame for its small style, and leaves a still one an
/// image, kept as it came.
async fn gif_files(state: &AppState, media_id: i64, file: Upload) -> Result<Files, MediaError> {
    let data = file.data;
    // `None` for an animated GIF; for a still one, its geometry and
    // blurhash, or `None` within when it cannot be read.
    let (data, still) = crate::tenants::spawn_blocking(move || {
        let still = (!animated(&data)).then(|| {
            let picture = Picture::decode(&data)?;
            Some((
                image_geometry(picture.width(), picture.height()),
                blurhash_of(&picture)?,
            ))
        });
        (data, still)
    })
    .await
    .map_err(|e| anyhow::anyhow!("image processing did not finish: {e}"))?;
    let stem = random_stem();
    let mut meta = Map::new();
    if let Some(still) = still {
        // `GifTranscoder` leaves a still GIF as it is, for its small style
        // too, which is kept under a `.png` name all the same.
        let (geometry, blurhash) = still.ok_or_else(|| not_identified("file"))?;
        let name = format!("{stem}.gif");
        state
            .storage
            .store(&data, &file_key(media_id, "original", &name), "image/gif")
            .await?;
        state
            .storage
            .store(
                &data,
                &file_key(media_id, "small", &format!("{stem}.png")),
                "image/gif",
            )
            .await?;
        meta.insert("original".into(), geometry.clone());
        meta.insert("small".into(), geometry);
        return Ok(Files {
            kind: TYPE_IMAGE,
            file_file_name: Some(name),
            file_content_type: Some("image/gif".into()),
            file_file_size: Some(data.len() as i32),
            meta,
            blurhash: Some(blurhash),
            processing: COMPLETE,
            ..Files::default()
        });
    }
    let input = transcode::probe(&data).await;
    let transcoded = transcode::transcode(&data, &input, false, true)
        .await
        .map_err(processing_failed)?;
    let small = small_frame(&data).await?;
    let name = format!("{stem}.mp4");
    state
        .storage
        .store(
            &transcoded.bytes,
            &file_key(media_id, "original", &name),
            transcoded.content_type,
        )
        .await?;
    state
        .storage
        .store(
            &small.bytes,
            &file_key(media_id, "small", &format!("{stem}.png")),
            small.content_type,
        )
        .await?;
    // `populate_meta` reads the transcoded original first, and keeps it.
    let output = transcode::probe(&transcoded.bytes).await;
    meta.insert("original".into(), output.meta());
    meta.insert("small".into(), small.meta);
    Ok(Files {
        kind: TYPE_GIFV,
        file_file_name: Some(name),
        file_content_type: Some(transcoded.content_type.into()),
        file_file_size: Some(transcoded.bytes.len() as i32),
        meta,
        blurhash: Some(small.blurhash),
        processing: COMPLETE,
        ..Files::default()
    })
}

fn processing_failed(error: anyhow::Error) -> MediaError {
    tracing::warn!(%error, "media processing failed");
    MediaError::Processing
}

/// `GifReader.animated?`: more than one image in it.
fn animated(data: &[u8]) -> bool {
    use image::AnimationDecoder;
    image::codecs::gif::GifDecoder::new(std::io::Cursor::new(data))
        .map(|decoder| decoder.into_frames().take(2).count() > 1)
        .unwrap_or(false)
}

/// `VIDEO_STYLES[:small]`: the first frame, fitted in 640×640, with the
/// blurhash `blurhash_transcoder` makes of it.
async fn small_frame(data: &[u8]) -> Result<Thumbnail, MediaError> {
    let png = transcode::small_frame(data)
        .await
        .map_err(processing_failed)?;
    crate::tenants::spawn_blocking(move || {
        let picture = Picture::decode(&png)?;
        let blurhash = blurhash_of(&picture)?;
        Some(Thumbnail {
            meta: image_geometry(picture.width(), picture.height()),
            content_type: "image/png",
            bytes: png,
            blurhash,
            colors: None,
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("image processing did not finish: {e}"))?
    .ok_or(MediaError::Processing)
}

/// A video: its small style made now, and its original transcoded now or,
/// when processing is delayed, by the queue. A video with no audio is a
/// gifv (`Transcoder#update_attachment_type`).
async fn video_files(
    state: &AppState,
    media_id: i64,
    file: Upload,
    movie: VideoMetadata,
    delayed: bool,
) -> Result<Files, MediaError> {
    // `Transcoder#make` refuses what `ffprobe` cannot read.
    if !movie.valid() {
        return Err(MediaError::Processing);
    }
    let kind = if movie.audio_codec.is_none() {
        TYPE_GIFV
    } else {
        TYPE_VIDEO
    };
    let stem = random_stem();
    let small = small_frame(&file.data).await?;
    state
        .storage
        .store(
            &small.bytes,
            &file_key(media_id, "small", &format!("{stem}.png")),
            small.content_type,
        )
        .await?;
    let mut meta = Map::new();
    // `ffmpeg_data` is memoized from `check_video_dimensions`, so a video's
    // `original` is what was uploaded.
    meta.insert("original".into(), movie.meta());
    meta.insert("small".into(), small.meta);
    let mut files = Files {
        kind,
        meta,
        blurhash: Some(small.blurhash),
        ..Files::default()
    };
    if delayed {
        queue_original(state, media_id, &stem, file, "video", &mut files).await?;
        return Ok(files);
    }
    let transcoded = transcode::transcode(&file.data, &movie, false, false)
        .await
        .map_err(processing_failed)?;
    store_transcoded(state, media_id, &stem, &transcoded, &mut files).await?;
    Ok(files)
}

/// An audio file: transcoded to MP3 now or, when processing is delayed, by
/// the queue, with its cover art as its thumbnail (`ImageExtractor`).
async fn audio_files(
    state: &AppState,
    media_id: i64,
    file: Upload,
    movie: VideoMetadata,
    delayed: bool,
) -> Result<Files, MediaError> {
    let stem = random_stem();
    let mut files = Files {
        kind: TYPE_AUDIO,
        ..Files::default()
    };
    if delayed {
        // `populate_meta` reads the original as it came, until the queue
        // replaces it.
        files.meta.insert("original".into(), movie.meta());
        queue_original(state, media_id, &stem, file, "audio", &mut files).await?;
        return Ok(files);
    }
    if !movie.valid() {
        return Err(MediaError::Processing);
    }
    let transcoded = transcode::transcode(&file.data, &movie, true, false)
        .await
        .map_err(processing_failed)?;
    let output = transcode::probe(&transcoded.bytes).await;
    files.meta.insert("original".into(), output.meta());
    store_transcoded(state, media_id, &stem, &transcoded, &mut files).await?;
    if let Some(cover) = transcode::cover_art(&file.data).await {
        if let Some(processed) = process_thumbnail(cover).await {
            replace_thumbnail(state, media_id, &mut files, processed).await?;
        }
    }
    Ok(files)
}

/// Keep the original as it came, under the name it will keep, for the
/// queue to transcode: `delay_processing`, with the row `queued`.
async fn queue_original(
    state: &AppState,
    media_id: i64,
    stem: &str,
    file: Upload,
    media_type: &'static str,
    files: &mut Files,
) -> Result<(), MediaError> {
    let name = format!(
        "{stem}.{}",
        extension(&file.content_type, file.file_name.as_deref())
    );
    let key = file_key(media_id, "original", &name);
    state
        .storage
        .store(&file.data, &key, &file.content_type)
        .await?;
    files.file_file_name = Some(name);
    files.file_file_size = Some(file.data.len() as i32);
    files.file_content_type = Some(file.content_type.clone());
    files.processing = QUEUED;
    files.job = Some((key, media_type, file.content_type));
    Ok(())
}

async fn store_transcoded(
    state: &AppState,
    media_id: i64,
    stem: &str,
    transcoded: &transcode::Transcoded,
    files: &mut Files,
) -> Result<(), MediaError> {
    let name = format!("{stem}.{}", transcoded.ext);
    state
        .storage
        .store(
            &transcoded.bytes,
            &file_key(media_id, "original", &name),
            transcoded.content_type,
        )
        .await?;
    files.file_file_name = Some(name);
    files.file_content_type = Some(transcoded.content_type.into());
    files.file_file_size = Some(transcoded.bytes.len() as i32);
    files.processing = COMPLETE;
    Ok(())
}

/// A thumbnail through `THUMBNAIL_STYLES`: the small style's 230,400
/// pixels, its blurhash and its colours (`ColorExtractor`). `None` when it
/// is no image this build reads.
async fn process_thumbnail(data: Vec<u8>) -> Option<Thumbnail> {
    crate::tenants::spawn_blocking(move || {
        let picture = Picture::decode(&data)?;
        let mut small = thumbnail(
            &picture,
            Fit::Pixels(SMALL_PIXELS),
            picture.thumbnail_format(),
        )?;
        small.colors = crate::media::colors::extract(picture.image());
        Some(small)
    })
    .await
    .ok()
    .flatten()
}

/// Store `processed` as the attachment's thumbnail, removing the one it
/// had, and record it: its geometry as `meta.small`, its colours, and its
/// blurhash as the attachment's.
async fn replace_thumbnail(
    state: &AppState,
    media_id: i64,
    files: &mut Files,
    processed: Thumbnail,
) -> Result<(), MediaError> {
    let name = format!(
        "{}.{}",
        random_stem(),
        extension(processed.content_type, None)
    );
    state
        .storage
        .store(
            &processed.bytes,
            &thumbnail_key(media_id, &name),
            processed.content_type,
        )
        .await?;
    if let Some(old) = files.thumbnail.take() {
        let _ = state
            .storage
            .delete(&thumbnail_key(media_id, &old.file_name))
            .await;
    }
    files.thumbnail = Some(StoredThumbnail {
        file_name: name,
        content_type: processed.content_type,
        size: processed.bytes.len() as i32,
    });
    files.meta.insert("small".into(), processed.meta);
    match processed.colors {
        Some(colors) => files.meta.insert("colors".into(), colors),
        None => files.meta.remove("colors"),
    };
    files.blurhash = Some(processed.blurhash);
    Ok(())
}

/// An image upload as it is stored.
struct ProcessedImage {
    original: Vec<u8>,
    content_type: &'static str,
    original_meta: Value,
    small: Thumbnail,
}

struct Thumbnail {
    bytes: Vec<u8>,
    content_type: &'static str,
    meta: Value,
    blurhash: String,
    colors: Option<Value>,
}

/// Turn an image upload upright, cap it at Mastodon's original size, and make
/// its small thumbnail and blurhash, the way `MediaAttachment`'s image styles
/// do. See [`crate::media::picture`] for why the original is re-encoded.
/// `None` when it is no image this build reads, which is never stored.
///
/// CPU-bound: call it from the blocking pool, never on a Tokio worker.
fn process_image(data: &[u8]) -> Option<ProcessedImage> {
    let picture = Picture::decode(data)?;
    let original = picture.original(data, Fit::Pixels(ORIGINAL_PIXELS))?;
    // The small style keeps the original's format, under its name.
    let small = thumbnail(&picture, Fit::Pixels(SMALL_PIXELS), original.format)?;
    Some(ProcessedImage {
        original_meta: image_geometry(original.width(), original.height()),
        content_type: original.content_type(),
        original: original.bytes,
        small,
    })
}

/// A rendition of `picture` fitted to `fit`, blurhashed from itself as
/// Mastodon's `BlurhashTranscoder` does.
fn thumbnail(picture: &Picture, fit: Fit, format: ImageFormat) -> Option<Thumbnail> {
    let small = picture.rendition(fit, format)?;
    let blurhash = blurhash::encode(
        4,
        4,
        small.width(),
        small.height(),
        small.image.to_rgba8().as_raw(),
    )
    .ok()?;
    Some(Thumbnail {
        content_type: small.content_type(),
        meta: image_geometry(small.width(), small.height()),
        bytes: small.bytes,
        blurhash,
        colors: None,
    })
}

fn blurhash_of(picture: &Picture) -> Option<String> {
    let image = picture.image();
    blurhash::encode(
        4,
        4,
        image.width(),
        image.height(),
        image.to_rgba8().as_raw(),
    )
    .ok()
}

// ── Media processing queue (eunha.media_processing_jobs) ───────────────────

const MEDIA_QUEUE_IDLE: std::time::Duration = std::time::Duration::from_secs(2);
const MEDIA_QUEUE_ERROR_IDLE: std::time::Duration = std::time::Duration::from_secs(10);

/// Drain the durable media-processing queue until the instance is stopped.
pub async fn run_media_queue(state: AppState) {
    let worker_id = format!("media-{}", std::process::id());
    let mut idle = crate::background::IdleBackoff::new(
        MEDIA_QUEUE_IDLE,
        state.config.workers.sanitized().queue_idle_poll(),
    );
    while !state.stop.is_cancelled() {
        match run_media_queue_batch(&state, &worker_id).await {
            Ok(0) => idle.idle(&state.queues.media, &state.stop).await,
            Ok(_) => idle.reset(),
            Err(e) => {
                tracing::error!(error = %e, "media processing queue batch failed");
                crate::background::rest(&state.stop, MEDIA_QUEUE_ERROR_IDLE).await;
            }
        }
    }
}

/// Claim and run the jobs that are due; how many there were.
pub async fn run_media_queue_batch(state: &AppState, worker_id: &str) -> anyhow::Result<usize> {
    // Claim due jobs, re-claiming any whose lock went stale (crashed worker).
    let jobs = sqlx::query!(
        r#"WITH picked AS (
             SELECT id FROM eunha.media_processing_jobs
             WHERE run_at <= now()
               AND (locked_at IS NULL OR locked_at < now() - interval '10 minutes')
             ORDER BY run_at ASC, id ASC
             LIMIT 4
             FOR UPDATE SKIP LOCKED
           )
           UPDATE eunha.media_processing_jobs j
           SET locked_at = now(), locked_by = $1, updated_at = now()
           FROM picked
           WHERE j.id = picked.id
           RETURNING j.id, j.media_id, j.media_type, j.source_key, j.attempts, j.max_attempts"#,
        worker_id,
    )
    .fetch_all(&state.db)
    .await?;

    let count = jobs.len();
    for job in jobs {
        process_media_job(
            state,
            job.id,
            job.media_id,
            &job.media_type,
            &job.source_key,
            job.attempts,
            job.max_attempts,
        )
        .await;
    }
    Ok(count)
}

/// `PostProcessMediaWorker`: the original transcoded, and for audio its
/// cover art made the thumbnail. The row is `in_progress` meanwhile, and
/// `complete` after, with the new `meta` merged over what it had.
async fn process_queued(
    state: &AppState,
    media_id: i64,
    media_type: &str,
    source_key: &str,
) -> anyhow::Result<()> {
    let Some(row) = sqlx::query_as!(
        crate::db::models::MediaAttachment,
        "SELECT * FROM media_attachments WHERE id = $1",
        media_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    sqlx::query!(
        "UPDATE media_attachments SET processing = $2, updated_at = now() WHERE id = $1",
        media_id,
        IN_PROGRESS,
    )
    .execute(&state.db)
    .await?;
    let data = state
        .storage
        .get(source_key)
        .await
        .map_err(|e| anyhow::anyhow!("fetch source: {e}"))?;
    let audio = media_type == "audio";
    let movie = transcode::probe(&data).await;
    anyhow::ensure!(movie.valid(), "unsupported file");
    let transcoded = transcode::transcode(&data, &movie, audio, false).await?;

    let stem = row
        .file_file_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .map(|name| {
            name.rsplit_once('.')
                .map_or(name, |(stem, _)| stem)
                .to_owned()
        })
        .unwrap_or_else(random_stem);
    let mut files = Files {
        kind: row.r#type.unwrap_or(TYPE_UNKNOWN),
        meta: row
            .file_meta
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        blurhash: row.blurhash.clone(),
        thumbnail: row
            .thumbnail_file_name
            .clone()
            .filter(|name| !name.is_empty())
            .map(|file_name| StoredThumbnail {
                file_name,
                content_type: "",
                size: 0,
            }),
        ..Files::default()
    };
    let original = if audio {
        transcode::probe(&transcoded.bytes).await
    } else {
        movie
    };
    files.meta.insert("original".into(), original.meta());
    store_transcoded(state, media_id, &stem, &transcoded, &mut files)
        .await
        .map_err(|_| anyhow::anyhow!("store original"))?;
    let mut thumbnail_changed = false;
    if audio {
        if let Some(cover) = transcode::cover_art(&data).await {
            if let Some(processed) = process_thumbnail(cover).await {
                replace_thumbnail(state, media_id, &mut files, processed)
                    .await
                    .map_err(|_| anyhow::anyhow!("store thumbnail"))?;
                thumbnail_changed = true;
            }
        }
    }
    let new_key = file_key(
        media_id,
        "original",
        files.file_file_name.as_deref().unwrap_or_default(),
    );
    let thumbnail = files.thumbnail.as_ref().filter(|_| thumbnail_changed);
    sqlx::query!(
        r#"UPDATE media_attachments
           SET file_file_name = $2, file_content_type = $3, file_file_size = $4,
               file_updated_at = now(), file_meta = $5, blurhash = $6, processing = $7,
               thumbnail_file_name = CASE WHEN $8 THEN $9 ELSE thumbnail_file_name END,
               thumbnail_content_type = CASE WHEN $8 THEN $10 ELSE thumbnail_content_type END,
               thumbnail_file_size = CASE WHEN $8 THEN $11 ELSE thumbnail_file_size END,
               thumbnail_updated_at = CASE WHEN $8 THEN now() ELSE thumbnail_updated_at END,
               thumbnail_storage_schema_version =
                 CASE WHEN $8 THEN 1 ELSE thumbnail_storage_schema_version END,
               updated_at = now()
           WHERE id = $1"#,
        media_id,
        files.file_file_name,
        files.file_content_type,
        files.file_file_size,
        Value::Object(files.meta),
        files.blurhash,
        COMPLETE,
        thumbnail_changed,
        thumbnail.map(|t| t.file_name.clone()),
        thumbnail.map(|t| t.content_type),
        thumbnail.map(|t| t.size),
    )
    .execute(&state.db)
    .await?;
    // The original as it came, unless the transcode kept its name.
    if new_key != source_key {
        let _ = state.storage.delete(source_key).await;
    }
    Ok(())
}

async fn process_media_job(
    state: &AppState,
    id: i64,
    media_id: i64,
    media_type: &str,
    source_key: &str,
    attempts: i32,
    max_attempts: i32,
) {
    match process_queued(state, media_id, media_type, source_key).await {
        Ok(()) => {
            let _ = sqlx::query!("DELETE FROM eunha.media_processing_jobs WHERE id = $1", id)
                .execute(&state.db)
                .await;
        }
        Err(e) => {
            let next = attempts + 1;
            let err = crate::error::sanitize_error_text(&e.to_string());
            if next >= max_attempts {
                tracing::warn!(media_id, error = %err, "media processing failed permanently");
                let _ = sqlx::query!(
                    "UPDATE media_attachments SET processing = $2, updated_at = now() WHERE id = $1",
                    media_id,
                    FAILED,
                )
                .execute(&state.db)
                .await;
                let _ = sqlx::query!("DELETE FROM eunha.media_processing_jobs WHERE id = $1", id)
                    .execute(&state.db)
                    .await;
            } else {
                // Exponential backoff: 30s, 60s, 120s, …
                let backoff = 30_i64 << (next.clamp(1, 6) - 1);
                let run_at = chrono::Utc::now() + chrono::Duration::seconds(backoff);
                let _ = sqlx::query!(
                    r#"UPDATE eunha.media_processing_jobs
                       SET attempts = $2, run_at = $3, locked_at = NULL, locked_by = NULL,
                           last_error = $4, updated_at = now()
                       WHERE id = $1"#,
                    id,
                    next,
                    run_at,
                    err,
                )
                .execute(&state.db)
                .await;
                tracing::warn!(media_id, attempts = next, error = %err, "media processing failed; will retry");
            }
        }
    }
}

// ── GET /api/v1/media/:id ─────────────────────────────────────────────────

/// `set_media_attachment` and `check_processing`: the account's own
/// unattached attachment, refused with `processing_error` when its
/// processing failed.
async fn own_unattached(
    state: &AppState,
    auth: &AuthenticatedUser,
    id: i64,
) -> Result<crate::db::models::MediaAttachment, MediaError> {
    auth.require_scope("write:media")?;
    let attachment = sqlx::query_as!(
        crate::db::models::MediaAttachment,
        "SELECT * FROM media_attachments WHERE id = $1 AND account_id = $2 AND status_id IS NULL",
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    if attachment.processing == Some(FAILED) {
        return Err(MediaError::Invalid(PROCESSING_ERROR.into()));
    }
    Ok(attachment)
}

/// `status_code_for_media_attachment`: 206 while processing.
fn shown(state: &AppState, attachment: &crate::db::models::MediaAttachment) -> Response {
    let status = if not_processed(attachment) {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    (status, Json(media_from_db(&state.urls, attachment))).into_response()
}

pub async fn get_media(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> Response {
    match own_unattached(&state, &auth, id).await {
        Ok(attachment) => shown(&state, &attachment),
        Err(error) => error.into_response(),
    }
}

// ── PUT /api/v1/media/:id ─────────────────────────────────────────────────

pub async fn update_media(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    request: axum::extract::Request,
) -> Response {
    update(&state, &auth, id, request)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

async fn update(
    state: &AppState,
    auth: &AuthenticatedUser,
    id: i64,
    request: axum::extract::Request,
) -> Result<Response, MediaError> {
    use axum::extract::FromRequest;
    // A non-owner is answered 404 whatever the body is.
    let attachment = own_unattached(state, auth, id).await?;
    let Parts(parts) = Parts::from_request(request, state)
        .await
        .map_err(|_| MediaError::Invalid("could not read the parameters".into()))?;
    let params = media_params(parts);
    let kind = attachment.r#type.unwrap_or(TYPE_IMAGE);

    if let Some(thumbnail) = &params.thumbnail {
        check_image_dimension(thumbnail)?;
    }
    let errors = validate(
        None,
        params.thumbnail.as_ref(),
        params.description.as_deref(),
        kind,
        false,
    );
    if !errors.is_empty() {
        return Err(invalid(errors));
    }

    let mut files = Files {
        kind,
        meta: attachment
            .file_meta
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        blurhash: attachment.blurhash.clone(),
        thumbnail: attachment
            .thumbnail_file_name
            .clone()
            .filter(|name| !name.is_empty())
            .map(|file_name| StoredThumbnail {
                file_name,
                content_type: "",
                size: 0,
            }),
        ..Files::default()
    };
    let mut thumbnail_changed = false;
    if let Some(thumbnail) = params.thumbnail {
        let processed = process_thumbnail(thumbnail.data)
            .await
            .ok_or_else(|| not_identified("thumbnail"))?;
        replace_thumbnail(state, id, &mut files, processed).await?;
        thumbnail_changed = true;
    }
    let focus = params.focus.as_deref().and_then(parse_focus);
    if let Some(focus) = &focus {
        files.meta.insert("focus".into(), focus.clone());
    }
    let meta_changed = thumbnail_changed || focus.is_some();
    let thumbnail = files.thumbnail.as_ref().filter(|_| thumbnail_changed);
    let updated = sqlx::query_as!(
        crate::db::models::MediaAttachment,
        r#"UPDATE media_attachments
           SET description = COALESCE($2, description),
               file_meta = CASE WHEN $3 THEN $4::jsonb::json ELSE file_meta END,
               blurhash = CASE WHEN $5 THEN $6 ELSE blurhash END,
               thumbnail_file_name = CASE WHEN $5 THEN $7 ELSE thumbnail_file_name END,
               thumbnail_content_type = CASE WHEN $5 THEN $8 ELSE thumbnail_content_type END,
               thumbnail_file_size = CASE WHEN $5 THEN $9 ELSE thumbnail_file_size END,
               thumbnail_updated_at = CASE WHEN $5 THEN now() ELSE thumbnail_updated_at END,
               thumbnail_storage_schema_version =
                 CASE WHEN $5 THEN 1 ELSE thumbnail_storage_schema_version END,
               updated_at = now()
           WHERE id = $1
           RETURNING *"#,
        id,
        params.description,
        meta_changed,
        Value::Object(files.meta),
        thumbnail_changed,
        files.blurhash,
        thumbnail.map(|t| t.file_name.clone()),
        thumbnail.map(|t| t.content_type),
        thumbnail.map(|t| t.size),
    )
    .fetch_one(&state.db)
    .await?;
    Ok(shown(state, &updated))
}

// ── DELETE /api/v1/media/:id ──────────────────────────────────────────────

pub async fn delete_media(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Value>> {
    auth.require_scope("write:media")?;
    let attachment = sqlx::query_as!(
        crate::db::models::MediaAttachment,
        "SELECT * FROM media_attachments WHERE id = $1 AND account_id = $2",
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    if attachment.status_id.is_some() {
        return Err(AppError::Unprocessable(
            "Media attachment is currently used by a status".into(),
        ));
    }

    sqlx::query!("DELETE FROM media_attachments WHERE id = $1", id)
        .execute(&state.db)
        .await?;
    // Paperclip removes every style of the file and of its thumbnail.
    for key in crate::media::attachment_keys(
        attachment.id,
        attachment.file_file_name.as_deref(),
        attachment.thumbnail_file_name.as_deref(),
    ) {
        let _ = state.storage.delete(&key).await;
    }
    // `render_empty`.
    Ok(Json(json!({})))
}

/// Whether `data` is an ISO base media file whose brand is HEIF's or AVIF's,
/// which libvips reads with `heifload`. The brands are the ones libheif
/// recognises as an image rather than a video.
fn is_heif(data: &[u8]) -> bool {
    const BRANDS: &[&[u8; 4]] = &[
        b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1", b"msf1",
        b"avif", b"avis",
    ];
    if data.len() < 12 || &data[4..8] != b"ftyp" {
        return false;
    }
    let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let end = size.clamp(12, data.len());
    // The major brand, then the compatible brands after the minor version.
    std::iter::once(&data[8..12])
        .chain(
            data.get(16..end)
                .unwrap_or_default()
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| c.as_slice()),
        )
        .any(|brand| BRANDS.iter().any(|b| b.as_slice() == brand))
}

/// `MediaAttachment`'s `type` enum: `{ image: 0, gifv: 1, video: 2,
/// unknown: 3, audio: 4 }`.
pub const TYPE_IMAGE: i32 = 0;
pub const TYPE_GIFV: i32 = 1;
pub const TYPE_VIDEO: i32 = 2;
pub const TYPE_UNKNOWN: i32 = 3;
pub const TYPE_AUDIO: i32 = 4;

pub fn media_type_str(type_int: Option<i32>) -> &'static str {
    match type_int {
        Some(TYPE_IMAGE) => "image",
        Some(TYPE_GIFV) => "gifv",
        Some(TYPE_VIDEO) => "video",
        Some(TYPE_AUDIO) => "audio",
        _ => "unknown",
    }
}

// ── GET /media_proxy/:id/(*any) ───────────────────────────────────────────

/// `MediaProxyController#show`: an attachment of a post the viewer may see
/// (`MediaAttachment.attached.find`, `authorize :download?`), sent to where
/// its file is, the small version when the path ends in `/small`.
pub async fn media_proxy(
    state: AppState,
    Path(id): Path<i64>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<axum::response::Response> {
    proxy(&state, id, false, auth.map(|Extension(a)| a.account_id)).await
}

/// [`media_proxy`] with a path after the id (`/media_proxy/:id/small`).
pub async fn media_proxy_style(
    state: AppState,
    Path((id, style)): Path<(i64, String)>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<axum::response::Response> {
    // `preview_requested?`: `request.path.end_with?('/small')`.
    let small = style.ends_with("small");
    proxy(&state, id, small, auth.map(|Extension(a)| a.account_id)).await
}

async fn proxy(
    state: &AppState,
    id: i64,
    small: bool,
    viewer: Option<i64>,
) -> AppResult<axum::response::Response> {
    use axum::response::IntoResponse;
    // `authenticate_user!, if: :limited_federation_mode?`.
    if state.instance.limited_federation_mode && viewer.is_none() {
        return Err(AppError::Unauthorized);
    }
    // `MediaAttachment.attached.find`.
    let media = sqlx::query_as!(
        crate::db::models::MediaAttachment,
        "SELECT * FROM media_attachments
         WHERE id = $1 AND (status_id IS NOT NULL OR scheduled_status_id IS NOT NULL)",
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    // `record.status`, under the default scope, which leaves a discarded
    // status out.
    let status = match media.status_id {
        Some(status_id) => {
            sqlx::query_as!(
                crate::db::models::Status,
                "SELECT * FROM statuses WHERE id = $1",
                status_id,
            )
            .fetch_optional(&state.db)
            .await?
        }
        None => None,
    };
    // `discarded?`: its status discarded, or gone.
    let discarded =
        media.status_id.is_some() && status.as_ref().is_none_or(|s| s.deleted_at.is_some());
    // `download?`: `(discarded? && role.can?(:manage_reports)) ||
    // show_status?`. A refusal (`NotPermittedError`) is a 404.
    let shown = match status.as_ref().filter(|s| s.deleted_at.is_none()) {
        Some(status) => match viewer {
            Some(viewer) => super::statuses::check_status_visible(state, status, viewer)
                .await
                .is_ok(),
            None => super::statuses::check_status_public(state, status)
                .await
                .is_ok(),
        },
        None => false,
    };
    let moderates = match (discarded, viewer) {
        (true, Some(viewer)) => crate::moderation::role::acting(&state.db, viewer)
            .await?
            .can(&[crate::moderation::role::flag::MANAGE_REPORTS]),
        _ => false,
    };
    if !shown && !moderates {
        return Err(AppError::NotFound);
    }

    let has_file = media
        .file_file_name
        .as_deref()
        .is_some_and(|f| !f.is_empty());
    let url = if has_file {
        // `media_attachment_file`: the thumbnail for `/small` when there is
        // one, else the file in the style asked for.
        if small {
            super::convert::media_preview_url(&state.urls, &media)
        } else {
            super::convert::media_url(&state.urls, &media)
        }
    } else {
        let remote_url = media.remote_url.as_deref().filter(|u| !u.is_empty());
        // `reject_media?`: a remote account's, its domain blocked with
        // `reject_media`.
        let reject_media = match (media.account_id, remote_url) {
            (Some(account_id), Some(_)) => {
                crate::federation::moderation::account_media_rejected(state, account_id).await
            }
            _ => false,
        };
        if reject_media {
            // `needs_redownload? && !reject_media?` is false, so nothing is
            // fetched, and the redirect is to the file Paperclip has for no
            // file: its `missing.png`.
            Some(
                state
                    .urls
                    .missing_file_url(if small { "small" } else { "original" }),
            )
        } else if remote_url.is_some() && media.r#type != Some(TYPE_UNKNOWN) {
            // `redownload!`, which fetches the file and sends the viewer to
            // the copy. Eunha keeps no copies (a recorded divergence), so the
            // viewer is sent to where the copy would have come from.
            if small {
                media
                    .thumbnail_remote_url
                    .clone()
                    .filter(|u| !u.is_empty())
                    .or_else(|| remote_url.map(str::to_owned))
            } else {
                remote_url.map(str::to_owned)
            }
        } else {
            // A type Mastodon does not take fails the download's validation
            // (`RecordInvalid`), which is a 404, as is nothing to fetch.
            None
        }
    }
    .ok_or(AppError::NotFound)?;
    // `redirect_to`: a 302.
    Ok((
        axum::http::StatusCode::FOUND,
        [(axum::http::header::LOCATION, url)],
    )
        .into_response())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_is_read_as_ruby_reads_numbers() {
        assert_eq!(
            parse_focus("0.5,-0.25"),
            Some(json!({"x": 0.5, "y": -0.25}))
        );
        assert_eq!(parse_focus("abc,1"), Some(json!({"x": 0.0, "y": 1.0})));
        assert_eq!(parse_focus("0.3"), Some(json!({"x": 0.3, "y": null})));
        assert_eq!(parse_focus(""), None);
    }

    #[test]
    fn a_file_keeps_its_own_extension_when_it_is_its_types() {
        assert_eq!(extension("image/jpeg", Some("photo.JPG")), "jpg");
        assert_eq!(extension("image/jpeg", Some("photo.png")), "jpeg");
        assert_eq!(extension("image/jpeg", None), "jpeg");
        assert_eq!(extension("video/mp4", Some("clip.mov")), "mp4");
    }

    #[test]
    fn uploads_are_validated_in_mastodons_order_with_its_messages() {
        let upload = |content_type: &str, size: usize| Upload {
            content_type: content_type.into(),
            data: vec![0; size],
            file_name: None,
        };
        let pdf = upload("application/pdf", IMAGE_LIMIT);
        let errors = validate(Some(&pdf), None, None, type_for("application/pdf"), true);
        assert_eq!(
            errors.message(),
            "Validation failed: File content type is invalid, File is invalid, \
             File file size must be less than 16 MB, File must be less than 16 MB"
        );
        let video = upload("video/mp4", IMAGE_LIMIT);
        assert!(validate(Some(&video), None, None, TYPE_VIDEO, true).is_empty());
        let image = upload("image/png", 10);
        let thumbnail = upload("image/png", 10);
        assert_eq!(
            validate(Some(&image), Some(&thumbnail), None, TYPE_IMAGE, true).message(),
            "Validation failed: Thumbnail must be blank"
        );
        assert_eq!(
            validate(None, None, None, TYPE_IMAGE, true).message(),
            "Validation failed: File can't be blank"
        );
    }
}
