//! Puts the static assets the server needs into a new R2 bucket.
//!
//! Mastodon: none

/// Initializes an Eunha R2 bucket with the static assets the server requires.
///
/// Currently uploads:
///   avatars/original/missing.png  — placeholder shown when no avatar is set
///   headers/original/missing.png  — placeholder shown when no header is set
///
/// Usage (via config file):
///   eunha-init-r2 --config /etc/eunha/config.toml
///
/// Usage (individual flags):
///   eunha-init-r2 \
///     --bucket eunha-social \
///     --endpoint https://ACCOUNT_ID.r2.cloudflarestorage.com \
///     --access-key-id KEY \
///     --secret-access-key SECRET
use anyhow::{Context, Result};
use aws_sdk_s3::primitives::ByteStream;
use clap::Parser;

static AVATAR_MISSING: &[u8] = include_bytes!("../../assets/avatar_missing.png");
static HEADER_MISSING: &[u8] = include_bytes!("../../assets/header_missing.png");

#[derive(Parser, Debug)]
#[command(about = "Initialize the Eunha R2 bucket with required static assets")]
struct Args {
    /// Path to the server config TOML file (media_storage section is used).
    #[arg(long)]
    config: Option<String>,
    #[arg(long)]
    bucket: Option<String>,
    #[arg(long)]
    endpoint: Option<String>,
    #[arg(long)]
    access_key_id: Option<String>,
    #[arg(long)]
    secret_access_key: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let cfg = args
        .config
        .as_deref()
        .map(eunha::config::Config::from_file)
        .transpose()?;
    let ms = cfg.as_ref().map(|c| &c.media_storage);
    let key_prefix = ms.map(|m| m.key_prefix.as_str()).unwrap_or("");

    let bucket = args
        .bucket
        .or_else(|| ms.map(|m| m.bucket.clone()))
        .context("--bucket (or --config with media_storage.bucket)")?;
    let endpoint = args
        .endpoint
        .or_else(|| ms.and_then(|m| m.endpoint.clone()))
        .context("--endpoint (or --config with media_storage.endpoint)")?;
    let access_key_id = args
        .access_key_id
        .or_else(|| ms.map(|m| m.access_key_id.clone()))
        .context("--access-key-id (or --config with media_storage.access_key_id)")?;
    let secret_access_key = args
        .secret_access_key
        .or_else(|| ms.map(|m| m.secret_access_key.clone()))
        .context("--secret-access-key (or --config with media_storage.secret_access_key)")?;

    let creds = aws_sdk_s3::config::Credentials::new(
        &access_key_id,
        &secret_access_key,
        None,
        None,
        "static",
    );
    let s3_conf = aws_sdk_s3::config::Builder::new()
        .region(aws_sdk_s3::config::Region::new("auto"))
        .credentials_provider(creds)
        .endpoint_url(&endpoint)
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .build();
    let s3 = aws_sdk_s3::Client::from_conf(s3_conf);

    let assets: &[(&str, &[u8])] = &[
        ("avatars/original/missing.png", AVATAR_MISSING),
        ("headers/original/missing.png", HEADER_MISSING),
    ];

    for (key, data) in assets {
        let object_key = eunha::media::prefixed_key(key_prefix, key);
        tracing::info!("uploading {} ({} bytes) ...", object_key, data.len());
        s3.put_object()
            .bucket(&bucket)
            .key(&object_key)
            .body(ByteStream::from(data.to_vec()))
            .content_type("image/png")
            .cache_control("public, max-age=2419200, must-revalidate")
            .send()
            .await
            .with_context(|| format!("uploading {object_key}"))?;
        tracing::info!("uploaded {}", object_key);
    }

    tracing::info!("R2 bucket initialized");
    Ok(())
}
