/// Uploads Mastodon backup media to R2.
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
use aws_sdk_s3::primitives::ByteStream;
use clap::Parser;
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    ///
    /// A re-run of an upload that was interrupted costs one HEAD per file it
    /// already moved instead of the file again, which on an instance's whole
    /// media library is the difference between minutes and hours. A first run
    /// pays a request per file for nothing, so it is not the default.
    #[arg(long)]
    skip_existing: bool,
}

/// What the upload moved, for whoever is driving this.
struct Uploaded {
    sent: usize,
    skipped: usize,
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
    let key_prefix = ms.map(|m| m.key_prefix.clone()).unwrap_or_default();

    let bucket_val = args
        .bucket
        .or_else(|| ms.map(|m| m.bucket.clone()))
        .context("--bucket or --config with media_storage.bucket")?;
    let endpoint_val = args
        .endpoint
        .or_else(|| ms.and_then(|m| m.endpoint.clone()))
        .context("--endpoint or --config with media_storage.endpoint")?;
    let access_key_id_val = args
        .access_key_id
        .or_else(|| ms.map(|m| m.access_key_id.clone()))
        .context("--access-key-id or --config with media_storage.access_key_id")?;
    let secret_access_key_val = args
        .secret_access_key
        .or_else(|| ms.map(|m| m.secret_access_key.clone()))
        .context("--secret-access-key or --config with media_storage.secret_access_key")?;

    let creds = aws_sdk_s3::config::Credentials::new(
        &access_key_id_val,
        &secret_access_key_val,
        None,
        None,
        "static",
    );
    let s3_conf = aws_sdk_s3::config::Builder::new()
        .region(aws_sdk_s3::config::Region::new("auto".to_string()))
        .credentials_provider(creds)
        .endpoint_url(&endpoint_val)
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .build();
    let client = aws_sdk_s3::Client::from_conf(s3_conf);
    let media_dir = PathBuf::from(&args.media_dir);

    tracing::info!(
        "uploading files from {} (concurrency={})...",
        media_dir.display(),
        args.concurrency
    );
    let files = collect_files(&media_dir)?;
    let total = files.len();
    tracing::info!("{} files to upload", total);
    let client = Arc::new(client);
    let bucket = Arc::new(bucket_val);
    let media_dir_arc = Arc::new(media_dir);
    let uploaded = upload_parallel(
        client,
        bucket,
        Arc::new(key_prefix),
        media_dir_arc,
        files,
        args.concurrency,
        args.skip_existing,
    )
    .await?;

    // Progress goes to the log; this is the result, and it is on stdout so a
    // caller can read it without reading the log.
    println!("OK");
    println!("files: {total}");
    println!("uploaded: {}", uploaded.sent);
    println!("skipped: {}", uploaded.skipped);
    Ok(())
}

fn collect_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_files_inner(dir, &mut files)?;
    Ok(files)
}

fn collect_files_inner(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files_inner(&path, out)?;
        } else if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| !n.starts_with('.'))
            .unwrap_or(false)
        {
            out.push(path);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn upload_parallel(
    client: Arc<aws_sdk_s3::Client>,
    bucket: Arc<String>,
    key_prefix: Arc<String>,
    root: Arc<PathBuf>,
    files: Vec<PathBuf>,
    concurrency: usize,
    skip_existing: bool,
) -> Result<Uploaded> {
    let total = files.len();
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let skipped = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    futures::stream::iter(files)
        .map(|path| {
            let client = client.clone();
            let bucket = bucket.clone();
            let key_prefix = key_prefix.clone();
            let root = root.clone();
            let counter = counter.clone();
            let skipped = skipped.clone();
            async move {
                let rel = path.strip_prefix(root.as_ref()).unwrap();
                let logical_key = rel.to_string_lossy().replace('\\', "/");
                let key = eunha::media::prefixed_key(&key_prefix, &logical_key);
                if skip_existing
                    && client
                        .head_object()
                        .bucket(bucket.as_ref())
                        .key(&key)
                        .send()
                        .await
                        .is_ok()
                {
                    skipped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Ok(());
                }
                let data = tokio::fs::read(&path)
                    .await
                    .with_context(|| format!("reading {}", path.display()))?;
                let ct = mime_guess::from_path(&path)
                    .first_or_octet_stream()
                    .to_string();
                client
                    .put_object()
                    .bucket(bucket.as_ref())
                    .key(&key)
                    .body(ByteStream::from(data))
                    .content_type(ct)
                    .send()
                    .await
                    .with_context(|| format!("uploading {key}"))?;
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n.is_multiple_of(100) {
                    tracing::info!("  {}/{} files uploaded...", n, total);
                }
                Ok::<(), anyhow::Error>(())
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<()>>()?;

    let skipped = skipped.load(std::sync::atomic::Ordering::Relaxed);
    Ok(Uploaded {
        sent: total - skipped,
        skipped,
    })
}
