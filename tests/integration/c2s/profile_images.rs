//! Avatars and headers as `Account::Avatar` and `Account::Header` take them.

use reqwest::StatusCode;
use serde_json::Value;

use crate::helpers::{sideways_jpeg, tiny_png, TestContext};

/// An animated GIF of `frames` frames, `width`×`height`.
pub fn animated_gif(width: u16, height: u16, frames: usize) -> Vec<u8> {
    use image::{codecs::gif::GifEncoder, Delay, Frame, Rgba, RgbaImage};
    let mut bytes = Vec::new();
    {
        let mut encoder = GifEncoder::new(&mut bytes);
        encoder
            .set_repeat(image::codecs::gif::Repeat::Infinite)
            .unwrap();
        for i in 0..frames {
            let shade = (i * 80 % 256) as u8;
            let image = RgbaImage::from_pixel(
                u32::from(width),
                u32::from(height),
                Rgba([shade, 0, 255 - shade, 255]),
            );
            encoder
                .encode_frame(Frame::from_parts(
                    image,
                    0,
                    0,
                    Delay::from_numer_denom_ms(100, 1),
                ))
                .unwrap();
        }
    }
    bytes
}

async fn update(
    ctx: &TestContext,
    files: &[(&'static str, &'static str, Vec<u8>)],
    texts: &[(&'static str, String)],
) -> reqwest::Response {
    let mut form = reqwest::multipart::Form::new();
    for (name, content_type, data) in files {
        form = form.part(
            *name,
            reqwest::multipart::Part::bytes(data.clone())
                .file_name(format!("{name}.bin"))
                .mime_str(content_type)
                .unwrap(),
        );
    }
    for (name, text) in texts {
        form = form.text(*name, text.clone());
    }
    ctx.api
        .http
        .patch(ctx.api.url("/api/v1/accounts/update_credentials"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap()
}

async fn stored(ctx: &TestContext) -> (Option<String>, Option<String>, Option<i32>) {
    let alice: i64 = ctx.alice_id.parse().unwrap();
    sqlx::query_as(
        "SELECT avatar_file_name, avatar_content_type, avatar_file_size FROM accounts WHERE id = $1",
    )
    .bind(alice)
    .fetch_one(&ctx.db)
    .await
    .unwrap()
}

/// Only JPEG, PNG, GIF and WebP are taken, and nothing is saved when one is
/// refused.
#[tokio::test]
async fn test_an_avatar_of_another_type_is_refused() {
    let ctx = TestContext::new("avatar-type").await;
    let resp = update(
        &ctx,
        &[("avatar", "image/svg+xml", b"<svg/>".to_vec())],
        &[("display_name", "Changed".into())],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Validation failed: Avatar content type is invalid, Avatar is invalid"
    );
    assert_eq!(stored(&ctx).await, (None, None, None));
}

/// Bytes that are no image are never stored under the type the client
/// claimed for them.
#[tokio::test]
async fn test_an_undecodable_avatar_is_refused() {
    let ctx = TestContext::new("avatar-undecodable").await;
    let resp = update(
        &ctx,
        &[(
            "avatar",
            "image/png",
            b"<html>not a picture</html>".to_vec(),
        )],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Validation failed: Avatar Paperclip::Errors::NotIdentifiedByImageMagickError"
    );
    assert_eq!(stored(&ctx).await, (None, None, None));
}

/// `AVATAR_LIMIT` and `HEADER_LIMIT`: under 8 MB.
#[tokio::test]
async fn test_a_header_of_eight_megabytes_is_refused() {
    let ctx = TestContext::new("header-size").await;
    let mut data = tiny_png();
    data.resize(8 * 1024 * 1024, 0);
    let resp = update(&ctx, &[("header", "image/png", data)], &[]).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Validation failed: Header file size must be less than 8 MB, Header must be less than 8 MB"
    );
}

/// A GIF avatar keeps moving, shrunk and cropped square as Paperclip's
/// `400x400#` does it, and its first frame is kept as the PNG that
/// `avatar_static` names.
#[tokio::test]
async fn test_a_gif_avatar_is_cropped_and_given_a_static_png() {
    let ctx = TestContext::new("avatar-gif").await;
    let resp = update(
        &ctx,
        &[("avatar", "image/gif", animated_gif(800, 400, 3))],
        &[],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let account: Value = resp.json().await.unwrap();
    let (name, content_type, size) = stored(&ctx).await;
    let name = name.unwrap();
    assert_eq!(content_type.as_deref(), Some("image/gif"));
    assert!(name.ends_with(".gif"));

    let avatar = account["avatar"].as_str().unwrap();
    let avatar_static = account["avatar_static"].as_str().unwrap();
    assert!(avatar.ends_with(&format!("/original/{name}")), "{avatar}");
    let stem = name.trim_end_matches(".gif");
    assert!(
        avatar_static.ends_with(&format!("/static/{stem}.png")),
        "{avatar_static}"
    );

    let object = |url: &str| url[url.find("accounts/").unwrap()..].to_owned();
    let gif = ctx.state.storage.get(&object(avatar)).await.unwrap();
    assert_eq!(gif.len() as i32, size.unwrap());
    let decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(gif)).unwrap();
    use image::{AnimationDecoder, ImageDecoder};
    assert_eq!(decoder.dimensions(), (200, 200));
    assert_eq!(decoder.into_frames().count(), 3);

    let png = ctx.state.storage.get(&object(avatar_static)).await.unwrap();
    let png = image::load_from_memory_with_format(&png, image::ImageFormat::Png).unwrap();
    assert_eq!((png.width(), png.height()), (400, 400));
}

/// Any other avatar is its own static one.
#[tokio::test]
async fn test_a_png_avatar_is_its_own_static_one() {
    let ctx = TestContext::new("avatar-png").await;
    let resp = update(&ctx, &[("avatar", "image/jpeg", sideways_jpeg())], &[]).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let account: Value = resp.json().await.unwrap();
    assert_eq!(account["avatar"], account["avatar_static"]);
    assert!(account["avatar"].as_str().unwrap().contains("/original/"));
}

/// `avatar_description` and `header_description` are saved, up to 150
/// characters each.
#[tokio::test]
async fn test_image_descriptions_are_saved_up_to_150_characters() {
    let ctx = TestContext::new("avatar-description").await;
    let resp = update(
        &ctx,
        &[],
        &[
            ("avatar_description", "A cat".into()),
            ("header_description", "A field".into()),
        ],
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let account: Value = resp.json().await.unwrap();
    assert_eq!(account["avatar_description"], "A cat");
    assert_eq!(account["header_description"], "A field");

    let resp = update(&ctx, &[], &[("avatar_description", "x".repeat(151))]).await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["error"],
        "Validation failed: Avatar description is too long (maximum is 150 characters)"
    );
    let resp = ctx
        .api
        .get(
            "/api/v1/accounts/verify_credentials",
            Some(&ctx.alice_token),
        )
        .await;
    let account: Value = resp.json().await.unwrap();
    assert_eq!(account["avatar_description"], "A cat");
}
