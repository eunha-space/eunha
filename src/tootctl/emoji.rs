//! `tootctl emoji` (`Mastodon::CLI::Emoji`): custom emoji packs in and out of
//! the instance, as gzipped tarballs of PNG and GIF files named for their
//! shortcodes.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use sqlx::Row as _;

use super::console::Console;
use crate::custom_emoji::{self, ImageRef};
use crate::state::AppState;

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Import emoji from a TAR GZIP archive at PATH, as `tootctl emoji import`
    /// does.
    ///
    /// Each `.png` and `.gif` file in it becomes a local emoji, its shortcode
    /// the file's name. An emoji that exists already is skipped, unless
    /// `--overwrite`.
    Import {
        path: PathBuf,
        /// Put this before every shortcode.
        #[arg(long)]
        prefix: Option<String>,
        /// Put this after every shortcode.
        #[arg(long)]
        suffix: Option<String>,
        /// Replace the image of an emoji that exists already.
        #[arg(long)]
        overwrite: bool,
        /// Keep the emoji out of the emoji picker. They can still be used.
        #[arg(long)]
        unlisted: bool,
        /// Group the emoji under this category, made if it does not exist.
        #[arg(long)]
        category: Option<String>,
    },
    /// Export the local emoji to `export.tar.gz` in the directory PATH, as
    /// `tootctl emoji export` does.
    Export {
        path: PathBuf,
        /// Only the emoji of this category.
        #[arg(long)]
        category: Option<String>,
        /// Replace an archive that is there already.
        #[arg(long)]
        overwrite: bool,
    },
    /// Remove all custom emoji, as `tootctl emoji purge` does.
    Purge {
        /// Only other servers' emoji.
        #[arg(long)]
        remote_only: bool,
        /// Only the emoji of servers suspended by a domain block, and of
        /// their subdomains.
        #[arg(long)]
        suspended_only: bool,
    },
}

impl Command {
    pub async fn run(self, state: &AppState, console: &dyn Console) -> anyhow::Result<()> {
        match self {
            Self::Import {
                path,
                prefix,
                suffix,
                overwrite,
                unlisted,
                category,
                ..
            } => {
                import(
                    state,
                    console,
                    &path,
                    &ImportOptions {
                        prefix,
                        suffix,
                        overwrite,
                        unlisted,
                        category,
                    },
                )
                .await
            }
            Self::Export {
                path,
                category,
                overwrite,
                ..
            } => export(state, console, &path, category.as_deref(), overwrite).await,
            Self::Purge {
                remote_only,
                suspended_only,
                ..
            } => purge(state, console, remote_only, suspended_only).await,
        }
    }
}

/// What `emoji import` was asked to do.
#[derive(Debug, Default, Clone)]
pub struct ImportOptions {
    pub prefix: Option<String>,
    pub suffix: Option<String>,
    pub overwrite: bool,
    pub unlisted: bool,
    pub category: Option<String>,
}

/// The emoji images in a gzipped tarball: each `.png` or `.gif` file, by its
/// name in the archive.
fn read_pack(path: &Path) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(file));
    let mut images = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let name = entry.path()?.to_string_lossy().into_owned();
        if !(name.ends_with(".png") || name.ends_with(".gif")) {
            continue;
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        images.push((name, data));
    }
    Ok(images)
}

/// `File.basename(name, '.*')`.
fn basename_without_extension(name: &str) -> &str {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rfind('.') {
        Some(0) | None => base,
        Some(dot) => &base[..dot],
    }
}

/// `CustomEmojiCategory.find_or_create_by(name:)`.
async fn category_named(state: &AppState, name: &str) -> anyhow::Result<i64> {
    if let Some(id) = sqlx::query_scalar("SELECT id FROM custom_emoji_categories WHERE name = $1")
        .bind(name)
        .fetch_optional(&state.db)
        .await?
    {
        return Ok(id);
    }
    Ok(sqlx::query_scalar(
        "INSERT INTO custom_emoji_categories (name, created_at, updated_at)
         VALUES ($1, now(), now())
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
         RETURNING id",
    )
    .bind(name)
    .fetch_one(&state.db)
    .await?)
}

/// `Emoji#import`.
pub async fn import(
    state: &AppState,
    console: &dyn Console,
    path: &Path,
    options: &ImportOptions,
) -> anyhow::Result<()> {
    let (mut imported, mut skipped, mut failed) = (0, 0, 0);
    let category = match &options.category {
        Some(name) => Some(category_named(state, name).await?),
        None => None,
    };
    let path = path.to_owned();
    let images = crate::tenants::spawn_blocking(move || read_pack(&path)).await??;
    for (name, data) in images {
        let filename = basename_without_extension(&name);
        // macOS's shadow files.
        if filename.starts_with("._") {
            continue;
        }
        let shortcode = format!(
            "{}{filename}{}",
            options.prefix.as_deref().unwrap_or_default(),
            options.suffix.as_deref().unwrap_or_default()
        );
        let existing: Option<(i64, String, Option<String>, Option<i32>)> = sqlx::query_as(
            "SELECT id, shortcode, image_file_name, image_storage_schema_version
             FROM custom_emojis
             WHERE domain IS NULL AND lower(shortcode) = lower($1)
             ORDER BY id LIMIT 1",
        )
        .bind(&shortcode)
        .fetch_optional(&state.db)
        .await?;
        if existing.is_some() && !options.overwrite {
            skipped += 1;
            continue;
        }
        let declared = mime_guess::from_path(&name).first_raw().unwrap_or_default();
        let content_type = custom_emoji::content_type_of(&data, declared);
        // `Mastodon::DimensionsValidationError`, raised before validation,
        // ends the import.
        custom_emoji::check_dimensions(&data, &content_type)
            .map_err(|e| anyhow::anyhow!("{name}: {e:?}"))?;
        let mut errors = custom_emoji::image_errors(Some((&data, &content_type)));
        let shortcode_valid = match &existing {
            // The emoji keeps the shortcode it has.
            Some(_) => true,
            None => {
                let taken: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM custom_emojis WHERE domain IS NULL AND shortcode = $1)",
                )
                .bind(&shortcode)
                .fetch_one(&state.db)
                .await?;
                !taken && custom_emoji::shortcode_errors(&shortcode, true).is_empty()
            }
        };
        let processed = if errors.is_empty() && shortcode_valid {
            match custom_emoji::process(data, &content_type) {
                Ok(processed) => Some(processed),
                Err(_) => {
                    errors.push("Image could not be processed".to_owned());
                    None
                }
            }
        } else {
            None
        };
        let Some(processed) = processed else {
            failed += 1;
            console.say("Failure/Error: ");
            console.say(&name);
            console.say(&format!("  {}", errors.join(", ")));
            continue;
        };
        let visible_in_picker = !options.unlisted;
        let id = match &existing {
            Some((id, _, old_file, old_version)) => {
                custom_emoji::delete_files(
                    &state.storage,
                    ImageRef {
                        id: *id,
                        domain: None,
                        image_file_name: old_file.as_deref(),
                        image_remote_url: None,
                        image_storage_schema_version: *old_version,
                    },
                )
                .await;
                sqlx::query(
                    "UPDATE custom_emojis
                     SET image_file_name = $2, image_content_type = $3, image_file_size = $4,
                         image_updated_at = now(), image_storage_schema_version = 1,
                         image_remote_url = NULL, visible_in_picker = $5, category_id = $6,
                         updated_at = now()
                     WHERE id = $1",
                )
                .bind(id)
                .bind(&processed.file_name)
                .bind(&processed.content_type)
                .bind(processed.original.len() as i32)
                .bind(visible_in_picker)
                .bind(category)
                .execute(&state.db)
                .await?;
                *id
            }
            None => {
                sqlx::query_scalar(
                    "INSERT INTO custom_emojis
                       (shortcode, visible_in_picker, category_id, image_file_name,
                        image_content_type, image_file_size, image_updated_at,
                        image_storage_schema_version, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $5, $6, now(), 1, now(), now())
                     RETURNING id",
                )
                .bind(&shortcode)
                .bind(visible_in_picker)
                .bind(category)
                .bind(&processed.file_name)
                .bind(&processed.content_type)
                .bind(processed.original.len() as i32)
                .fetch_one(&state.db)
                .await?
            }
        };
        custom_emoji::store(&state.storage, id, &processed)
            .await
            .map_err(|e| anyhow::anyhow!("storing {name}: {e:?}"))?;
        imported += 1;
    }
    console.say(&format!(
        "Imported {imported}, skipped {skipped}, failed to import {failed}"
    ));
    Ok(())
}

/// `Emoji#export`.
pub async fn export(
    state: &AppState,
    console: &dyn Console,
    path: &Path,
    category: Option<&str>,
    overwrite: bool,
) -> anyhow::Result<()> {
    let export_file = path.join("export.tar.gz");
    if export_file.is_file() && !overwrite {
        bail!("Archive already exists! Use '--overwrite' to overwrite it!");
    }
    let category_id: Option<i64> = match category {
        Some(name) => Some(
            sqlx::query_scalar("SELECT id FROM custom_emoji_categories WHERE name = $1")
                .bind(name)
                .fetch_optional(&state.db)
                .await?
                .with_context(|| format!("Unable to find category '{name}'!"))?,
        ),
        None => None,
    };
    // `CustomEmoji.local`, or `category.emojis`.
    let rows = sqlx::query(
        "SELECT id, shortcode, domain, image_file_name, image_storage_schema_version
         FROM custom_emojis
         WHERE CASE WHEN $1::bigint IS NULL THEN domain IS NULL ELSE category_id = $1 END
         ORDER BY id",
    )
    .bind(category_id)
    .fetch_all(&state.db)
    .await?;
    let file = std::fs::File::create(&export_file)
        .with_context(|| format!("creating {}", export_file.display()))?;
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        file,
        flate2::Compression::default(),
    ));
    let mut exported = 0;
    for row in rows {
        let shortcode: String = row.get("shortcode");
        let domain: Option<String> = row.get("domain");
        let file_name: Option<String> = row.get("image_file_name");
        console.say(&format!("Adding '{shortcode}'..."));
        let image = ImageRef {
            id: row.get("id"),
            domain: domain.as_deref(),
            image_file_name: file_name.as_deref(),
            image_remote_url: None,
            image_storage_schema_version: row.get("image_storage_schema_version"),
        };
        let key = image
            .key("original")
            .with_context(|| format!("'{shortcode}' has no image"))?;
        let data = state
            .storage
            .get(&key)
            .await
            .map_err(|e| anyhow::anyhow!("reading '{shortcode}': {e:?}"))?;
        let extension = file_name
            .as_deref()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, extension)| format!(".{extension}"))
            .unwrap_or_default();
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
        header.set_cksum();
        tar.append_data(
            &mut header,
            format!("{shortcode}{extension}"),
            data.as_slice(),
        )?;
        exported += 1;
    }
    tar.into_inner()?.finish()?;
    console.say(&format!("Exported {exported}"));
    Ok(())
}

/// `Emoji#purge`: the rows, and their images' files.
pub async fn purge(
    state: &AppState,
    console: &dyn Console,
    remote_only: bool,
    suspended_only: bool,
) -> anyhow::Result<()> {
    if suspended_only {
        let domains: Vec<String> =
            sqlx::query_scalar("SELECT domain FROM domain_blocks WHERE severity = 1 ORDER BY id")
                .fetch_all(&state.db)
                .await?;
        for domain in domains {
            // `CustomEmoji.by_domain_and_subdomains(domain)`.
            delete_where(
                state,
                "domain = $1 OR domain ILIKE '%.' || $1",
                Some(domain.as_str()),
            )
            .await?;
        }
    } else if remote_only {
        delete_where(state, "domain IS NOT NULL", None).await?;
    } else {
        delete_where(state, "true", None).await?;
    }
    console.say("OK");
    Ok(())
}

/// Delete the emoji `condition` picks, and their files.
async fn delete_where(
    state: &AppState,
    condition: &str,
    domain: Option<&str>,
) -> anyhow::Result<u64> {
    let sql = format!("SELECT id FROM custom_emojis WHERE {condition}");
    let mut query = sqlx::query_scalar(&sql);
    if let Some(domain) = domain {
        query = query.bind(domain);
    }
    let ids: Vec<i64> = query.fetch_all(&state.db).await?;
    delete_ids(state, &ids).await
}

/// Delete these emoji, and their files.
pub(crate) async fn delete_ids(state: &AppState, ids: &[i64]) -> anyhow::Result<u64> {
    let rows = sqlx::query(
        "DELETE FROM custom_emojis WHERE id = ANY($1)
         RETURNING id, domain, image_file_name, image_storage_schema_version",
    )
    .bind(ids)
    .fetch_all(&state.db)
    .await?;
    for row in &rows {
        let domain: Option<String> = row.get("domain");
        let file_name: Option<String> = row.get("image_file_name");
        custom_emoji::delete_files(
            &state.storage,
            ImageRef {
                id: row.get("id"),
                domain: domain.as_deref(),
                image_file_name: file_name.as_deref(),
                image_remote_url: None,
                image_storage_schema_version: row.get("image_storage_schema_version"),
            },
        )
        .await;
    }
    Ok(rows.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shortcode_is_the_files_name_without_its_extension() {
        assert_eq!(basename_without_extension("pack/blobcat.png"), "blobcat");
        assert_eq!(basename_without_extension("blob.cat.gif"), "blob.cat");
        assert_eq!(
            basename_without_extension("pack/._blobcat.png"),
            "._blobcat"
        );
        assert_eq!(basename_without_extension(".png"), ".png");
    }
}
