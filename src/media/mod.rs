use uuid::Uuid;

use crate::{
    config::MediaStorageConfig,
    error::{AppError, AppResult},
};

pub mod picture;
pub mod profile;
pub mod transcode;

pub struct Storage {
    client: aws_sdk_s3::Client,
    bucket: String,
    key_prefix: String,
    base_url: String,
}

impl Storage {
    pub async fn from_config(config: &MediaStorageConfig) -> Self {
        let creds = aws_sdk_s3::config::Credentials::new(
            &config.access_key_id,
            &config.secret_access_key,
            None,
            None,
            "static",
        );
        let mut builder = aws_sdk_s3::config::Builder::new()
            .region(aws_sdk_s3::config::Region::new(config.region.clone()))
            .credentials_provider(creds)
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest());
        if let Some(ep) = &config.endpoint {
            builder = builder.endpoint_url(ep).force_path_style(true);
        }
        let client = aws_sdk_s3::Client::from_conf(builder.build());
        Storage {
            client,
            bucket: config.bucket.clone(),
            key_prefix: config.key_prefix.trim_matches('/').to_string(),
            base_url: config.base_url.clone(),
        }
    }

    /// Where `key` is kept in the bucket: Paperclip's `path`, under
    /// `S3_KEY_PREFIX` when one is set.
    pub fn object_key(&self, key: &str) -> String {
        prefixed_key(&self.key_prefix, key)
    }

    pub async fn store(&self, data: &[u8], key: &str, content_type: &str) -> AppResult<String> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(self.object_key(key))
            .body(data.to_vec().into())
            .content_type(content_type)
            .send()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("S3 upload: {}", e)))?;
        Ok(key.to_string())
    }

    /// [`Storage::store`] for a file on disk, streamed rather than read into
    /// memory: an archive takeout can be large.
    pub async fn store_file(
        &self,
        path: &std::path::Path,
        key: &str,
        content_type: &str,
    ) -> AppResult<String> {
        let body = aws_sdk_s3::primitives::ByteStream::from_path(path)
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("S3 upload body: {}", e)))?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(self.object_key(key))
            .body(body)
            .content_type(content_type)
            .send()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("S3 upload: {}", e)))?;
        Ok(key.to_string())
    }

    /// A link that fetches `key` for `expires_in`, signed with the storage's
    /// credentials: Paperclip's `expiring_url`, which Mastodon hands out for
    /// an archive takeout.
    pub async fn presigned_url(
        &self,
        key: &str,
        expires_in: std::time::Duration,
    ) -> AppResult<String> {
        let config = aws_sdk_s3::presigning::PresigningConfig::expires_in(expires_in)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("S3 presigning: {}", e)))?;
        let request = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.object_key(key))
            .presigned(config)
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("S3 presigning: {}", e)))?;
        Ok(request.uri().to_string())
    }

    pub fn public_url(&self, key: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            self.object_key(key)
        )
    }

    pub fn missing_avatar_url(&self) -> String {
        self.public_url("avatars/original/missing.png")
    }

    pub fn missing_header_url(&self) -> String {
        self.public_url("headers/original/missing.png")
    }

    pub async fn get(&self, key: &str) -> AppResult<Vec<u8>> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(self.object_key(key))
            .send()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("S3 get: {}", e)))?;
        let data = resp
            .body
            .collect()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("S3 body: {}", e)))?;
        Ok(data.into_bytes().to_vec())
    }

    pub async fn delete(&self, key: &str) -> AppResult<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(self.object_key(key))
            .send()
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("S3 delete: {}", e)))?;
        Ok(())
    }
}

/// Convert the logical key stored in PostgreSQL into its physical object key.
/// Media maintenance binaries use the same boundary as the serving process.
pub fn prefixed_key(prefix: &str, key: &str) -> String {
    let prefix = prefix.trim_matches('/');
    let key = key.trim_start_matches('/');
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}/{key}")
    }
}

// ── Key generation ────────────────────────────────────────────────────────

pub fn singleton_icon_key(content_type: &str) -> String {
    let ext = ext_for(content_type);
    format!("instance/icon/{}.{}", random_hex(), ext)
}

pub fn account_avatar_key(account_id: i64, content_type: &str) -> String {
    let ext = ext_for(content_type);
    format!(
        "accounts/avatars/{}/original/{}.{}",
        int_to_path(account_id),
        random_hex(),
        ext,
    )
}

pub fn account_header_key(account_id: i64, content_type: &str) -> String {
    let ext = ext_for(content_type);
    format!(
        "accounts/headers/{}/original/{}.{}",
        int_to_path(account_id),
        random_hex(),
        ext,
    )
}

pub struct MediaAttachmentKeys {
    pub original: String,
    pub small: String,
}

pub fn media_attachment_keys(content_type: &str) -> MediaAttachmentKeys {
    let ext = ext_for(content_type);
    let base = format!("media_attachments/files/{}", uuid_to_path(Uuid::new_v4()),);
    let name = random_hex();
    MediaAttachmentKeys {
        original: format!("{}/original/{}.{}", base, name, ext),
        small: format!("{}/small/{}.{}", base, name, ext),
    }
}

fn uuid_to_path(id: Uuid) -> String {
    let hex = id.simple().to_string();
    hex.as_bytes()
        .chunks(3)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("/")
}

pub fn int_to_path(id: i64) -> String {
    // Format as zero-padded 9-digit decimal, split into 3-digit chunks — matches Mastodon's Paperclip convention
    let s = format!("{:09}", id);
    s.as_bytes()
        .chunks(3)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("/")
}

/// Every object a media attachment's files may be kept under, which is what
/// Paperclip removes when a `MediaAttachment` is destroyed: the file's
/// `original` and `small` styles, the custom thumbnail's, and the same under
/// `cache/`, where a remote attachment's copy is kept.
pub fn attachment_keys(id: i64, file: Option<&str>, thumbnail: Option<&str>) -> Vec<String> {
    let partition = int_to_path(id);
    let mut keys = vec![];
    for (attachment, name) in [("files", file), ("thumbnails", thumbnail)] {
        let Some(name) = name.filter(|n| !n.is_empty()) else {
            continue;
        };
        for style in ["original", "small"] {
            let key = format!("media_attachments/{attachment}/{partition}/{style}/{name}");
            keys.push(format!("cache/{key}"));
            keys.push(key);
        }
    }
    // Eunha keeps a custom thumbnail's preview where the file's would be.
    if let Some(name) = thumbnail.filter(|n| !n.is_empty()) {
        let key = format!("media_attachments/files/{partition}/small/{name}");
        keys.push(format!("cache/{key}"));
        keys.push(key);
    }
    keys
}

fn random_hex() -> String {
    let bytes = Uuid::new_v4().into_bytes();
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn ext_for_content_type(content_type: &str) -> &'static str {
    ext_for(content_type)
}

fn ext_for(content_type: &str) -> &'static str {
    mime_guess::get_mime_extensions_str(content_type)
        .and_then(|e| e.first().copied())
        .unwrap_or("bin")
}

#[cfg(test)]
mod tests {
    use super::prefixed_key;

    #[test]
    fn empty_prefix_preserves_existing_object_keys() {
        assert_eq!(
            prefixed_key("", "accounts/avatars/a.png"),
            "accounts/avatars/a.png"
        );
    }

    #[test]
    fn prefix_namespaces_object_keys_once() {
        assert_eq!(
            prefixed_key("/tenants/abc/", "/media/file.png"),
            "tenants/abc/media/file.png"
        );
    }
}
