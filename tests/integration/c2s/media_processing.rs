//! Media uploads as `MediaAttachment` validates, processes and stores them,
//! and the media API's answers while they are processed.

use reqwest::StatusCode;
use serde_json::{json, Value};

use super::profile_images::animated_gif;
use crate::helpers::{tiny_png, TestContext};

/// A file ffmpeg makes from `args` (its inputs and options), as `ext`.
fn ffmpeg(args: &[&str], ext: &str) -> Vec<u8> {
    let out = std::env::temp_dir().join(format!(
        "eunha-test-{}-{}.{ext}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let status = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y"])
        .args(args)
        .arg(&out)
        .status()
        .expect("ffmpeg runs");
    assert!(status.success(), "ffmpeg {args:?}");
    let bytes = std::fs::read(&out).unwrap();
    let _ = std::fs::remove_file(&out);
    bytes
}

/// A second of 64×48 video at 10 frames a second, with a tone.
fn video() -> Vec<u8> {
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=64x48:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440",
            "-t",
            "1",
            "-c:v",
            "mpeg4",
            "-c:a",
            "aac",
        ],
        "mp4",
    )
}

/// A second of WAV audio.
fn audio() -> Vec<u8> {
    ffmpeg(
        &["-f", "lavfi", "-i", "sine=frequency=440", "-t", "1"],
        "wav",
    )
}

/// A second of MP3 with a 32×32 red cover.
fn audio_with_cover() -> Vec<u8> {
    ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:size=32x32",
            "-t",
            "1",
            "-map",
            "0:a",
            "-map",
            "1:v",
            "-frames:v",
            "1",
            "-c:v",
            "png",
            "-disposition:v",
            "attached_pic",
            "-id3v2_version",
            "3",
        ],
        "mp3",
    )
}

async fn upload(
    ctx: &TestContext,
    path: &str,
    files: &[(&'static str, &str, &'static str, Vec<u8>)],
    texts: &[(&'static str, &str)],
) -> reqwest::Response {
    let mut form = reqwest::multipart::Form::new();
    for (name, file_name, content_type, data) in files {
        form = form.part(
            *name,
            reqwest::multipart::Part::bytes(data.clone())
                .file_name(file_name.to_string())
                .mime_str(content_type)
                .unwrap(),
        );
    }
    for (name, text) in texts {
        form = form.text(*name, text.to_string());
    }
    ctx.api
        .http
        .post(ctx.api.url(path))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap()
}

/// The object key of a URL the fake bucket serves.
fn key(url: &str) -> String {
    url[url.find("media_attachments/").unwrap()..].to_owned()
}

struct Row {
    file_file_name: Option<String>,
    file_content_type: Option<String>,
    thumbnail_file_name: Option<String>,
    processing: Option<i32>,
    kind: i32,
}

async fn row(ctx: &TestContext, id: &str) -> Row {
    let (file_file_name, file_content_type, thumbnail_file_name, processing, kind) =
        sqlx::query_as(
            "SELECT file_file_name, file_content_type, thumbnail_file_name, processing, type
             FROM media_attachments WHERE id = $1",
        )
        .bind(id.parse::<i64>().unwrap())
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    Row {
        file_file_name,
        file_content_type,
        thumbnail_file_name,
        processing,
        kind,
    }
}

/// An image is processed as it is uploaded and recorded `complete`, under a
/// name of sixteen hex digits, its small style beside it under the same name.
#[tokio::test]
async fn test_an_image_is_stored_as_paperclip_stores_it() {
    let ctx = TestContext::new("media-image-files").await;
    let resp = upload(
        &ctx,
        "/api/v1/media",
        &[("file", "photo.png", "image/png", tiny_png())],
        &[("focus", "0.5,-0.25")],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let media: Value = resp.json().await.unwrap();
    let id = media["id"].as_str().unwrap();
    let row = row(&ctx, id).await;
    assert_eq!(row.processing, Some(2));
    assert_eq!(row.thumbnail_file_name, None);
    let name = row.file_file_name.unwrap();
    assert_eq!(name.len(), "0123456789abcdef.png".len(), "{name}");
    assert!(name.ends_with(".png"));
    let url = media["url"].as_str().unwrap();
    let preview = media["preview_url"].as_str().unwrap();
    assert!(url.ends_with(&format!("/original/{name}")), "{url}");
    assert!(preview.ends_with(&format!("/small/{name}")), "{preview}");
    assert!(!ctx
        .state
        .storage
        .get(&key(preview))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(media["meta"]["focus"], json!({"x": 0.5, "y": -0.25}));
}

/// Only Mastodon's image, video and audio types are taken, with Rails'
/// messages.
#[tokio::test]
async fn test_an_unsupported_type_is_refused() {
    let ctx = TestContext::new("media-type-refused").await;
    let resp = upload(
        &ctx,
        "/api/v1/media",
        &[("file", "a.pdf", "application/pdf", b"%PDF-1.4".to_vec())],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Validation failed: File content type is invalid, File is invalid"
    );
}

/// `IMAGE_LIMIT`: an image under 16 MB, whose upload the server still
/// takes in to refuse.
#[tokio::test]
async fn test_an_image_of_sixteen_megabytes_is_refused() {
    let ctx = TestContext::new("media-image-size").await;
    let mut data = tiny_png();
    data.resize(16 * 1024 * 1024, 0);
    let resp = upload(
        &ctx,
        "/api/v1/media",
        &[("file", "big.png", "image/png", data)],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Validation failed: File file size must be less than 16 MB, File must be less than 16 MB"
    );
}

/// Bytes that are no image are refused rather than stored as one.
#[tokio::test]
async fn test_an_undecodable_image_is_refused() {
    let ctx = TestContext::new("media-undecodable").await;
    let resp = upload(
        &ctx,
        "/api/v1/media",
        &[("file", "a.png", "image/png", b"not a picture".to_vec())],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Validation failed: File Paperclip::Errors::NotIdentifiedByImageMagickError"
    );
}

/// A still GIF stays an image; an animated one becomes a gifv, an MP4 with
/// a PNG small style, its `original` read from the MP4.
#[tokio::test]
async fn test_gifs_are_images_unless_they_move() {
    let ctx = TestContext::new("media-gifs").await;
    let resp = upload(
        &ctx,
        "/api/v1/media",
        &[("file", "still.gif", "image/gif", animated_gif(20, 10, 1))],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let still: Value = resp.json().await.unwrap();
    assert_eq!(still["type"], "image");
    assert!(still["url"].as_str().unwrap().ends_with(".gif"));
    let preview = still["preview_url"].as_str().unwrap();
    assert!(preview.ends_with(".png"), "{preview}");
    // `GifTranscoder` leaves it as it is, under that name all the same.
    let small = ctx.state.storage.get(&key(preview)).await.unwrap();
    assert!(small.starts_with(b"GIF8"));
    assert_eq!(still["meta"]["original"]["width"], 20);
    assert_eq!(still["meta"]["small"]["width"], 20);

    let resp = upload(
        &ctx,
        "/api/v1/media",
        &[("file", "moving.gif", "image/gif", animated_gif(64, 48, 3))],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let moving: Value = resp.json().await.unwrap();
    assert_eq!(moving["type"], "gifv");
    let row = row(&ctx, moving["id"].as_str().unwrap()).await;
    assert_eq!(row.file_content_type.as_deref(), Some("video/mp4"));
    assert!(moving["url"].as_str().unwrap().ends_with(".mp4"));
    assert!(moving["preview_url"].as_str().unwrap().ends_with(".png"));
    assert_eq!(moving["meta"]["original"]["width"], 64);
    assert_eq!(moving["meta"]["original"]["frame_rate"], "10/1");
    assert!(moving["meta"]["original"].get("size").is_none());
}

/// v1 processes a video before it answers: an MP4, its first frame as a
/// PNG small style, and `original` as `video_metadata` writes it.
#[tokio::test]
async fn test_a_video_is_processed_before_v1_answers() {
    let ctx = TestContext::new("media-video-v1").await;
    let resp = upload(
        &ctx,
        "/api/v1/media",
        &[("file", "clip.mp4", "video/mp4", video())],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let media: Value = resp.json().await.unwrap();
    assert_eq!(media["type"], "video");
    let row = row(&ctx, media["id"].as_str().unwrap()).await;
    assert_eq!(row.processing, Some(2));
    assert!(media["url"].as_str().unwrap().ends_with(".mp4"));
    let preview = media["preview_url"].as_str().unwrap();
    assert!(preview.ends_with(".png"), "{preview}");
    let png = ctx.state.storage.get(&key(preview)).await.unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    let original = &media["meta"]["original"];
    assert_eq!(original["width"], 64);
    assert_eq!(original["height"], 48);
    assert_eq!(original["frame_rate"], "10/1");
    assert!(original["duration"].as_f64().unwrap() > 0.5);
    assert_eq!(media["meta"]["small"]["width"], 64);
    assert!(media["blurhash"].is_string());
}

/// A video with no sound is a gifv, as `Transcoder` makes it.
#[tokio::test]
async fn test_a_silent_video_is_a_gifv() {
    let ctx = TestContext::new("media-silent").await;
    let silent = ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=32x32:rate=10",
            "-t",
            "1",
            "-c:v",
            "mpeg4",
        ],
        "mp4",
    );
    let resp = upload(
        &ctx,
        "/api/v1/media",
        &[("file", "loop.mp4", "video/mp4", silent)],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let media: Value = resp.json().await.unwrap();
    assert_eq!(media["type"], "gifv");
}

/// `check_video_dimensions`: no more than 3840×2160 pixels, and no more
/// than 120 frames a second.
#[tokio::test]
async fn test_a_video_too_fast_or_too_large_is_refused() {
    let ctx = TestContext::new("media-video-dimensions").await;
    let fast = ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=32x32:rate=150",
            "-t",
            "0.1",
            "-c:v",
            "mpeg4",
        ],
        "mp4",
    );
    let large = ffmpeg(
        &[
            "-f",
            "lavfi",
            "-i",
            "color=size=4000x2200:rate=1",
            "-frames:v",
            "1",
            "-c:v",
            "mpeg4",
        ],
        "mp4",
    );
    for (data, message) in [
        (fast, "150fps videos are not supported"),
        (large, "4000x2200 videos are not supported"),
    ] {
        let resp = upload(
            &ctx,
            "/api/v2/media",
            &[("file", "clip.mp4", "video/mp4", data)],
            &[],
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"], message);
    }
}

/// v2 leaves a video to the queue: 202 and `queued`, no `url` but its
/// preview already made, 206 from GET until it is done.
#[tokio::test]
async fn test_a_video_waits_for_the_queue_on_v2() {
    let ctx = TestContext::new("media-video-v2").await;
    let resp = upload(
        &ctx,
        "/api/v2/media",
        &[("file", "clip.webm", "video/webm", video())],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let media: Value = resp.json().await.unwrap();
    let id = media["id"].as_str().unwrap();
    assert_eq!(media["url"], Value::Null);
    assert!(media["preview_url"].as_str().unwrap().ends_with(".png"));
    let queued = row(&ctx, id).await;
    assert_eq!(queued.processing, Some(0));
    let source = queued.file_file_name.unwrap();
    assert!(source.ends_with(".webm"), "{source}");

    let resp = ctx
        .api
        .get(&format!("/api/v1/media/{id}"), Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);

    eunha::api::mastodon::media::run_media_queue_batch(&ctx.state, "test")
        .await
        .unwrap();
    let resp = ctx
        .api
        .get(&format!("/api/v1/media/{id}"), Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let done: Value = resp.json().await.unwrap();
    let processed = row(&ctx, id).await;
    assert_eq!(processed.processing, Some(2));
    let name = processed.file_file_name.unwrap();
    assert_eq!(name, source.replace(".webm", ".mp4"));
    assert!(done["url"].as_str().unwrap().ends_with(&name));
    // The original as it came is gone.
    let partition = eunha::media::int_to_path(id.parse().unwrap());
    let raw = format!("media_attachments/files/{partition}/original/{source}");
    assert!(ctx.state.storage.get(&raw).await.unwrap().is_empty());
}

/// Audio is transcoded to MP3, and has no preview without a cover.
#[tokio::test]
async fn test_audio_is_an_mp3_without_a_preview() {
    let ctx = TestContext::new("media-audio").await;
    let resp = upload(
        &ctx,
        "/api/v1/media",
        &[("file", "tone.wav", "audio/wav", audio())],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let media: Value = resp.json().await.unwrap();
    assert_eq!(media["type"], "audio");
    let row = row(&ctx, media["id"].as_str().unwrap()).await;
    assert_eq!(row.kind, 4);
    assert_eq!(row.file_content_type.as_deref(), Some("audio/mpeg"));
    assert!(media["url"].as_str().unwrap().ends_with(".mp3"));
    assert_eq!(media["preview_url"], Value::Null);
    assert!(media["meta"]["original"]["duration"].as_f64().unwrap() > 0.5);
}

/// An audio file's cover is its thumbnail (`ImageExtractor`), with the
/// colours `ColorExtractor` finds in it.
#[tokio::test]
async fn test_audio_cover_art_is_its_thumbnail() {
    let ctx = TestContext::new("media-audio-cover").await;
    let resp = upload(
        &ctx,
        "/api/v1/media",
        &[("file", "song.mp3", "audio/mpeg", audio_with_cover())],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let media: Value = resp.json().await.unwrap();
    let row = row(&ctx, media["id"].as_str().unwrap()).await;
    let thumbnail = row.thumbnail_file_name.unwrap();
    let preview = media["preview_url"].as_str().unwrap();
    assert!(
        preview.ends_with(&format!("/thumbnails/{}/original/{thumbnail}", {
            eunha::media::int_to_path(media["id"].as_str().unwrap().parse().unwrap())
        })),
        "{preview}"
    );
    assert_eq!(media["meta"]["small"]["width"], 32);
    assert_eq!(media["meta"]["colors"]["background"], "#f30c0c");
    assert!(media["blurhash"].is_string());
}

/// A thumbnail is taken for audio and video, at create and update alike,
/// and refused for anything else.
#[tokio::test]
async fn test_a_thumbnail_is_for_audio_and_video_only() {
    let ctx = TestContext::new("media-thumbnail").await;
    let resp = upload(
        &ctx,
        "/api/v1/media",
        &[
            ("file", "a.png", "image/png", tiny_png()),
            ("thumbnail", "t.png", "image/png", tiny_png()),
        ],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "Validation failed: Thumbnail must be blank");

    let resp = upload(
        &ctx,
        "/api/v1/media",
        &[
            ("file", "tone.wav", "audio/wav", audio()),
            ("thumbnail", "t.png", "image/png", tiny_png()),
        ],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let media: Value = resp.json().await.unwrap();
    let id = media["id"].as_str().unwrap();
    let first = row(&ctx, id).await.thumbnail_file_name.unwrap();
    assert!(media["preview_url"].as_str().unwrap().ends_with(&first));

    // Replaced by an update, the old one removed.
    let form = reqwest::multipart::Form::new().part(
        "thumbnail",
        reqwest::multipart::Part::bytes(crate::helpers::sideways_jpeg())
            .file_name("t.jpg")
            .mime_str("image/jpeg")
            .unwrap(),
    );
    let resp = ctx
        .api
        .http
        .put(ctx.api.url(&format!("/api/v1/media/{id}")))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let updated: Value = resp.json().await.unwrap();
    let second = row(&ctx, id).await.thumbnail_file_name.unwrap();
    assert_ne!(first, second);
    assert!(updated["preview_url"].as_str().unwrap().ends_with(&second));
    assert_eq!(updated["meta"]["small"]["width"], 32);
    let partition = eunha::media::int_to_path(id.parse().unwrap());
    let old = format!("media_attachments/thumbnails/{partition}/original/{first}");
    assert!(ctx.state.storage.get(&old).await.unwrap().is_empty());
}

/// GET and PUT answer 206 while processing waits, and `processing_error`
/// once it has failed.
#[tokio::test]
async fn test_show_answers_by_processing_state() {
    let ctx = TestContext::new("media-processing-state").await;
    let media: Value = upload(
        &ctx,
        "/api/v1/media",
        &[("file", "a.png", "image/png", tiny_png())],
        &[],
    )
    .await
    .json()
    .await
    .unwrap();
    let id = media["id"].as_str().unwrap();
    for (processing, status) in [
        (0, StatusCode::PARTIAL_CONTENT),
        (1, StatusCode::PARTIAL_CONTENT),
        (2, StatusCode::OK),
    ] {
        sqlx::query("UPDATE media_attachments SET processing = $2 WHERE id = $1")
            .bind(id.parse::<i64>().unwrap())
            .bind(processing)
            .execute(&ctx.db)
            .await
            .unwrap();
        let resp = ctx
            .api
            .get(&format!("/api/v1/media/{id}"), Some(&ctx.alice_token))
            .await;
        assert_eq!(resp.status(), status, "processing {processing}");
        let resp = ctx
            .api
            .put_json(
                &format!("/api/v1/media/{id}"),
                Some(&ctx.alice_token),
                &json!({ "description": "d" }),
            )
            .await;
        assert_eq!(resp.status(), status, "processing {processing}");
    }
    sqlx::query("UPDATE media_attachments SET processing = 3 WHERE id = $1")
        .bind(id.parse::<i64>().unwrap())
        .execute(&ctx.db)
        .await
        .unwrap();
    let resp = ctx
        .api
        .get(&format!("/api/v1/media/{id}"), Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Error processing thumbnail for uploaded media"
    );
}

/// DELETE answers `{}` and removes every file the attachment had.
#[tokio::test]
async fn test_deleting_media_removes_its_files() {
    let ctx = TestContext::new("media-delete").await;
    let media: Value = upload(
        &ctx,
        "/api/v1/media",
        &[
            ("file", "clip.mp4", "video/mp4", video()),
            ("thumbnail", "t.png", "image/png", tiny_png()),
        ],
        &[],
    )
    .await
    .json()
    .await
    .unwrap();
    let id = media["id"].as_str().unwrap();
    let row = row(&ctx, id).await;
    let partition = eunha::media::int_to_path(id.parse().unwrap());
    let name = row.file_file_name.unwrap();
    let stem = name.trim_end_matches(".mp4");
    let keys = [
        format!("media_attachments/files/{partition}/original/{name}"),
        format!("media_attachments/files/{partition}/small/{stem}.png"),
        format!(
            "media_attachments/thumbnails/{partition}/original/{}",
            row.thumbnail_file_name.unwrap()
        ),
    ];
    for key in &keys {
        assert!(
            !ctx.state.storage.get(key).await.unwrap().is_empty(),
            "{key}"
        );
    }

    let resp = ctx
        .api
        .delete(&format!("/api/v1/media/{id}"), &ctx.alice_token)
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.json::<Value>().await.unwrap(), json!({}));
    for key in &keys {
        assert!(
            ctx.state.storage.get(key).await.unwrap().is_empty(),
            "{key}"
        );
    }
}

/// An upload eunha stored before it named files as Paperclip does keeps its
/// preview where it was put.
#[tokio::test]
async fn test_an_old_upload_keeps_its_preview() {
    let ctx = TestContext::new("media-legacy").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let id = eunha::snowflake::next_id();
    sqlx::query(
        "INSERT INTO media_attachments
           (id, account_id, type, file_file_name, file_content_type, thumbnail_file_name,
            processing, created_at, updated_at)
         VALUES ($1, $2, 0, 'original.png', 'image/png', 'small.png', 2, now(), now())",
    )
    .bind(id)
    .bind(alice)
    .execute(&ctx.db)
    .await
    .unwrap();
    let media: Value = ctx
        .api
        .get(&format!("/api/v1/media/{id}"), Some(&ctx.alice_token))
        .await
        .json()
        .await
        .unwrap();
    let partition = eunha::media::int_to_path(id);
    assert!(media["preview_url"].as_str().unwrap().ends_with(&format!(
        "/media_attachments/files/{partition}/small/small.png"
    )));
}
