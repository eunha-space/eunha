/// Uploads a Mastodon media tree into an instance's storage.
///
/// The work is `eunha::import::upload_media`, which `eunha import-media` also
/// runs; this exists for a media directory that has to go somewhere no config
/// file describes.
///
/// Usage (via config file):
///   eunha-upload-media \
///     --config /etc/eunha/config.toml \
///     --media-dir ~/seoulearth_dump/media
///
/// Usage (individual flags):
///   eunha-upload-media \
///     --media-dir ~/seoulearth_dump/media \
///     --bucket eunha-social \
///     --endpoint https://5d508a37b0c6ea183620094959bbc8d1.r2.cloudflarestorage.com \
///     --access-key-id KEY \
///     --secret-access-key SECRET
use anyhow::{Context, Result};
use clap::Parser;
use eunha::config::MediaStorageConfig;
use std::path::PathBuf;

#[derive(Parser, Debug)]
struct Args {
    /// Path to the server config TOML file (media_storage is used).
    #[arg(long)]
    config: Option<String>,
    #[arg(long)]
    media_dir: String,
    /// S3 bucket name (overrides config media_storage.bucket).
    #[arg(long)]
    bucket: Option<String>,
    /// Namespace for every object key (overrides config
    /// media_storage.key_prefix). A bucket shared by several instances needs
    /// one; a dedicated bucket does not.
    #[arg(long)]
    key_prefix: Option<String>,
    /// S3 endpoint URL (overrides config media_storage.endpoint).
    #[arg(long)]
    endpoint: Option<String>,
    /// S3 access key ID (overrides config media_storage.access_key_id).
    #[arg(long)]
    access_key_id: Option<String>,
    /// S3 secret access key (overrides config media_storage.secret_access_key).
    #[arg(long)]
    secret_access_key: Option<String>,
    /// Number of concurrent S3 uploads (default: 32).
    #[arg(long, default_value_t = 32)]
    concurrency: usize,
    /// Ask for each object before sending it, and send only what is missing.
    /// This is how an interrupted upload resumes cheaply; a first run pays a
    /// request per file for nothing.
    #[arg(long)]
    skip_existing: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let configured = args
        .config
        .as_deref()
        .map(eunha::config::Config::from_file)
        .transpose()?
        .map(|config| config.media_storage);
    let storage = MediaStorageConfig {
        bucket: args
            .bucket
            .or_else(|| configured.as_ref().map(|m| m.bucket.clone()))
            .context("--bucket or --config with media_storage.bucket")?,
        key_prefix: args
            .key_prefix
            .or_else(|| configured.as_ref().map(|m| m.key_prefix.clone()))
            .unwrap_or_default(),
        region: configured
            .as_ref()
            .map(|m| m.region.clone())
            .unwrap_or_else(|| "auto".to_string()),
        endpoint: Some(
            args.endpoint
                .or_else(|| configured.as_ref().and_then(|m| m.endpoint.clone()))
                .context("--endpoint or --config with media_storage.endpoint")?,
        ),
        access_key_id: args
            .access_key_id
            .or_else(|| configured.as_ref().map(|m| m.access_key_id.clone()))
            .context("--access-key-id or --config with media_storage.access_key_id")?,
        secret_access_key: args
            .secret_access_key
            .or_else(|| configured.as_ref().map(|m| m.secret_access_key.clone()))
            .context("--secret-access-key or --config with media_storage.secret_access_key")?,
        base_url: configured
            .as_ref()
            .map(|m| m.base_url.clone())
            .unwrap_or_default(),
    };

    let uploaded = eunha::import::upload_media(
        &storage,
        &PathBuf::from(&args.media_dir),
        args.concurrency,
        args.skip_existing,
    )
    .await?;

    // Progress goes to the log; this is the result, and it is on stdout so a
    // caller can read it without reading the log.
    println!("OK");
    println!("files: {}", uploaded.total);
    println!("uploaded: {}", uploaded.sent);
    println!("skipped: {}", uploaded.skipped);
    println!("key prefix: {}", uploaded.key_prefix);
    Ok(())
}
