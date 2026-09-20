//! Importing an existing Mastodon instance into an eunha database.
//!
//! Eunha builds Mastodon's schema exactly, so an instance moves here by having
//! its data restored into a database eunha has migrated — there is nothing to
//! convert. What needs care is everything around `pg_restore`: refusing a dump
//! written for a Mastodon release this binary does not build, keeping eunha's
//! own migration ledger instead of the dump's, and proving afterwards that what
//! landed is the instance the caller named.
//!
//! An instance's media moves the same way and for the same reason. Mastodon
//! stores a file's name, never its address, and derives the object key from the
//! row, so the media has to arrive under the keys the instance already minted.
//! Its `public/system` tree is laid out by exactly those keys, which makes the
//! move a copy rather than a translation.
//!
//! The domain is checked after the restore rather than before it. Nothing in a
//! custom-format dump answers "which instance is this?" without reading the
//! `accounts` data out of it, which costs as much as restoring it, so the
//! import restores first and refuses to call the result an instance it is not.
//! The database a failed import leaves behind is not repairable by re-running:
//! discard it and restore into a fresh one.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use anyhow::{bail, Context, Result};
use aws_sdk_s3::primitives::ByteStream;
use futures::StreamExt;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{config::MediaStorageConfig, media, migrate, version};

/// Mastodon's reserved `accounts` row for the instance actor, whose `username`
/// is the instance's own domain. Every federating Mastodon has one, which makes
/// it the witness for what domain a dump's data was written under.
const INSTANCE_ACTOR_ID: i64 = -99;

/// Table entries a Mastodon dump carries that this database keeps its own copy
/// of. Rails' two ledgers are seeded by eunha's migrations — `schema_migrations`
/// is what makes the database self-describing, and restoring the dump's would
/// overwrite it with the same rows at best and a different history at worst.
/// PgHero's space stats belong to whatever PgHero was watching the old server.
const SKIPPED_ENTRIES: [&str; 4] = [
    "TABLE DATA public ar_internal_metadata",
    "TABLE DATA public schema_migrations",
    "TABLE DATA public pghero_space_stats",
    "SEQUENCE SET public pghero_space_stats_id_seq",
];

/// A Mastodon dump and the instance it is claimed to hold.
#[derive(Debug, Clone)]
pub struct Import {
    /// A custom-format dump, as `pg_dump -Fc` writes.
    pub dump: PathBuf,
    /// The domain the imported instance answers to afterwards.
    pub domain: String,
    /// The domain the dump was written under, when that is not `domain`.
    ///
    /// Renaming an instance abandons its ActivityPub identity: remote servers
    /// remember its accounts at the old domain and will not follow them to a
    /// new one. It exists for moving an instance nobody federates with yet.
    pub rename_from: Option<String>,
    /// Restore a dump from another Mastodon release anyway.
    pub allow_schema_mismatch: bool,
}

/// What a dump holds and whether this database can take it, read before
/// anything is written.
#[derive(Debug)]
pub struct Plan {
    /// The newest Mastodon migration the dump records, which identifies the
    /// schema its data was written for. `None` when the dump records none.
    pub dump_schema_version: Option<String>,
    /// What this binary's migrations build.
    pub expected_schema_version: &'static str,
    /// Migrations this database has not applied, if any.
    pub pending_migrations: Option<String>,
    /// Rows already in `public.accounts`. A restore goes into an empty schema.
    pub existing_accounts: i64,
    /// Whether the connected role may disable the triggers a data-only restore
    /// has to load through.
    pub superuser: bool,
}

impl Plan {
    /// Why this import must not run. An empty list means it may.
    pub fn refusals(&self, import: &Import) -> Vec<String> {
        let mut refusals = Vec::new();
        if let Some(pending) = &self.pending_migrations {
            refusals.push(format!(
                "this database is behind this binary ({pending}); run `eunha migrate` first"
            ));
        }
        if self.existing_accounts > 0 {
            refusals.push(format!(
                "this database already holds {} account(s); import into an empty one",
                self.existing_accounts
            ));
        }
        if !self.superuser {
            refusals.push(
                "a data-only restore loads through the schema's foreign-key triggers, which \
                 only a superuser may disable; connect as one"
                    .into(),
            );
        }
        match self.dump_schema_version.as_deref() {
            Some(found)
                if found != self.expected_schema_version && !import.allow_schema_mismatch =>
            {
                refusals.push(format!(
                    "the dump is at Mastodon schema {found} and this eunha builds {}; data \
                     shaped for one schema would go into another, where columns added since \
                     {found} keep their defaults instead of being backfilled and columns \
                     dropped since have nowhere to go. Upgrade the source Mastodon to {} and \
                     dump again, or use an eunha that tracks {found}. \
                     `--allow-schema-mismatch` proceeds anyway",
                    self.expected_schema_version,
                    version::MASTODON
                ));
            }
            _ => {}
        }
        refusals
    }

    /// What is worth saying even when the import may proceed.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if self.dump_schema_version.is_none() {
            warnings.push(format!(
                "the dump records no Mastodon migrations, so the release it was written for \
                 cannot be checked; continuing as though it were {}",
                self.expected_schema_version
            ));
        }
        warnings
    }
}

/// What the import put in the database.
#[derive(Debug)]
pub struct Report {
    pub domain: String,
    pub renamed_from: Option<String>,
    /// Table name and row count, in the order they are worth reading.
    pub counts: Vec<(&'static str, i64)>,
}

/// Read the dump and the database, without writing to either.
pub async fn plan(db: &PgPool, import: &Import) -> Result<Plan> {
    anyhow::ensure!(
        tokio::fs::try_exists(&import.dump).await.unwrap_or(false),
        "no dump at {}",
        import.dump.display()
    );
    Ok(Plan {
        dump_schema_version: dump_schema_version(&import.dump).await?,
        expected_schema_version: version::MASTODON_SCHEMA,
        pending_migrations: migrate::pending(db)
            .await?
            .map(|pending| pending.to_string()),
        existing_accounts: sqlx::query_scalar::<_, i64>("SELECT count(*) FROM public.accounts")
            .fetch_one(db)
            .await
            .context("counting the accounts already in this database")?,
        superuser: sqlx::query_scalar::<_, String>("SELECT current_setting('is_superuser')")
            .fetch_one(db)
            .await?
            == "on",
    })
}

/// Restore the dump into `database_url`'s database and prove what landed.
///
/// `db` and `database_url` are the same database: `pg_restore` is a separate
/// process and takes the connection string, while the checks around it run on
/// the pool that already exists.
pub async fn run(db: &PgPool, database_url: &str, import: &Import) -> Result<Report> {
    let plan = plan(db, import).await?;
    let refusals = plan.refusals(import);
    if !refusals.is_empty() {
        bail!(
            "this dump cannot be imported:\n{}",
            refusals
                .iter()
                .map(|refusal| format!("  - {refusal}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    for warning in plan.warnings() {
        tracing::warn!("{warning}");
    }

    restore(&import.dump, database_url).await?;

    if let Some(old) = &import.rename_from {
        rename(db, old, &import.domain).await?;
    }
    verify_domain(db, &import.domain).await?;

    Ok(Report {
        domain: import.domain.clone(),
        renamed_from: import.rename_from.clone(),
        counts: counts(db).await?,
    })
}

/// `pg_restore`, `pg_dump` and friends, from `PGBIN` when they are not on PATH.
fn pg_command(name: &str) -> tokio::process::Command {
    let binary = match std::env::var("PGBIN") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join(name),
        _ => PathBuf::from(name),
    };
    tokio::process::Command::new(binary)
}

/// The newest Mastodon migration the dump records.
///
/// Mastodon writes every migration it has run into `public.schema_migrations`,
/// so the newest one identifies the schema its data was written for. Restoring
/// just that table to stdout gives a COPY stream whose payload lines are the
/// versions themselves.
async fn dump_schema_version(dump: &Path) -> Result<Option<String>> {
    let output = pg_command("pg_restore")
        .args(["--data-only", "--table=schema_migrations", "-f", "-"])
        .arg(dump)
        .stderr(Stdio::piped())
        .output()
        .await
        .context("running pg_restore; is PostgreSQL's bin directory on PATH, or PGBIN set?")?;
    let newest = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| line.len() == 14 && line.bytes().all(|byte| byte.is_ascii_digit()))
        .max()
        .map(str::to_owned);
    // A dump without the table is not an error — a dump this cannot be read at
    // all is. Reporting no versions and failing are told apart by the exit code.
    if newest.is_none() && !output.status.success() {
        bail!(
            "could not read {}: {}",
            dump.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(newest)
}

/// Load the dump's data, and only its data, into an already-migrated database.
async fn restore(dump: &Path, database_url: &str) -> Result<()> {
    let list = pg_command("pg_restore")
        .arg("-l")
        .arg(dump)
        .stderr(Stdio::piped())
        .output()
        .await
        .context("listing the dump's contents")?;
    anyhow::ensure!(
        list.status.success(),
        "could not list {}: {}",
        dump.display(),
        String::from_utf8_lossy(&list.stderr).trim()
    );
    let kept: String = String::from_utf8_lossy(&list.stdout)
        .lines()
        .filter(|line| !SKIPPED_ENTRIES.iter().any(|skipped| line.contains(skipped)))
        .map(|line| format!("{line}\n"))
        .collect();

    let toc = std::env::temp_dir().join(format!("eunha-import-{}.toc", Uuid::new_v4()));
    tokio::fs::write(&toc, kept.as_bytes())
        .await
        .with_context(|| format!("writing {}", toc.display()))?;
    let restored = pg_command("pg_restore")
        .args([
            "--data-only",
            "--no-owner",
            "--no-privileges",
            // One transaction, so a dump that cannot be loaded in full leaves
            // an empty database rather than a partial instance.
            "--single-transaction",
            // A data-only restore loads tables in the dump's order, not in one
            // that satisfies their foreign keys.
            "--disable-triggers",
        ])
        .arg("--use-list")
        .arg(&toc)
        .arg("-d")
        .arg(database_url)
        .arg(dump)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("restoring the dump")?;
    let _ = tokio::fs::remove_file(&toc).await;
    anyhow::ensure!(
        restored.status.success(),
        "the restore failed and nothing was committed: {}",
        String::from_utf8_lossy(&restored.stderr).trim()
    );
    Ok(())
}

/// Move every local URL the instance minted to a new domain.
///
/// Mastodon derives a local account's or status's address from configuration
/// rather than storing it, except where it has already handed the address to
/// somebody else, which is what these columns are. The instance actor is
/// renamed with them: its `username` *is* the domain, and nothing else would
/// notice it was stale.
async fn rename(db: &PgPool, old: &str, new: &str) -> Result<()> {
    let mut tx = db.begin().await?;
    sqlx::query(
        r#"UPDATE public.accounts SET
             uri                     = replace(uri,                     $1, $2),
             url                     = replace(url,                     $1, $2),
             inbox_url               = replace(inbox_url,               $1, $2),
             outbox_url              = replace(outbox_url,              $1, $2),
             shared_inbox_url        = replace(shared_inbox_url,        $1, $2),
             followers_url           = replace(followers_url,           $1, $2),
             following_url           = replace(following_url,           $1, $2),
             featured_collection_url = replace(featured_collection_url, $1, $2),
             collections_url         = replace(collections_url,         $1, $2)
           WHERE domain IS NULL"#,
    )
    .bind(format!("https://{old}"))
    .bind(format!("https://{new}"))
    .execute(&mut *tx)
    .await
    .context("rewriting local account URLs")?;
    sqlx::query("UPDATE public.accounts SET username=$1 WHERE id=$2")
        .bind(new)
        .bind(INSTANCE_ACTOR_ID)
        .execute(&mut *tx)
        .await
        .context("renaming the instance actor")?;
    sqlx::query(
        r#"UPDATE public.statuses SET
             uri = replace(uri, $1, $2),
             url = replace(url, $1, $2)
           WHERE uri LIKE $1 || '%'"#,
    )
    .bind(format!("https://{old}"))
    .bind(format!("https://{new}"))
    .execute(&mut *tx)
    .await
    .context("rewriting local status URLs")?;
    tx.commit().await?;
    Ok(())
}

/// Refuse to call the restored data an instance it is not.
async fn verify_domain(db: &PgPool, domain: &str) -> Result<()> {
    let actor = sqlx::query_scalar::<_, String>("SELECT username FROM public.accounts WHERE id=$1")
        .bind(INSTANCE_ACTOR_ID)
        .fetch_optional(db)
        .await?;
    match actor.as_deref() {
        Some(found) if found.eq_ignore_ascii_case(domain) => {}
        Some(found) => bail!(
            "the dump holds {found}, not {domain}. Its data was restored but belongs to another \
             instance: discard this database rather than serving it. To move an instance that \
             nobody federates with yet, import it again with `--rename-from {found}`."
        ),
        // Only an instance that has never signed a federated request, which in
        // practice means one that has never run.
        None => tracing::warn!(
            "the dump has no instance actor, so the domain its data was written under could not \
             be confirmed; continuing as {domain}"
        ),
    }
    // A local account's own address is the other witness, and a mismatched one
    // means the rename was partial rather than that the dump is the wrong one.
    let strays = sqlx::query_scalar::<_, String>(
        r#"SELECT DISTINCT split_part(split_part(uri, '://', 2), '/', 1)
           FROM public.accounts
           WHERE domain IS NULL AND uri IS NOT NULL AND uri <> ''
             AND split_part(split_part(uri, '://', 2), '/', 1) <> $1
           LIMIT 5"#,
    )
    .bind(domain)
    .fetch_all(db)
    .await?;
    anyhow::ensure!(
        strays.is_empty(),
        "local accounts still address themselves at {} rather than {domain}",
        strays.join(", ")
    );
    Ok(())
}

/// What the instance is made of, as evidence that the restore is complete.
async fn counts(db: &PgPool) -> Result<Vec<(&'static str, i64)>> {
    let row = sqlx::query_as::<_, (i64, i64, i64, i64, i64)>(
        r#"SELECT (SELECT count(*) FROM public.users),
                  (SELECT count(*) FROM public.accounts WHERE domain IS NULL),
                  (SELECT count(*) FROM public.accounts),
                  (SELECT count(*) FROM public.statuses),
                  (SELECT count(*) FROM public.media_attachments)"#,
    )
    .fetch_one(db)
    .await?;
    Ok(vec![
        ("users", row.0),
        ("local accounts", row.1),
        ("known accounts", row.2),
        ("statuses", row.3),
        ("media attachments", row.4),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import() -> Import {
        Import {
            dump: PathBuf::from("dump.custom"),
            domain: "seoul.earth".into(),
            rename_from: None,
            allow_schema_mismatch: false,
        }
    }

    fn ready() -> Plan {
        Plan {
            dump_schema_version: Some(version::MASTODON_SCHEMA.to_owned()),
            expected_schema_version: version::MASTODON_SCHEMA,
            pending_migrations: None,
            existing_accounts: 0,
            superuser: true,
        }
    }

    #[test]
    fn a_matching_dump_into_an_empty_migrated_database_is_allowed() {
        assert!(ready().refusals(&import()).is_empty());
        assert!(ready().warnings().is_empty());
    }

    #[test]
    fn a_dump_from_another_mastodon_release_is_refused_unless_allowed() {
        let plan = Plan {
            dump_schema_version: Some("20240101000000".into()),
            ..ready()
        };
        assert_eq!(plan.refusals(&import()).len(), 1);
        let allowed = Import {
            allow_schema_mismatch: true,
            ..import()
        };
        assert!(plan.refusals(&allowed).is_empty());
    }

    #[test]
    fn a_dump_that_records_no_release_is_warned_about_rather_than_refused() {
        let plan = Plan {
            dump_schema_version: None,
            ..ready()
        };
        assert!(plan.refusals(&import()).is_empty());
        assert_eq!(plan.warnings().len(), 1);
    }

    #[test]
    fn an_occupied_or_unmigrated_database_and_an_ordinary_role_are_each_refused() {
        assert_eq!(
            Plan {
                existing_accounts: 12,
                ..ready()
            }
            .refusals(&import())
            .len(),
            1
        );
        assert_eq!(
            Plan {
                pending_migrations: Some("1 migration(s) not applied: 42".into()),
                ..ready()
            }
            .refusals(&import())
            .len(),
            1
        );
        assert_eq!(
            Plan {
                superuser: false,
                ..ready()
            }
            .refusals(&import())
            .len(),
            1
        );
    }

    #[test]
    fn the_dumps_own_migration_ledger_is_never_restored() {
        let listing = "\
2891; 0 16528 TABLE DATA public accounts postgres
2892; 0 16529 TABLE DATA public schema_migrations postgres
2893; 0 16530 TABLE DATA public ar_internal_metadata postgres
2894; 0 16531 TABLE DATA public statuses postgres
2895; 0 16532 TABLE DATA public pghero_space_stats postgres";
        let kept: Vec<&str> = listing
            .lines()
            .filter(|line| !SKIPPED_ENTRIES.iter().any(|skipped| line.contains(skipped)))
            .collect();
        assert_eq!(kept.len(), 2);
        assert!(kept
            .iter()
            .all(|line| line.contains("accounts") || line.contains("statuses")));
    }
}

/// What an upload of an instance's media moved.
#[derive(Debug, Default)]
pub struct Uploaded {
    /// Files found under the media directory.
    pub total: usize,
    /// Files sent to the bucket.
    pub sent: usize,
    /// Files the bucket already had, with `skip_existing`.
    pub skipped: usize,
    /// The namespace the objects went under, which is what keeps one instance's
    /// media apart from another's in a bucket they share. Reported because
    /// uploading a library to the wrong prefix looks exactly like uploading it
    /// to the right one until somebody asks for a picture.
    pub key_prefix: String,
}

/// Copy a Mastodon `public/system` tree into an instance's own storage.
///
/// The tree's layout is the instance's object keys, so each file's path
/// relative to the directory is the key it goes to, under whatever prefix the
/// instance's storage namespaces it with.
///
/// `skip_existing` asks for each object before sending it. An upload that was
/// interrupted then resumes at the cost of a request per file it already moved
/// rather than the file again, which over a whole media library is the
/// difference between minutes and hours; a first run would pay that for
/// nothing, so the caller chooses.
pub async fn upload_media(
    storage: &MediaStorageConfig,
    media_dir: &Path,
    concurrency: usize,
    skip_existing: bool,
) -> Result<Uploaded> {
    anyhow::ensure!(
        media_dir.is_dir(),
        "no media directory at {}",
        media_dir.display()
    );
    let endpoint = storage
        .endpoint
        .clone()
        .context("the instance's media storage has no endpoint")?;
    let credentials = aws_sdk_s3::config::Credentials::new(
        &storage.access_key_id,
        &storage.secret_access_key,
        None,
        None,
        "static",
    );
    let client = Arc::new(aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::config::Builder::new()
            .region(aws_sdk_s3::config::Region::new("auto".to_string()))
            .credentials_provider(credentials)
            .endpoint_url(&endpoint)
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .build(),
    ));

    let files = collect_files(media_dir)?;
    let total = files.len();
    tracing::info!(
        "uploading {total} files from {} (concurrency={concurrency})",
        media_dir.display()
    );
    let bucket = Arc::new(storage.bucket.clone());
    let key_prefix = Arc::new(storage.key_prefix.clone());
    let root = Arc::new(media_dir.to_path_buf());
    let done = Arc::new(AtomicUsize::new(0));
    let skipped = Arc::new(AtomicUsize::new(0));

    futures::stream::iter(files)
        .map(|path| {
            let (client, bucket, key_prefix, root) = (
                client.clone(),
                bucket.clone(),
                key_prefix.clone(),
                root.clone(),
            );
            let (done, skipped) = (done.clone(), skipped.clone());
            async move {
                let relative = path
                    .strip_prefix(root.as_ref())
                    .expect("collected under the media directory");
                let key = media::prefixed_key(
                    &key_prefix,
                    &relative.to_string_lossy().replace('\\', "/"),
                );
                let already_there = skip_existing
                    && client
                        .head_object()
                        .bucket(bucket.as_ref())
                        .key(&key)
                        .send()
                        .await
                        .is_ok();
                if !already_there {
                    let body = tokio::fs::read(&path)
                        .await
                        .with_context(|| format!("reading {}", path.display()))?;
                    client
                        .put_object()
                        .bucket(bucket.as_ref())
                        .key(&key)
                        .body(ByteStream::from(body))
                        .content_type(
                            mime_guess::from_path(&path)
                                .first_or_octet_stream()
                                .to_string(),
                        )
                        .send()
                        .await
                        .with_context(|| format!("uploading {key}"))?;
                } else {
                    skipped.fetch_add(1, Ordering::Relaxed);
                }
                let moved = done.fetch_add(1, Ordering::Relaxed) + 1;
                if moved.is_multiple_of(100) {
                    tracing::info!("  {moved}/{total} files");
                }
                Ok::<(), anyhow::Error>(())
            }
        })
        .buffer_unordered(concurrency)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<()>>()?;

    let skipped = skipped.load(Ordering::Relaxed);
    Ok(Uploaded {
        total,
        sent: total - skipped,
        skipped,
        key_prefix: storage.key_prefix.trim_matches('/').to_owned(),
    })
}

/// Every file under the tree, ignoring the dotfiles a copy leaves behind.
fn collect_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_files_into(dir, &mut files)?;
    Ok(files)
}

fn collect_files_into(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect_files_into(&path, out)?;
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| !name.starts_with('.'))
        {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod media_tests {
    use crate::media::prefixed_key;

    /// A tenant sharing a bucket is kept apart from its neighbours by its
    /// prefix alone, so an upload has to put a file exactly where the serving
    /// process will look for it.
    #[test]
    fn an_uploaded_file_lands_where_the_instance_will_look_for_it() {
        let logical = "media_attachments/files/109/372/original/a1b2.jpg";
        assert_eq!(
            prefixed_key("t/8f14e45f-ea8d-4b41-9d0a-1b2c3d4e5f60", logical),
            "t/8f14e45f-ea8d-4b41-9d0a-1b2c3d4e5f60/media_attachments/files/109/372/original/a1b2.jpg"
        );
        // A dedicated bucket namespaces nothing, and must not gain a leading
        // slash that would make every key a different object.
        assert_eq!(prefixed_key("", logical), logical);
        assert_eq!(prefixed_key("/t/one/", logical), format!("t/one/{logical}"));
    }
}
