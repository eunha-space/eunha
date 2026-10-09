//! Local custom emoji, stored and shown as Mastodon's `CustomEmoji` stores
//! and shows them: Paperclip's columns, its `custom_emojis/images/...`
//! layout, and a `static` PNG beside the original.

use std::io::Cursor;

use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::helpers::TestContext;

async fn make_admin(ctx: &TestContext) {
    crate::helpers::make_admin(&ctx.db, ctx.alice_id.parse().unwrap()).await;
}

fn gif() -> Vec<u8> {
    let mut gif = Vec::new();
    image::DynamicImage::new_rgba8(8, 8)
        .write_to(&mut Cursor::new(&mut gif), image::ImageFormat::Gif)
        .unwrap();
    gif
}

fn png() -> Vec<u8> {
    let mut png = Vec::new();
    image::DynamicImage::new_rgba8(8, 8)
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    png
}

async fn upload(
    ctx: &TestContext,
    shortcode: &str,
    content_type: &str,
    data: Vec<u8>,
) -> reqwest::Response {
    let part = reqwest::multipart::Part::bytes(data)
        .file_name("upload")
        .mime_str(content_type)
        .unwrap();
    let form = reqwest::multipart::Form::new()
        .text("shortcode", shortcode.to_owned())
        .part("image", part);
    ctx.api
        .http
        .post(ctx.api.url("/api/v1/admin/custom_emojis"))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .multipart(form)
        .send()
        .await
        .unwrap()
}

/// What a local emoji's image is stored under, relative to the bucket.
fn key_of(url: &str, ctx: &TestContext) -> String {
    let base = ctx.state.storage.public_url("");
    url.strip_prefix(&base).unwrap().to_owned()
}

#[tokio::test]
async fn an_upload_is_stored_where_paperclip_stores_it() {
    let ctx = TestContext::new("emoji-upload").await;
    make_admin(&ctx).await;

    let resp = upload(&ctx, "blobcat", "image/gif", gif()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let created: Value = resp.json().await.unwrap();
    let id: i64 = created["id"].as_str().unwrap().parse().unwrap();

    let row = sqlx::query!(
        "SELECT image_file_name, image_content_type, image_file_size, image_updated_at,
                image_storage_schema_version, image_remote_url
         FROM custom_emojis WHERE id = $1",
        id
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let file_name = row.image_file_name.unwrap();
    assert_eq!(file_name.len(), "0123456789abcdef.gif".len(), "{file_name}");
    assert!(file_name.ends_with(".gif"));
    assert_eq!(row.image_content_type.as_deref(), Some("image/gif"));
    assert_eq!(row.image_file_size, Some(gif().len() as i32));
    assert!(row.image_updated_at.is_some());
    assert_eq!(row.image_storage_schema_version, Some(1));
    assert_eq!(row.image_remote_url, None);

    let partition = eunha::media::int_to_path(id);
    let stem = file_name.trim_end_matches(".gif");
    let original = format!("custom_emojis/images/{partition}/original/{file_name}");
    let static_ = format!("custom_emojis/images/{partition}/static/{stem}.png");
    assert_eq!(created["url"], ctx.state.storage.public_url(&original));
    assert_eq!(
        created["static_url"],
        ctx.state.storage.public_url(&static_)
    );
    assert_eq!(ctx.state.storage.get(&original).await.unwrap(), gif());
    let still = ctx.state.storage.get(&static_).await.unwrap();
    assert_eq!(
        image::guess_format(&still).unwrap(),
        image::ImageFormat::Png
    );

    // The picker lists it, with the static style a PNG.
    let listed: Vec<Value> = ctx
        .api
        .get("/api/v1/custom_emojis", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        listed,
        vec![json!({
            "shortcode": "blobcat",
            "url": ctx.state.storage.public_url(&original),
            "static_url": ctx.state.storage.public_url(&static_),
            "visible_in_picker": true,
        })]
    );

    // Destroyed, its files go too.
    let resp = ctx
        .api
        .delete(
            &format!("/api/v1/admin/custom_emojis/{id}"),
            &ctx.alice_token,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(ctx
        .state
        .storage
        .get(&original)
        .await
        .map_or(true, |b| b.is_empty()));
    let logged: Vec<String> = sqlx::query_scalar!(
        "SELECT action FROM admin_action_logs WHERE target_type = 'CustomEmoji' ORDER BY id"
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap();
    assert_eq!(logged, ["create", "destroy"]);
}

#[tokio::test]
async fn uploads_are_validated_as_custom_emoji_validates_them() {
    let ctx = TestContext::new("emoji-validate").await;
    make_admin(&ctx).await;

    let error = |resp: reqwest::Response| async move {
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        resp.json::<Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    assert_eq!(
        error(upload(&ctx, "jpeg", "image/jpeg", b"not an image".to_vec()).await).await,
        "Validation failed: Image content type is invalid, Image is invalid"
    );
    let mut big = png();
    big.resize(256 * 1024, 0);
    assert_eq!(
        error(upload(&ctx, "big", "image/png", big).await).await,
        "Validation failed: Image file size must be less than 256 KB, \
         Image must be less than 256 KB"
    );
    assert_eq!(
        error(upload(&ctx, "a-b", "image/png", png()).await).await,
        "Validation failed: Shortcode is invalid"
    );
    assert_eq!(
        error(upload(&ctx, &"a".repeat(129), "image/png", png()).await).await,
        "Validation failed: Shortcode is too long (maximum is 128 characters)"
    );

    assert_eq!(
        upload(&ctx, "taken", "image/png", png()).await.status(),
        StatusCode::OK
    );
    assert_eq!(
        error(upload(&ctx, "taken", "image/png", png()).await).await,
        "Validation failed: Shortcode has already been taken"
    );
}

/// An emoji as Mastodon writes it — the file columns, no remote URL —
/// shown in the picker, in a post, and to other servers.
#[tokio::test]
async fn a_mastodon_emoji_is_shown_from_its_files() {
    let ctx = TestContext::new("emoji-mastodon").await;
    let category = sqlx::query_scalar!(
        "INSERT INTO custom_emoji_categories (name, created_at, updated_at)
         VALUES ('Cats', now(), now()) RETURNING id"
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    let id = sqlx::query_scalar!(
        "INSERT INTO custom_emojis
           (shortcode, image_file_name, image_content_type, image_file_size,
            image_updated_at, category_id, created_at, updated_at)
         VALUES ('blobcat', 'f00dfeedf00dfeed.png', 'image/png', 100, now(), $1, now(), now())
         RETURNING id",
        category,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE custom_emoji_categories SET featured_emoji_id = $1 WHERE id = $2",
        id,
        category
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    // Hidden from the picker: not `listed`.
    sqlx::query!(
        "INSERT INTO custom_emojis
           (shortcode, image_file_name, image_content_type, visible_in_picker,
            created_at, updated_at)
         VALUES ('hidden', 'a.png', 'image/png', false, now(), now())"
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let partition = eunha::media::int_to_path(id);
    let url = ctx.state.storage.public_url(&format!(
        "custom_emojis/images/{partition}/original/f00dfeedf00dfeed.png"
    ));
    let static_url = ctx.state.storage.public_url(&format!(
        "custom_emojis/images/{partition}/static/f00dfeedf00dfeed.png"
    ));

    let listed: Vec<Value> = ctx
        .api
        .get("/api/v1/custom_emojis", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        listed,
        vec![json!({
            "shortcode": "blobcat",
            "url": url,
            "static_url": static_url,
            "visible_in_picker": true,
            "category": "Cats",
            "featured": true,
        })]
    );

    let status = ctx
        .api
        .post_status(&ctx.alice_token, "Hi :blobcat: :hidden:", "public")
        .await;
    let emojis = status["emojis"].as_array().unwrap();
    assert_eq!(emojis.len(), 2);
    assert_eq!(
        emojis[0],
        json!({
            "shortcode": "blobcat",
            "url": url,
            "static_url": static_url,
            "visible_in_picker": true,
        })
    );

    let note: Value = ctx
        .api
        .ap_get(
            &format!("/users/alice/statuses/{}", status["id"].as_str().unwrap()),
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    let tag = note["tag"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == ":blobcat:")
        .cloned()
        .unwrap();
    let emoji_uri = format!("https://{}/emojis/{id}", ctx.domain);
    assert_eq!(tag["id"], emoji_uri);
    assert_eq!(
        tag["icon"],
        json!({"type": "Image", "mediaType": "image/png", "url": url})
    );

    // The emoji's own document, `EmojisController#show`.
    let document: Value = ctx
        .api
        .ap_get(&format!("/emojis/{id}"), None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(document["id"], emoji_uri);
    assert_eq!(document["type"], "Emoji");
    assert_eq!(document["name"], ":blobcat:");
    assert_eq!(document["icon"]["url"], url);
    assert_eq!(document["@context"][1]["Emoji"], "toot:Emoji", "{document}");
}

/// What eunha uploaded before it stored emoji as Mastodon does is moved by
/// `eunha migrate` into Paperclip's layout.
#[tokio::test]
async fn emoji_eunha_uploaded_before_are_moved() {
    let ctx = TestContext::new("emoji-move").await;
    ctx.state
        .storage
        .store(&gif(), "emoji/party.gif", "image/gif")
        .await
        .unwrap();
    let old_url = ctx.state.storage.public_url("emoji/party.gif");
    let id = sqlx::query_scalar!(
        "INSERT INTO custom_emojis (shortcode, image_remote_url, created_at, updated_at)
         VALUES ('party', $1, now(), now()) RETURNING id",
        old_url,
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    // One whose image is gone stays as it was.
    sqlx::query!(
        "INSERT INTO custom_emojis (shortcode, image_remote_url, created_at, updated_at)
         VALUES ('gone', $1, now(), now())",
        ctx.state.storage.public_url("emoji/gone.png"),
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    assert_eq!(
        eunha::custom_emoji::eunha_uploads_waiting(&ctx.db)
            .await
            .unwrap(),
        2
    );
    let report = eunha::custom_emoji::move_eunha_uploads(&ctx.db, &ctx.state.storage)
        .await
        .unwrap();
    assert_eq!(report.moved, 1);
    assert_eq!(report.missing.len(), 1);

    let row = sqlx::query!(
        "SELECT image_file_name, image_content_type, image_file_size, image_remote_url
         FROM custom_emojis WHERE id = $1",
        id
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert_eq!(row.image_remote_url, None);
    assert_eq!(row.image_content_type.as_deref(), Some("image/gif"));
    assert_eq!(row.image_file_size, Some(gif().len() as i32));
    let listed: Vec<Value> = ctx
        .api
        .get("/api/v1/custom_emojis", None)
        .await
        .json()
        .await
        .unwrap();
    let party = listed.iter().find(|e| e["shortcode"] == "party").unwrap();
    let original = key_of(party["url"].as_str().unwrap(), &ctx);
    assert_eq!(
        original,
        format!(
            "custom_emojis/images/{}/original/{}",
            eunha::media::int_to_path(id),
            row.image_file_name.unwrap()
        )
    );
    assert_eq!(ctx.state.storage.get(&original).await.unwrap(), gif());
    let still = ctx
        .state
        .storage
        .get(&key_of(party["static_url"].as_str().unwrap(), &ctx))
        .await
        .unwrap();
    assert_eq!(
        image::guess_format(&still).unwrap(),
        image::ImageFormat::Png
    );

    // Running it again moves nothing more.
    let again = eunha::custom_emoji::move_eunha_uploads(&ctx.db, &ctx.state.storage)
        .await
        .unwrap();
    assert_eq!(again.moved, 0);
}

/// An emoji is updated from a form as from JSON, its booleans cast as Rails
/// casts them, and from the query string, which Rails merges in.
#[tokio::test]
async fn an_emoji_is_updated_from_a_form() {
    let ctx = TestContext::new("emoji-patch-form").await;
    make_admin(&ctx).await;
    let created: Value = upload(&ctx, "blobcat", "image/gif", gif())
        .await
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();

    let resp = ctx
        .api
        .http
        .patch(ctx.api.url(&format!(
            "/api/v1/admin/custom_emojis/{id}?shortcode=blobfox"
        )))
        .header("host", &ctx.api.host)
        .bearer_auth(&ctx.alice_token)
        .form(&[("visible_in_picker", "0"), ("disabled", "1")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let emoji: Value = resp.json().await.unwrap();
    assert_eq!(emoji["shortcode"], json!("blobfox"));
    assert_eq!(emoji["visible_in_picker"], json!(false));
    assert_eq!(emoji["disabled"], json!(true));
}
