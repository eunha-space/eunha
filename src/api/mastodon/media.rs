use super::{convert::media_from_db, types::MediaAttachment};
use crate::media::picture::{self, Fit, Picture};
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};
use axum::{
    extract::{Extension, Multipart, Path},
    Json,
};
use image::ImageFormat;

/// `MediaAttachment::IMAGE_STYLES[:original]`: 3840×2160.
const ORIGINAL_PIXELS: u64 = 8_294_400;
/// `MediaAttachment::IMAGE_STYLES[:small]`: 640×360.
const SMALL_PIXELS: u64 = 230_400;

// ── POST /api/v1/media, POST /api/v2/media ────────────────────────────────

pub async fn upload_media(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    mut multipart: Multipart,
) -> AppResult<axum::response::Response> {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    auth.require_scope("write:media")?;
    let mut file_field: Option<(String, String, Vec<u8>)> = None;
    let mut description: Option<String> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::Unprocessable(e.to_string()))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                let filename = field.file_name().unwrap_or("upload").to_string();
                let content_type = field
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_string();
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| AppError::Unprocessable(e.to_string()))?;
                file_field = Some((filename, content_type, data.to_vec()));
            }
            "description" => {
                let text = field
                    .text()
                    .await
                    .map_err(|e| AppError::Unprocessable(e.to_string()))?;
                description = Some(text);
            }
            _ => {}
        }
    }

    let (_, content_type, data) =
        file_field.ok_or_else(|| AppError::Unprocessable("missing file field".into()))?;
    let media_type = classify_media_type(&content_type);
    // Mastodon 4.7.2 no longer lets libvips load HEIF ("Temporarily disable
    // HEIF support"), so processing a HEIC, HEIF or AVIF image fails, whatever
    // type it was declared as, when the file is assigned and before any
    // validation; `rescue Paperclip::Error` answers it. The types are still
    // advertised.
    if media_type == "image" && is_heif(&data) {
        tracing::warn!("refusing a HEIF upload, as Mastodon 4.7.2 does");
        return Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "Error processing thumbnail for uploaded media",
            })),
        )
            .into_response());
    }
    validate_media_description(description.as_deref())?;
    let media_id = crate::snowflake::next_id();

    // Video / gifv / audio: transcode in the background (Mastodon's "larger media
    // formats"). Insert a processing row with no file yet — `url` stays null
    // until processing completes — and return 202 for the client to poll.
    if matches!(media_type, "video" | "gifv" | "audio") {
        // Store the source durably and enqueue a transcode job. The worker
        // (background.rs) drains the queue; this survives restarts.
        let src_ext = crate::media::ext_for_content_type(&content_type);
        let source_key = format!(
            "media_attachments/files/{}/source/source.{}",
            crate::media::int_to_path(media_id),
            src_ext
        );
        state
            .storage
            .store(&data, &source_key, &content_type)
            .await?;

        let attachment = sqlx::query_as!(
            crate::db::models::MediaAttachment,
            r#"INSERT INTO media_attachments
                 (id, account_id, "type", description, processing, created_at, updated_at)
               VALUES ($1,$2,$3,$4,1, now(), now())
               RETURNING *"#,
            media_id,
            auth.account_id,
            media_type_int(media_type),
            description,
        )
        .fetch_one(&state.db)
        .await?;

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

        return Ok((
            StatusCode::ACCEPTED,
            Json(media_from_db(&state.urls, &attachment)),
        )
            .into_response());
    }

    // Images: process synchronously and return 200. Decoding, turning
    // upright, re-encoding and blurhashing are CPU work that would hold a Tokio
    // worker for as long as they take, stalling every request scheduled on it —
    // every tenant's, when a process serves several.
    let processed = crate::tenants::spawn_blocking(move || process_image(data))
        .await
        .map_err(|e| anyhow::anyhow!("image processing did not finish: {e}"))?;
    let content_type = processed
        .content_type
        .map(str::to_owned)
        .unwrap_or(content_type);
    let file_filename = format!(
        "original.{}",
        crate::media::ext_for_content_type(&content_type)
    );
    let file_key = format!(
        "media_attachments/files/{}/original/{}",
        crate::media::int_to_path(media_id),
        file_filename
    );
    let data = processed.original;
    state.storage.store(&data, &file_key, &content_type).await?;

    let (file_meta, blurhash, thumbnail_file_name) =
        match (processed.original_meta, processed.small) {
            (Some(original_meta), Some(small)) => {
                let small_filename = format!(
                    "small.{}",
                    crate::media::ext_for_content_type(small.content_type)
                );
                let small_key = format!(
                    "media_attachments/files/{}/small/{}",
                    crate::media::int_to_path(media_id),
                    small_filename
                );
                state
                    .storage
                    .store(&small.bytes, &small_key, small.content_type)
                    .await?;
                let meta = serde_json::json!({ "original": original_meta, "small": small.meta });
                (Some(meta), Some(small.blurhash), Some(small_filename))
            }
            _ => (None, None, None),
        };

    let file_size = data.len() as i32;
    let attachment = sqlx::query_as!(
        crate::db::models::MediaAttachment,
        r#"INSERT INTO media_attachments
             (id, account_id, "type", file_file_name, file_content_type, file_file_size, thumbnail_file_name, description, file_meta, blurhash, created_at, updated_at)
           VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10, now(), now())
           RETURNING *"#,
        media_id,
        auth.account_id,
        media_type_int(media_type),
        file_filename,
        content_type,
        file_size,
        thumbnail_file_name,
        description,
        file_meta,
        blurhash,
    )
    .fetch_one(&state.db)
    .await?;

    Ok((
        StatusCode::OK,
        Json(media_from_db(&state.urls, &attachment)),
    )
        .into_response())
}

/// Transcode a video/gifv/audio upload (ffmpeg), store the result + thumbnail,
/// and mark the attachment complete.
async fn process_media(
    state: &AppState,
    media_id: i64,
    data: &[u8],
    media_type: &str,
) -> anyhow::Result<()> {
    let transcoded = crate::media::transcode::transcode(data, media_type).await?;
    let orig_filename = format!("original.{}", transcoded.ext);
    let orig_key = format!(
        "media_attachments/files/{}/original/{}",
        crate::media::int_to_path(media_id),
        orig_filename
    );
    state
        .storage
        .store(&transcoded.bytes, &orig_key, transcoded.content_type)
        .await
        .map_err(|e| anyhow::anyhow!("store original: {e}"))?;

    let mut file_meta = serde_json::json!({ "original": transcoded.meta });
    let mut thumbnail_filename: Option<String> = None;
    let mut thumbnail_ct: Option<String> = None;
    let mut blurhash: Option<String> = None;

    if media_type == "video" || media_type == "gifv" {
        if let Ok(frame) = crate::media::transcode::extract_frame(data).await {
            let small = crate::tenants::spawn_blocking(move || {
                Picture::decode(&frame).and_then(|picture| thumbnail(&picture, ImageFormat::Jpeg))
            })
            .await
            .map_err(|e| anyhow::anyhow!("frame processing did not finish: {e}"))?;
            if let Some(small) = small {
                let small_filename = "small.jpg".to_string();
                let small_key = format!(
                    "media_attachments/files/{}/small/{}",
                    crate::media::int_to_path(media_id),
                    small_filename
                );
                state
                    .storage
                    .store(&small.bytes, &small_key, small.content_type)
                    .await
                    .map_err(|e| anyhow::anyhow!("store thumbnail: {e}"))?;
                file_meta["small"] = small.meta;
                thumbnail_filename = Some(small_filename);
                thumbnail_ct = Some(small.content_type.to_string());
                blurhash = Some(small.blurhash);
            }
        }
    }

    let file_size = transcoded.bytes.len() as i32;
    sqlx::query!(
        r#"UPDATE media_attachments
             SET file_file_name = $2, file_content_type = $3, file_file_size = $4,
                 thumbnail_file_name = $5, thumbnail_content_type = $6,
                 file_meta = $7, blurhash = $8, processing = 2, updated_at = now()
           WHERE id = $1"#,
        media_id,
        orig_filename,
        transcoded.content_type,
        file_size,
        thumbnail_filename,
        thumbnail_ct,
        file_meta,
        blurhash,
    )
    .execute(&state.db)
    .await?;

    Ok(())
}

/// An image upload as it is stored.
struct ProcessedImage {
    original: Vec<u8>,
    /// What `original` was re-encoded as; `None` when it is the upload's own
    /// bytes, less what metadata could be removed from them.
    content_type: Option<&'static str>,
    original_meta: Option<serde_json::Value>,
    small: Option<Thumbnail>,
}

struct Thumbnail {
    bytes: Vec<u8>,
    content_type: &'static str,
    meta: serde_json::Value,
    blurhash: String,
}

/// Turn an image upload upright, cap it at Mastodon's original size, and make
/// its small thumbnail and blurhash, the way `MediaAttachment`'s image styles
/// do. See [`crate::media::picture`] for why the original is re-encoded.
///
/// CPU-bound: call it from the blocking pool, never on a Tokio worker.
fn process_image(data: Vec<u8>) -> ProcessedImage {
    let Some(picture) = Picture::decode(&data) else {
        return ProcessedImage {
            original: picture::strip_metadata(&data),
            content_type: None,
            original_meta: None,
            small: None,
        };
    };
    let small = thumbnail(&picture, picture.thumbnail_format());
    match picture.original(&data, Fit::Pixels(ORIGINAL_PIXELS)) {
        Some(original) => ProcessedImage {
            original_meta: Some(image_dim_json(original.width(), original.height())),
            content_type: Some(original.content_type()),
            original: original.bytes,
            small,
        },
        None => ProcessedImage {
            original_meta: Some(image_dim_json(picture.width(), picture.height())),
            content_type: None,
            original: data,
            small,
        },
    }
}

/// The 230,400-pixel `small` style, blurhashed from itself as Mastodon's
/// `BlurhashTranscoder` does.
fn thumbnail(picture: &Picture, format: ImageFormat) -> Option<Thumbnail> {
    let small = picture.rendition(Fit::Pixels(SMALL_PIXELS), format)?;
    let (width, height) = (small.width(), small.height());
    let blurhash = blurhash::encode(4, 4, width, height, small.image.to_rgba8().as_raw()).ok()?;
    Some(Thumbnail {
        content_type: small.content_type(),
        meta: image_dim_json(width, height),
        bytes: small.bytes,
        blurhash,
    })
}

fn image_dim_json(w: u32, h: u32) -> serde_json::Value {
    serde_json::json!({
        "width": w,
        "height": h,
        "size": format!("{}x{}", w, h),
        "aspect": w as f64 / h as f64,
    })
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

async fn run_media_queue_batch(state: &AppState, worker_id: &str) -> anyhow::Result<usize> {
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

async fn process_media_job(
    state: &AppState,
    id: i64,
    media_id: i64,
    media_type: &str,
    source_key: &str,
    attempts: i32,
    max_attempts: i32,
) {
    let result = async {
        let data = state
            .storage
            .get(source_key)
            .await
            .map_err(|e| anyhow::anyhow!("fetch source: {e}"))?;
        process_media(state, media_id, &data, media_type).await
    }
    .await;

    match result {
        Ok(()) => {
            let _ = state.storage.delete(source_key).await;
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
                    "UPDATE media_attachments SET processing = 3, updated_at = now() WHERE id = $1",
                    media_id,
                )
                .execute(&state.db)
                .await;
                let _ = state.storage.delete(source_key).await;
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

pub async fn get_media(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<MediaAttachment>> {
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

    Ok(Json(media_from_db(&state.urls, &attachment)))
}

// ── PUT /api/v1/media/:id ─────────────────────────────────────────────────

pub async fn update_media(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    request: axum::extract::Request,
) -> AppResult<Json<MediaAttachment>> {
    auth.require_scope("write:media")?;
    // Verify ownership before touching the body so a non-owner gets 404 even
    // when the body format is unexpected.
    sqlx::query!(
        "SELECT id FROM media_attachments WHERE id = $1 AND account_id = $2 AND status_id IS NULL",
        id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let mut description: Option<String> = None;
    let mut focus: Option<String> = None;

    // Mastodon accepts both multipart/form-data and JSON bodies here.
    let is_multipart = request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("multipart/form-data"));

    if is_multipart {
        use axum::extract::FromRequest;
        let mut multipart = Multipart::from_request(request, &state)
            .await
            .map_err(|e| AppError::Unprocessable(e.to_string()))?;
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|e| AppError::Unprocessable(e.to_string()))?
        {
            let name = field.name().unwrap_or("").to_string();
            match name.as_str() {
                "description" => {
                    description = Some(
                        field
                            .text()
                            .await
                            .map_err(|e| AppError::Unprocessable(e.to_string()))?,
                    );
                }
                "focus" => {
                    focus = Some(
                        field
                            .text()
                            .await
                            .map_err(|e| AppError::Unprocessable(e.to_string()))?,
                    );
                }
                _ => {}
            }
        }
    } else {
        let bytes = axum::body::to_bytes(request.into_body(), 64 * 1024)
            .await
            .map_err(|e| AppError::Unprocessable(e.to_string()))?;
        let body: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or_else(|_| serde_json::json!({}));
        description = body
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        focus = body
            .get("focus")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
    }

    validate_media_description(description.as_deref())?;
    if let Some(ref desc) = description {
        sqlx::query!(
            "UPDATE media_attachments SET description = $1 WHERE id = $2",
            desc,
            id,
        )
        .execute(&state.db)
        .await?;
    }

    if let Some(ref focus_str) = focus {
        // Parse "x,y" format into { focus: { x, y } } and merge into meta
        if let Some((x_str, y_str)) = focus_str.split_once(',') {
            if let (Ok(x), Ok(y)) = (x_str.trim().parse::<f64>(), y_str.trim().parse::<f64>()) {
                let current = sqlx::query_scalar!(
                    "SELECT file_meta FROM media_attachments WHERE id = $1",
                    id,
                )
                .fetch_one(&state.db)
                .await?;
                let mut meta = current.unwrap_or(serde_json::json!({}));
                if let Some(obj) = meta.as_object_mut() {
                    obj.insert("focus".to_string(), serde_json::json!({ "x": x, "y": y }));
                }
                sqlx::query!(
                    "UPDATE media_attachments SET file_meta = $1 WHERE id = $2",
                    meta,
                    id,
                )
                .execute(&state.db)
                .await?;
            }
        }
    }

    let updated = sqlx::query_as!(
        crate::db::models::MediaAttachment,
        "SELECT * FROM media_attachments WHERE id = $1",
        id,
    )
    .fetch_one(&state.db)
    .await?;

    Ok(Json(media_from_db(&state.urls, &updated)))
}

// ── DELETE /api/v1/media/:id ──────────────────────────────────────────────

pub async fn delete_media(
    state: AppState,
    Path(id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<axum::http::StatusCode> {
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

    sqlx::query!("DELETE FROM media_attachments WHERE id = $1", id,)
        .execute(&state.db)
        .await?;

    // Delete file from S3 using computed key from file_file_name
    if let Some(filename) = &attachment.file_file_name {
        if !filename.is_empty() {
            let key = format!(
                "media_attachments/files/{}/original/{}",
                crate::media::int_to_path(attachment.id),
                filename
            );
            let _ = state.storage.delete(&key).await;
        }
    }

    Ok(axum::http::StatusCode::OK)
}

/// Reject media descriptions over Mastodon's `MediaAttachment::MAX_DESCRIPTION_LENGTH`.
fn validate_media_description(description: Option<&str>) -> AppResult<()> {
    if let Some(desc) = description {
        if desc.chars().count() > 10_000 {
            return Err(AppError::Unprocessable(
                "Validation failed: Description is too long (maximum is 10000 characters)".into(),
            ));
        }
    }
    Ok(())
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

fn classify_media_type(content_type: &str) -> &'static str {
    if content_type.starts_with("image/gif") {
        "gifv"
    } else if content_type.starts_with("image/") {
        "image"
    } else if content_type.starts_with("video/") {
        "video"
    } else if content_type.starts_with("audio/") {
        "audio"
    } else {
        "unknown"
    }
}

/// `MediaAttachment`'s `type` enum: `{ image: 0, gifv: 1, video: 2,
/// unknown: 3, audio: 4 }`.
pub const TYPE_IMAGE: i32 = 0;
pub const TYPE_GIFV: i32 = 1;
pub const TYPE_VIDEO: i32 = 2;
pub const TYPE_UNKNOWN: i32 = 3;
pub const TYPE_AUDIO: i32 = 4;

fn media_type_int(mt: &str) -> i32 {
    match mt {
        "image" => TYPE_IMAGE,
        "gifv" => TYPE_GIFV,
        "video" => TYPE_VIDEO,
        "audio" => TYPE_AUDIO,
        _ => TYPE_UNKNOWN,
    }
}

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
