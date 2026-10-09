//! `eunha emoji`, which is `tootctl emoji`: custom emoji packs imported from
//! and exported to gzipped tarballs, and purged.

use std::io::Read as _;

use eunha::tootctl::{emoji, Recorder};

use crate::helpers::TestContext;

fn encoded(format: image::ImageFormat) -> Vec<u8> {
    let mut data = Vec::new();
    image::DynamicImage::new_rgba8(2, 2)
        .write_to(&mut std::io::Cursor::new(&mut data), format)
        .unwrap();
    data
}

fn png() -> Vec<u8> {
    encoded(image::ImageFormat::Png)
}

fn gif() -> Vec<u8> {
    encoded(image::ImageFormat::Gif)
}

/// A pack as an emoji collection ships one.
fn pack(dir: &std::path::Path, files: &[(&str, Vec<u8>)]) -> std::path::PathBuf {
    let path = dir.join("pack.tar.gz");
    let file = std::fs::File::create(&path).unwrap();
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        file,
        flate2::Compression::default(),
    ));
    for (name, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, data.as_slice()).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
    path
}

fn scratch(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("eunha-emoji-{label}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn emojis(ctx: &TestContext) -> Vec<(String, bool, Option<String>)> {
    sqlx::query_as(
        "SELECT e.shortcode, e.visible_in_picker, c.name FROM custom_emojis e
         LEFT JOIN custom_emoji_categories c ON c.id = e.category_id
         WHERE e.domain IS NULL ORDER BY e.shortcode",
    )
    .fetch_all(&ctx.db)
    .await
    .unwrap()
}

/// What a pack holds becomes emoji, named with the prefix and suffix asked
/// for; what is there already is skipped unless overwritten; and what an
/// emoji may not be is reported and left out.
#[tokio::test]
async fn test_import_a_pack() {
    let ctx = TestContext::new("cli-emoji-import").await;
    let dir = scratch("import");
    let too_big = {
        let mut png = png();
        png.resize(eunha::custom_emoji::LIMIT + 1, 0);
        png
    };
    let path = pack(
        &dir,
        &[
            ("pack/blobcat.png", png()),
            ("pack/._blobcat.png", png()),
            ("pack/party.gif", gif()),
            ("pack/README.txt", b"hello".to_vec()),
            ("pack/huge.png", too_big),
        ],
    );
    let options = emoji::ImportOptions {
        prefix: Some("x_".into()),
        suffix: Some("_y".into()),
        unlisted: true,
        category: Some("Blobs".into()),
        ..Default::default()
    };

    let console = Recorder::default();
    emoji::import(&ctx.state, &console, &path, &options)
        .await
        .unwrap();
    assert_eq!(
        console.lines(),
        [
            "Failure/Error: ",
            "pack/huge.png",
            "  Image file size must be less than 256 KB, Image must be less than 256 KB",
            "Imported 2, skipped 0, failed to import 1",
        ]
    );
    assert_eq!(
        emojis(&ctx).await,
        [
            ("x_blobcat_y".to_owned(), false, Some("Blobs".to_owned())),
            ("x_party_y".to_owned(), false, Some("Blobs".to_owned())),
        ]
    );
    let file: (String, String) = sqlx::query_as(
        "SELECT image_file_name, image_content_type FROM custom_emojis WHERE shortcode = 'x_party_y'",
    )
    .fetch_one(&ctx.db)
    .await
    .unwrap();
    assert!(file.0.ends_with(".gif"));
    assert_eq!(file.1, "image/gif");

    let console = Recorder::default();
    emoji::import(&ctx.state, &console, &path, &options)
        .await
        .unwrap();
    assert_eq!(
        console.lines().last().unwrap(),
        "Imported 0, skipped 2, failed to import 1"
    );

    // Overwritten: a new image, listed, and in no category.
    let console = Recorder::default();
    emoji::import(
        &ctx.state,
        &console,
        &path,
        &emoji::ImportOptions {
            prefix: Some("X_".into()),
            suffix: Some("_Y".into()),
            overwrite: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        console.lines().last().unwrap(),
        "Imported 2, skipped 0, failed to import 1"
    );
    assert_eq!(
        emojis(&ctx).await,
        [
            ("x_blobcat_y".to_owned(), true, None),
            ("x_party_y".to_owned(), true, None),
        ]
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// The local emoji go out under their shortcodes, from storage; an archive
/// that is there already is kept unless overwritten.
#[tokio::test]
async fn test_export_and_purge() {
    let ctx = TestContext::new("cli-emoji-export").await;
    let dir = scratch("export");
    let path = pack(&dir, &[("blobcat.png", png()), ("party.gif", gif())]);
    let console = Recorder::default();
    emoji::import(&ctx.state, &console, &path, &Default::default())
        .await
        .unwrap();
    emoji::import(
        &ctx.state,
        &console,
        &pack(&dir, &[("cat.png", png())]),
        &emoji::ImportOptions {
            category: Some("Cats".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO custom_emojis (shortcode, domain, image_remote_url, created_at, updated_at)
         VALUES ('remote', 'remote.invalid', 'https://remote.invalid/e.png', now(), now()),
                ('blocked', 'sub.blocked.invalid', 'https://sub.blocked.invalid/e.png', now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();

    let out = scratch("export-out");
    let console = Recorder::default();
    emoji::export(&ctx.state, &console, &out, None, false)
        .await
        .unwrap();
    assert_eq!(
        console.lines(),
        [
            "Adding 'blobcat'...",
            "Adding 'party'...",
            "Adding 'cat'...",
            "Exported 3"
        ]
    );
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(
        std::fs::File::open(out.join("export.tar.gz")).unwrap(),
    ));
    let mut exported = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        exported.push((name, data));
    }
    assert_eq!(exported[0], ("blobcat.png".to_owned(), png()));
    assert_eq!(exported[1], ("party.gif".to_owned(), gif()));

    let error = emoji::export(&ctx.state, &console, &out, None, false)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Archive already exists! Use '--overwrite' to overwrite it!"
    );
    let console = Recorder::default();
    emoji::export(&ctx.state, &console, &out, Some("Cats"), true)
        .await
        .unwrap();
    assert_eq!(console.lines(), ["Adding 'cat'...", "Exported 1"]);
    let error = emoji::export(&ctx.state, &console, &out, Some("Dogs"), true)
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "Unable to find category 'Dogs'!");

    let shortcodes = async || -> Vec<String> {
        sqlx::query_scalar("SELECT shortcode FROM custom_emojis ORDER BY shortcode")
            .fetch_all(&ctx.db)
            .await
            .unwrap()
    };
    sqlx::query(
        "INSERT INTO domain_blocks (domain, severity, created_at, updated_at)
         VALUES ('blocked.invalid', 1, now(), now())",
    )
    .execute(&ctx.db)
    .await
    .unwrap();
    let console = Recorder::default();
    emoji::purge(&ctx.state, &console, false, true)
        .await
        .unwrap();
    assert_eq!(console.lines(), ["OK"]);
    assert_eq!(
        shortcodes().await,
        ["blobcat", "cat", "party", "remote"],
        "a suspended server's subdomain's emoji go"
    );
    emoji::purge(&ctx.state, &console, true, false)
        .await
        .unwrap();
    assert_eq!(shortcodes().await, ["blobcat", "cat", "party"]);
    emoji::purge(&ctx.state, &console, false, false)
        .await
        .unwrap();
    assert!(shortcodes().await.is_empty());
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(out);
}
