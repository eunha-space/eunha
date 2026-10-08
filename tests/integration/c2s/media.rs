use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::{sideways_jpeg, tiny_png, TestContext};

/// POST /api/v1/media uploads an image and returns a media attachment.
#[tokio::test]
async fn test_media_upload_image() {
    let ctx = TestContext::new("media-upload").await;

    let resp = ctx
        .api
        .post_multipart_file(
            "/api/v1/media",
            &ctx.alice_token,
            "test.png",
            "image/png",
            tiny_png(),
            &[],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK, "upload should succeed");
    let media: Value = resp.json().await.unwrap();
    assert!(media["id"].as_str().is_some(), "id missing");
    assert_eq!(media["type"].as_str(), Some("image"));
    assert!(media["url"].as_str().is_some(), "url missing");
}

/// A photo is stored upright, as Mastodon's libvips thumbnailing leaves it:
/// its EXIF orientation is applied to the pixels, so the dimensions a client
/// lays the attachment out by are the portrait ones.
#[tokio::test]
async fn test_media_upload_applies_exif_orientation() {
    let ctx = TestContext::new("media-orientation").await;

    let resp = ctx
        .api
        .post_multipart_file(
            "/api/v1/media",
            &ctx.alice_token,
            "portrait.jpg",
            "image/jpeg",
            sideways_jpeg(),
            &[],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let media: Value = resp.json().await.unwrap();
    assert_eq!(media["meta"]["original"]["width"], 32);
    assert_eq!(media["meta"]["original"]["height"], 64);
    assert_eq!(media["meta"]["small"]["width"], 32);
    assert_eq!(media["meta"]["small"]["height"], 64);
    assert!(media["blurhash"].as_str().is_some(), "blurhash missing");
}

/// POST /api/v2/media also works and returns the same shape.
#[tokio::test]
async fn test_media_upload_v2() {
    let ctx = TestContext::new("media-upload-v2").await;

    let resp = ctx
        .api
        .post_multipart_file(
            "/api/v2/media",
            &ctx.alice_token,
            "test.png",
            "image/png",
            tiny_png(),
            &[],
        )
        .await;
    assert!(
        resp.status() == StatusCode::OK || resp.status() == StatusCode::ACCEPTED,
        "v2 upload should return 200 or 202, got {}",
        resp.status()
    );
    let media: Value = resp.json().await.unwrap();
    assert!(media["id"].as_str().is_some(), "id missing");
    assert_eq!(media["type"].as_str(), Some("image"));
}

/// The start of an ISO base media file with major brand `brand`, which is all
/// libvips needs to pick `heifload` for it.
fn ftyp(brand: &[u8; 4]) -> Vec<u8> {
    let mut data = vec![0, 0, 0, 24];
    data.extend_from_slice(b"ftyp");
    data.extend_from_slice(brand);
    data.extend_from_slice(&[0, 0, 0, 0]);
    data.extend_from_slice(b"mif1");
    data.extend_from_slice(brand);
    data.extend_from_slice(&[0; 64]);
    data
}

/// Mastodon 4.7.2 blocks libvips' HEIF loader, so a HEIC, HEIF or AVIF image
/// fails processing whatever type it was sent as, and the upload is answered
/// with `processing_error`'s 500 on both versions of the endpoint. The types
/// stay advertised, as upstream still lists them.
#[tokio::test]
async fn test_media_upload_refuses_heif_as_mastodon_4_7_2_does() {
    let ctx = TestContext::new("media-heif").await;
    for (path, name, content_type, data) in [
        ("/api/v1/media", "photo.heic", "image/heic", ftyp(b"heic")),
        ("/api/v2/media", "photo.heif", "image/heif", ftyp(b"mif1")),
        ("/api/v2/media", "photo.avif", "image/avif", ftyp(b"avif")),
        ("/api/v1/media", "photo.jpg", "image/jpeg", ftyp(b"avif")),
    ] {
        let resp = ctx
            .api
            .post_multipart_file(path, &ctx.alice_token, name, content_type, data, &[])
            .await;
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "{path} {content_type}"
        );
        let body: Value = resp.json().await.unwrap();
        assert_eq!(
            body["error"],
            "Error processing thumbnail for uploaded media"
        );
    }
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM media_attachments")
        .fetch_one(&ctx.db)
        .await
        .unwrap();
    assert_eq!(stored, 0);

    let instance: Value = ctx
        .api
        .get("/api/v2/instance", None)
        .await
        .json()
        .await
        .unwrap();
    let types = instance["configuration"]["media_attachments"]["supported_mime_types"]
        .as_array()
        .unwrap();
    for advertised in ["image/heic", "image/heif", "image/avif"] {
        assert!(types.iter().any(|t| t == advertised), "{advertised}");
    }
}

/// POST /api/v1/media with a description stores it.
#[tokio::test]
async fn test_media_upload_with_description() {
    let ctx = TestContext::new("media-desc").await;

    let resp = ctx
        .api
        .post_multipart_file(
            "/api/v1/media",
            &ctx.alice_token,
            "test.png",
            "image/png",
            tiny_png(),
            &[("description", "a tiny image")],
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let media: Value = resp.json().await.unwrap();
    assert_eq!(media["description"].as_str(), Some("a tiny image"));
}

/// POST /api/v1/media without a file returns 422.
#[tokio::test]
async fn test_media_upload_missing_file() {
    let ctx = TestContext::new("media-no-file").await;

    // Send an empty multipart (no file part).
    let form = reqwest::multipart::Form::new().text("description", "no file here");
    let resp = ctx
        .api
        .http
        .post(ctx.api.url("/api/v1/media"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "missing file should be 422"
    );
}

/// GET /api/v1/media/:id returns the media attachment.
#[tokio::test]
async fn test_media_get() {
    let ctx = TestContext::new("media-get").await;

    let upload: Value = ctx
        .api
        .post_multipart_file(
            "/api/v1/media",
            &ctx.alice_token,
            "test.png",
            "image/png",
            tiny_png(),
            &[],
        )
        .await
        .json()
        .await
        .unwrap();
    let id = upload["id"].as_str().unwrap();

    let resp = ctx
        .api
        .get(&format!("/api/v1/media/{}", id), Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let got: Value = resp.json().await.unwrap();
    assert_eq!(got["id"].as_str(), Some(id));
    assert_eq!(got["type"].as_str(), Some("image"));
}

/// GET /api/v1/media/:id for unknown id returns 404.
#[tokio::test]
async fn test_media_get_not_found() {
    let ctx = TestContext::new("media-get-404").await;

    let resp = ctx
        .api
        .get("/api/v1/media/999999999999", Some(&ctx.alice_token))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// A media description over 10000 characters is rejected (Mastodon
/// MediaAttachment::MAX_DESCRIPTION_LENGTH).
#[tokio::test]
async fn test_media_description_too_long() {
    let ctx = TestContext::new("media-desc-long").await;

    let long = "x".repeat(10_001);
    let resp = ctx
        .api
        .post_multipart_file(
            "/api/v1/media",
            &ctx.alice_token,
            "t.png",
            "image/png",
            tiny_png(),
            &[("description", long.as_str())],
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "over-long description must be rejected"
    );
}

/// A status may attach at most 4 media (Mastodon `MEDIA_ATTACHMENTS_LIMIT`);
/// a fifth attachment is rejected while exactly four is accepted.
#[tokio::test]
async fn test_status_rejects_more_than_four_media() {
    let ctx = TestContext::new("media-limit").await;

    let mut ids = Vec::new();
    for _ in 0..5 {
        let media: Value = ctx
            .api
            .post_multipart_file(
                "/api/v1/media",
                &ctx.alice_token,
                "t.png",
                "image/png",
                tiny_png(),
                &[],
            )
            .await
            .json()
            .await
            .unwrap();
        ids.push(media["id"].as_str().unwrap().to_string());
    }

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &serde_json::json!({ "status": "five", "media_ids": ids }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "5 media must be rejected"
    );

    let resp = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &serde_json::json!({ "status": "four", "media_ids": ids[..4] }),
        )
        .await;
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "exactly 4 media should be accepted"
    );
}

/// PUT /api/v1/media/:id updates the description.
#[tokio::test]
async fn test_media_update_description() {
    let ctx = TestContext::new("media-update").await;

    let upload: Value = ctx
        .api
        .post_multipart_file(
            "/api/v1/media",
            &ctx.alice_token,
            "test.png",
            "image/png",
            tiny_png(),
            &[("description", "original")],
        )
        .await
        .json()
        .await
        .unwrap();
    let id = upload["id"].as_str().unwrap();

    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/media/{}", id),
            Some(&ctx.alice_token),
            &serde_json::json!({ "description": "updated description" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let updated: Value = resp.json().await.unwrap();
    assert_eq!(updated["description"].as_str(), Some("updated description"));
}

/// PUT /api/v1/media/:id owned by another user returns 404.
#[tokio::test]
async fn test_media_update_not_owner() {
    let ctx = TestContext::new("media-update-owner").await;

    let upload: Value = ctx
        .api
        .post_multipart_file(
            "/api/v1/media",
            &ctx.alice_token,
            "test.png",
            "image/png",
            tiny_png(),
            &[],
        )
        .await
        .json()
        .await
        .unwrap();
    let id = upload["id"].as_str().unwrap();

    let resp = ctx
        .api
        .put_json(
            &format!("/api/v1/media/{}", id),
            Some(&ctx.bob_token),
            &serde_json::json!({ "description": "should fail" }),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// Uploading media and attaching it to a status works end-to-end.
#[tokio::test]
async fn test_media_attach_to_status() {
    let ctx = TestContext::new("media-attach").await;

    let upload: Value = ctx
        .api
        .post_multipart_file(
            "/api/v1/media",
            &ctx.alice_token,
            "test.png",
            "image/png",
            tiny_png(),
            &[("description", "attached image")],
        )
        .await
        .json()
        .await
        .unwrap();
    let media_id = upload["id"].as_str().unwrap();

    let status: Value = ctx
        .api
        .post_json(
            "/api/v1/statuses",
            Some(&ctx.alice_token),
            &serde_json::json!({
                "status": "look at this image",
                "media_ids": [media_id]
            }),
        )
        .await
        .json()
        .await
        .unwrap();

    let attachments = status["media_attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1, "status should have one attachment");
    assert_eq!(attachments[0]["id"].as_str(), Some(media_id));
    assert_eq!(
        attachments[0]["description"].as_str(),
        Some("attached image")
    );
}

/// Migration 035 gives the rows eunha wrote with its old type numbers
/// Mastodon's, telling them apart by content type, and leaves Mastodon's own.
#[tokio::test]
async fn test_migration_033_swaps_only_the_media_types_eunha_wrote() {
    let ctx = TestContext::new("media-type-migration").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let remote = "https://remote.invalid/media/a";
    let original = r#"{"original": {"duration": 3.0, "bitrate": 128000}}"#;
    let focus = r#"{"focus": {"x": 0.5, "y": 0.5}}"#;
    // (type before, content type, waiting on an audio job, remote URL,
    // file_meta, type after)
    type Row<'a> = (i32, Option<&'a str>, bool, &'a str, Option<&'a str>, i32);
    let rows: [Row; 11] = [
        // eunha's audio, read by Mastodon as unknown
        (3, Some("audio/mpeg"), false, "", None, 4),
        // eunha's audio upload still waiting to be transcoded
        (3, None, true, "", None, 4),
        // eunha's unknown remote attachment, read by Mastodon as audio
        (4, Some("application/pdf"), false, remote, None, 3),
        // eunha's record of one from a `reject_media` domain
        (4, None, false, remote, Some(focus), 3),
        (4, None, false, remote, None, 3),
        // Mastodon's unknown: a remote attachment it never downloaded
        (3, None, false, remote, Some(focus), 3),
        // Mastodon's audio, transcoded to MP3
        (4, Some("audio/mpeg"), false, "", Some(original), 4),
        // Mastodon's remote audio whose cached copy was removed
        (4, None, false, remote, Some(original), 4),
        // Mastodon's audio upload waiting to be processed
        (4, Some("video/x-ms-asf"), false, "", None, 4),
        (0, Some("image/png"), false, "", None, 0),
        (2, Some("video/mp4"), false, "", None, 2),
    ];
    let mut ids = vec![];
    for (kind, content_type, job, remote_url, meta, _) in rows {
        let id = eunha::snowflake::next_id();
        sqlx::query(
            "INSERT INTO media_attachments
               (id, account_id, type, file_content_type, remote_url, file_meta, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6::json, now(), now())",
        )
        .bind(id)
        .bind(alice)
        .bind(kind)
        .bind(content_type)
        .bind(remote_url)
        .bind(meta)
        .execute(&ctx.db)
        .await
        .unwrap();
        if job {
            sqlx::query(
                "INSERT INTO eunha.media_processing_jobs (media_id, media_type, source_key, content_type)
                 VALUES ($1, 'audio', 'source', 'audio/ogg')",
            )
            .bind(id)
            .execute(&ctx.db)
            .await
            .unwrap();
        }
        ids.push(id);
    }

    sqlx::raw_sql(include_str!(
        "../../../migrations/035_media_attachment_audio_type.sql"
    ))
    .execute(&ctx.db)
    .await
    .unwrap();

    for ((before, content_type, _, _, meta, after), id) in rows.iter().zip(ids) {
        let kind: i32 = sqlx::query_scalar("SELECT type FROM media_attachments WHERE id = $1")
            .bind(id)
            .fetch_one(&ctx.db)
            .await
            .unwrap();
        assert_eq!(
            kind, *after,
            "type {before} with content type {content_type:?} and meta {meta:?}"
        );
    }
}

/// Audio, type 4, is not attached beside other media (`audio_or_video?`);
/// an unknown attachment, type 3, is.
#[tokio::test]
async fn test_audio_is_attached_alone() {
    let ctx = TestContext::new("media-audio-alone").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    let mut ids = vec![];
    for kind in [0, 4, 3] {
        let id = eunha::snowflake::next_id();
        sqlx::query(
            "INSERT INTO media_attachments (id, account_id, type, file_file_name, processing, created_at, updated_at)
             VALUES ($1, $2, $3, 'a.bin', 2, now(), now())",
        )
        .bind(id)
        .bind(alice)
        .bind(kind)
        .execute(&ctx.db)
        .await
        .unwrap();
        ids.push(id.to_string());
    }
    for (media, status) in [
        ([&ids[0], &ids[1]], StatusCode::UNPROCESSABLE_ENTITY),
        ([&ids[0], &ids[2]], StatusCode::OK),
    ] {
        let resp = ctx
            .api
            .post_json(
                "/api/v1/statuses",
                Some(&ctx.alice_token),
                &serde_json::json!({ "status": "media", "media_ids": media }),
            )
            .await;
        assert_eq!(resp.status(), status);
    }
}

/// `type` is read as Mastodon's enum: 3 is `unknown` and 4 is `audio`.
#[tokio::test]
async fn test_media_types_are_mastodons() {
    let ctx = TestContext::new("media-type-enum").await;
    let alice: i64 = ctx.alice_id.parse().unwrap();
    for (kind, name) in [
        (0, "image"),
        (1, "gifv"),
        (2, "video"),
        (3, "unknown"),
        (4, "audio"),
    ] {
        let id = eunha::snowflake::next_id();
        sqlx::query(
            "INSERT INTO media_attachments (id, account_id, type, file_file_name, file_content_type, processing, created_at, updated_at)
             VALUES ($1, $2, $3, 'a.bin', 'application/octet-stream', 2, now(), now())",
        )
        .bind(id)
        .bind(alice)
        .bind(kind)
        .execute(&ctx.db)
        .await
        .unwrap();
        let got: Value = ctx
            .api
            .get(&format!("/api/v1/media/{id}"), Some(&ctx.alice_token))
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(got["type"], name, "type {kind}");
    }
}
