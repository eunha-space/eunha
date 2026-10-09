//! `tootctl media`: `Mastodon::CLI::Media`, against the instance's bucket.
//!
//! Eunha keeps no copies of other servers' media
//! (`remote-account-images-not-downloaded`), so `remove` only ever finds the
//! copies a Mastodon sharing the database made, and `refresh`, which
//! downloads them again, is not offered (`maintenance-commands-in-eunha-terms`).

use std::collections::HashMap;

use crate::state::AppState;

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Remove remote media files, headers or avatars.
    ///
    /// Removes locally cached copies of media attachments (and optionally
    /// profile headers and avatars) from other servers. By default, only
    /// media attachments are removed. `--days` sets how old media attachments
    /// have to be before they are removed; for avatars and headers, how old
    /// the last webfinger request and update to the account have to be.
    Remove(RemoveOptions),
    /// Scan storage and check for files that do not belong to existing
    /// media attachments.
    ///
    /// Requires listing every object under the instance's key prefix, so it
    /// is slow, and some storage providers charge for the requests.
    RemoveOrphans {
        /// Carry on from this key.
        #[arg(long, value_name = "KEY")]
        start_after: Option<String>,
        /// Only keys starting with this.
        #[arg(long)]
        prefix: Option<String>,
        /// Report what would be done, changing nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Fetch remote media files. Not offered: eunha keeps no copies of
    /// remote media.
    Refresh {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        domain: Option<String>,
        #[arg(long)]
        status: Option<i64>,
        #[arg(long)]
        days: Option<i64>,
        /// How many records to work on at once.
        #[arg(short = 'c', long, default_value_t = 5)]
        concurrency: usize,
        /// Print each record as it is processed.
        #[arg(short = 'v', long)]
        verbose: bool,
        /// Report what would be done, changing nothing.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        force: bool,
    },
    /// Calculate disk space consumed by the instance.
    Usage,
    /// Look up where media is displayed by passing a media URL.
    Lookup { url: String },
}

#[derive(clap::Args, Debug, Clone)]
pub struct RemoveOptions {
    /// How old, in days, what is removed must be.
    #[arg(short = 'd', long, default_value_t = 7)]
    pub days: i64,
    /// Remove only avatars and headers.
    #[arg(long)]
    pub prune_profiles: bool,
    /// Remove only headers.
    #[arg(long)]
    pub remove_headers: bool,
    /// With `--prune-profiles` or `--remove-headers`, prune every remote
    /// profile, not only those no local account follows or is followed by.
    #[arg(long)]
    pub include_follows: bool,
    /// How many records to work on at once.
    #[arg(short = 'c', long, default_value_t = 5)]
    pub concurrency: usize,
    /// Print each record as it is processed.
    #[arg(short = 'v', long)]
    pub verbose: bool,
    /// Report what would be done, changing nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Keep media attached to a post a local account favourited,
    /// bookmarked, quoted, replied to or boosted.
    #[arg(long)]
    pub keep_interacted: bool,
}

impl Default for RemoveOptions {
    fn default() -> Self {
        Self {
            days: 7,
            prune_profiles: false,
            remove_headers: false,
            include_follows: false,
            concurrency: 5,
            verbose: false,
            dry_run: false,
            keep_interacted: false,
        }
    }
}

impl Command {
    pub(crate) fn concurrency(&self) -> usize {
        match self {
            Self::Remove(options) => options.concurrency,
            Self::Refresh { concurrency, .. } => *concurrency,
            _ => 1,
        }
    }
}

pub async fn run(state: &AppState, command: Command) -> anyhow::Result<()> {
    match command {
        Command::Remove(options) => {
            for line in remove(state, &options).await? {
                println!("{line}");
            }
        }
        Command::RemoveOrphans {
            start_after,
            prefix,
            dry_run,
        } => {
            let report = remove_orphans(
                state,
                prefix.as_deref(),
                start_after.as_deref(),
                dry_run,
                |line| println!("{line}"),
            )
            .await;
            println!(
                "Removed {} orphans (approx. {}){}",
                report.removed,
                super::human_size(report.bytes),
                super::dry_run_suffix(dry_run)
            );
        }
        Command::Refresh { .. } => refresh()?,
        Command::Usage => print_table(&usage(state).await?),
        Command::Lookup { url } => println!("{}", lookup(state, &url).await?),
    }
    Ok(())
}

/// `tootctl media refresh`, which eunha does not offer.
pub fn refresh() -> anyhow::Result<()> {
    anyhow::bail!(
        "eunha keeps no copies of remote media, so there is nothing to download again; \
         clients load it from the servers it came from"
    )
}

/// `Media#remove`: what it would print.
pub async fn remove(state: &AppState, options: &RemoveOptions) -> anyhow::Result<Vec<String>> {
    anyhow::ensure!(
        !(options.prune_profiles && options.remove_headers),
        "--prune-profiles and --remove-headers should not be specified simultaneously"
    );
    anyhow::ensure!(
        !options.include_follows || options.prune_profiles || options.remove_headers,
        "--include-follows can only be used with --prune-profiles or --remove-headers"
    );
    let suffix = super::dry_run_suffix(options.dry_run);
    if options.prune_profiles || options.remove_headers {
        let tally = prune_profiles(state, options).await?;
        return Ok(vec![format!(
            "Visited {} accounts and removed profile media totaling {}{suffix}",
            tally.processed,
            super::human_size(tally.aggregate)
        )]);
    }
    let tally = remove_attachments(state, options).await?;
    Ok(vec![format!(
        "Removed {} media attachments (approx. {}){suffix}",
        tally.processed,
        super::human_size(tally.aggregate)
    )])
}

/// The objects a profile picture may be kept under, cached and not, in
/// both of its styles.
fn profile_keys(account_id: i64, attachment: &str, file_name: &str) -> Vec<String> {
    let partition = crate::media::int_to_path(account_id);
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(stem, _)| stem);
    let mut keys = Vec::new();
    for (style, name) in [
        ("original", file_name.to_owned()),
        ("static", file_name.to_owned()),
        ("static", format!("{stem}.png")),
    ] {
        let key = format!("accounts/{attachment}/{partition}/{style}/{name}");
        keys.push(format!("cache/{key}"));
        keys.push(key);
    }
    keys.dedup();
    keys
}

async fn delete_objects(state: &AppState, keys: Vec<String>) {
    for key in keys {
        if let Err(error) = state.storage.delete(&key).await {
            tracing::debug!(%error, key, "could not remove a file");
        }
    }
}

/// The profile half of `Media#remove`: remote accounts not webfingered or
/// updated within `days`, unless followed or following locally (without
/// `include_follows`), have their header, and with `prune_profiles` their
/// avatar, removed. The sizes removed are summed.
async fn prune_profiles(state: &AppState, options: &RemoveOptions) -> anyhow::Result<super::Tally> {
    let days = i32::try_from(options.days).unwrap_or(i32::MAX);
    super::parallelize_batches(
        options.concurrency,
        options.verbose,
        |after| async move {
            sqlx::query_scalar!(
                "SELECT id FROM accounts
                 WHERE domain IS NOT NULL
                   AND last_webfingered_at <= now() - make_interval(days => $1)
                   AND updated_at <= now() - make_interval(days => $1)
                   AND id > $2
                 ORDER BY id LIMIT $3",
                days,
                after,
                super::BATCH,
            )
            .fetch_all(&state.db)
            .await
        },
        |account_id| async move { prune_profile(state, options, account_id).await },
    )
    .await
}

async fn prune_profile(
    state: &AppState,
    options: &RemoveOptions,
    account_id: i64,
) -> anyhow::Result<Option<i64>> {
    let account = sqlx::query!(
        r#"SELECT avatar_file_name, avatar_file_size, header_file_name, header_file_size,
                  EXISTS (SELECT 1 FROM follows
                          WHERE account_id = $1 OR target_account_id = $1) AS "follows!"
           FROM accounts WHERE id = $1"#,
        account_id
    )
    .fetch_one(&state.db)
    .await?;
    let avatar = account.avatar_file_name.filter(|n| !n.is_empty());
    let header = account.header_file_name.filter(|n| !n.is_empty());
    if !options.include_follows && account.follows {
        return Ok(None);
    }
    if avatar.is_none() && header.is_none() {
        return Ok(None);
    }
    if options.remove_headers && header.is_none() {
        return Ok(None);
    }
    let mut size = i64::from(account.header_file_size.unwrap_or(0));
    if options.prune_profiles {
        size += i64::from(account.avatar_file_size.unwrap_or(0));
    }
    if !options.dry_run {
        let mut keys = Vec::new();
        if let Some(name) = &header {
            keys.extend(profile_keys(account_id, "headers", name));
        }
        if options.prune_profiles {
            if let Some(name) = &avatar {
                keys.extend(profile_keys(account_id, "avatars", name));
            }
        }
        sqlx::query!(
            "UPDATE accounts SET
               header_file_name = NULL, header_content_type = NULL,
               header_file_size = NULL, header_updated_at = NULL,
               avatar_file_name = CASE WHEN $2 THEN NULL ELSE avatar_file_name END,
               avatar_content_type = CASE WHEN $2 THEN NULL ELSE avatar_content_type END,
               avatar_file_size = CASE WHEN $2 THEN NULL ELSE avatar_file_size END,
               avatar_updated_at = CASE WHEN $2 THEN NULL ELSE avatar_updated_at END,
               updated_at = now()
             WHERE id = $1",
            account_id,
            options.prune_profiles,
        )
        .execute(&state.db)
        .await?;
        delete_objects(state, keys).await;
    }
    Ok(Some(size))
}

/// The attachment half of `Media#remove`:
/// `MediaAttachment.cached.remote.where(created_at: ..time_ago)`, with
/// `keep_interacted` `.without_local_interaction`, each file and thumbnail
/// removed and its size summed.
async fn remove_attachments(
    state: &AppState,
    options: &RemoveOptions,
) -> anyhow::Result<super::Tally> {
    let days = i32::try_from(options.days).unwrap_or(i32::MAX);
    let keep_interacted = options.keep_interacted;
    super::parallelize_batches(
        options.concurrency,
        options.verbose,
        |after| async move {
            sqlx::query_scalar!(
                r#"SELECT m.id FROM media_attachments m
                   WHERE m.remote_url <> '' AND m.file_file_name IS NOT NULL
                     AND m.created_at <= now() - make_interval(days => $1)
                     AND (NOT $2 OR (
                       NOT EXISTS (SELECT 1 FROM favourites f JOIN accounts a ON a.id = f.account_id
                                   WHERE a.domain IS NULL AND f.status_id = m.status_id)
                       AND NOT EXISTS (SELECT 1 FROM bookmarks b WHERE b.status_id = m.status_id)
                       AND NOT EXISTS (SELECT 1 FROM statuses s
                                       WHERE s.local AND s.deleted_at IS NULL
                                         AND s.in_reply_to_id = m.status_id)
                       AND NOT EXISTS (SELECT 1 FROM statuses s
                                       WHERE s.local AND s.deleted_at IS NULL
                                         AND s.reblog_of_id = m.status_id)
                       AND NOT EXISTS (SELECT 1 FROM quotes q JOIN statuses s ON s.id = q.status_id
                                       WHERE s.local AND s.deleted_at IS NULL
                                         AND q.quoted_status_id = m.status_id)
                       AND NOT EXISTS (SELECT 1 FROM quotes q
                                       JOIN statuses s ON s.id = q.quoted_status_id
                                       WHERE s.local AND s.deleted_at IS NULL
                                         AND q.status_id = m.status_id)))
                     AND m.id > $3
                   ORDER BY m.id LIMIT $4"#,
                days,
                keep_interacted,
                after,
                super::BATCH,
            )
            .fetch_all(&state.db)
            .await
        },
        |id| async move { remove_attachment(state, options.dry_run, id).await },
    )
    .await
}

async fn remove_attachment(
    state: &AppState,
    dry_run: bool,
    id: i64,
) -> anyhow::Result<Option<i64>> {
    let row = sqlx::query!(
        "SELECT file_file_name, file_file_size, thumbnail_file_name, thumbnail_file_size
         FROM media_attachments WHERE id = $1",
        id
    )
    .fetch_one(&state.db)
    .await?;
    let Some(file) = row.file_file_name.filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    let size = i64::from(row.file_file_size.unwrap_or(0))
        + i64::from(row.thumbnail_file_size.unwrap_or(0));
    if !dry_run {
        sqlx::query!(
            "UPDATE media_attachments SET
               file_file_name = NULL, file_content_type = NULL, file_file_size = NULL,
               file_updated_at = NULL, thumbnail_file_name = NULL,
               thumbnail_content_type = NULL, thumbnail_file_size = NULL,
               thumbnail_updated_at = NULL, updated_at = now()
             WHERE id = $1",
            id
        )
        .execute(&state.db)
        .await?;
        delete_objects(
            state,
            crate::media::attachment_keys(id, Some(&file), row.thumbnail_file_name.as_deref()),
        )
        .await;
    }
    Ok(Some(size))
}

/// `Media::PRELOADED_MODELS`, by the table a path starts with.
const MODELS: &[(&str, &str)] = &[
    ("accounts", "Account"),
    ("backups", "Backup"),
    ("custom_emojis", "CustomEmoji"),
    ("media_attachments", "MediaAttachment"),
    ("preview_cards", "PreviewCard"),
    ("site_uploads", "SiteUpload"),
];

/// `Media::VALID_PATH_SEGMENTS_SIZE`: a model, an attachment, an id
/// partitioned three digits at a time (three parts, or six for a snowflake),
/// a style and a file name.
const VALID_PATH_SEGMENTS_SIZE: [usize; 2] = [7, 10];

/// A stored file's place, read off its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPath {
    pub table: String,
    pub attachment: String,
    pub id: i64,
    pub style: String,
    pub file_name: String,
}

/// Read a key as `orphaned_file?` reads it: every `cache` part dropped, then
/// seven or ten parts, the first a model's table. `None` is an unrecognized
/// file.
#[must_use]
pub fn parse_key(key: &str) -> Option<MediaPath> {
    let segments: Vec<&str> = key.split('/').filter(|s| *s != "cache").collect();
    if !VALID_PATH_SEGMENTS_SIZE.contains(&segments.len()) {
        return None;
    }
    MODELS.iter().find(|(table, _)| *table == segments[0])?;
    let n = segments.len();
    Some(MediaPath {
        table: segments[0].to_owned(),
        attachment: segments[1].to_owned(),
        id: ruby_to_i(&segments[2..n - 2].concat()),
        style: segments[n - 2].to_owned(),
        file_name: segments[n - 1].to_owned(),
    })
}

/// Ruby's `String#to_i`: the leading digits, or 0.
fn ruby_to_i(text: &str) -> i64 {
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or(0)
}

/// The file names a record's attachments hold, by attachment (`avatars`,
/// `files`, …).
type Attachments = HashMap<&'static str, Option<String>>;

/// `preload_records_from_mixed_objects`: the records `paths` name, by table
/// and id.
async fn preload(
    state: &AppState,
    paths: &[MediaPath],
) -> sqlx::Result<HashMap<(String, i64), Attachments>> {
    let mut by_table: HashMap<&str, Vec<i64>> = HashMap::new();
    for path in paths {
        by_table.entry(&path.table).or_default().push(path.id);
    }
    let mut records = HashMap::new();
    for (table, ids) in by_table {
        let rows: Vec<(i64, Attachments)> = match table {
            "accounts" => sqlx::query!(
                "SELECT id, avatar_file_name, header_file_name FROM accounts WHERE id = ANY($1)",
                &ids
            )
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|r| {
                let attachments = [
                    ("avatars", r.avatar_file_name),
                    ("headers", r.header_file_name),
                ];
                (r.id, attachments.into_iter().collect())
            })
            .collect(),
            "backups" => sqlx::query!(
                "SELECT id, dump_file_name FROM backups WHERE id = ANY($1)",
                &ids
            )
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|r| (r.id, [("dumps", r.dump_file_name)].into_iter().collect()))
            .collect(),
            "custom_emojis" => sqlx::query!(
                "SELECT id, image_file_name FROM custom_emojis WHERE id = ANY($1)",
                &ids
            )
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|r| (r.id, [("images", r.image_file_name)].into_iter().collect()))
            .collect(),
            "media_attachments" => sqlx::query!(
                "SELECT id, file_file_name, thumbnail_file_name FROM media_attachments
                 WHERE id = ANY($1)",
                &ids
            )
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|r| {
                let attachments = [
                    ("files", r.file_file_name),
                    ("thumbnails", r.thumbnail_file_name),
                ];
                (r.id, attachments.into_iter().collect())
            })
            .collect(),
            "preview_cards" => sqlx::query!(
                "SELECT id, image_file_name FROM preview_cards WHERE id = ANY($1)",
                &ids
            )
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|r| (r.id, [("images", r.image_file_name)].into_iter().collect()))
            .collect(),
            "site_uploads" => sqlx::query!(
                "SELECT id, file_file_name FROM site_uploads WHERE id = ANY($1)",
                &ids
            )
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|r| (r.id, [("files", r.file_file_name)].into_iter().collect()))
            .collect(),
            _ => Vec::new(),
        };
        for (id, attachments) in rows {
            records.insert((table.to_owned(), id), attachments);
        }
    }
    Ok(records)
}

/// The name without its extension, as `File.basename(name, File.extname(name))`.
fn stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

/// `orphaned_file?`: whether the record `path` names is gone, or its
/// attachment is blank, or the file is not one of the attachment's.
///
/// Paperclip's `variant?` takes a file of the same name, or of the same
/// stem with the extension of one of the attachment's styles; this takes any
/// extension, so it keeps what it is unsure of. Eunha once kept an upload's
/// small style under its thumbnail's name (`files/…/small/`) and a video's
/// upload under `files/…/source/` while it waited to be transcoded; both
/// belong to an attachment that is still there.
#[must_use]
pub fn orphaned(path: &MediaPath, record: Option<&Attachments>) -> bool {
    let Some(record) = record else { return true };
    let own = |attachment: &str| {
        record
            .iter()
            .find(|(name, _)| **name == attachment)
            .and_then(|(_, file)| file.as_deref())
            .filter(|f| !f.is_empty())
    };
    let variant =
        |original: &str| original == path.file_name || stem(original) == stem(&path.file_name);
    if path.table == "media_attachments" && path.attachment == "files" {
        if path.style == "source" {
            return false;
        }
        if path.style == "small" && own("thumbnails").is_some_and(variant) {
            return false;
        }
    }
    let Some(original) = own(&path.attachment) else {
        return true;
    };
    !variant(original)
}

/// What `remove-orphans` found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OrphanReport {
    pub removed: u64,
    pub bytes: i64,
    pub unrecognized: Vec<String>,
}

/// `Media#remove_orphans` over the instance's objects: each whose record is
/// gone, or is not the file the record holds, is deleted.
pub async fn remove_orphans(
    state: &AppState,
    prefix: Option<&str>,
    start_after: Option<&str>,
    dry_run: bool,
    mut log: impl FnMut(&str),
) -> OrphanReport {
    let mut report = OrphanReport::default();
    let mut last_key = start_after.map(str::to_owned);
    loop {
        let objects = match state
            .storage
            .list(prefix.unwrap_or_default(), last_key.as_deref())
            .await
        {
            Ok(objects) => objects,
            Err(error) => {
                log(&format!("Error fetching list of files: {error}"));
                if let Some(key) = &last_key {
                    log(&format!(
                        "If you want to continue from this point, add --start-after={key} to your command"
                    ));
                }
                break;
            }
        };
        let Some((last, _)) = objects.last() else {
            break;
        };
        last_key = Some(last.clone());
        let parsed: Vec<Option<MediaPath>> = objects.iter().map(|(k, _)| parse_key(k)).collect();
        let known: Vec<MediaPath> = parsed.iter().flatten().cloned().collect();
        let records = match preload(state, &known).await {
            Ok(records) => records,
            Err(error) => {
                log(&format!("Error fetching list of files: {error}"));
                log(&format!(
                    "If you want to continue from this point, add --start-after={last} to your command"
                ));
                break;
            }
        };
        for ((key, size), path) in objects.iter().zip(parsed) {
            let Some(path) = path else {
                log(&format!("Unrecognized file found: {key}"));
                report.unrecognized.push(key.clone());
                continue;
            };
            if !orphaned(&path, records.get(&(path.table.clone(), path.id))) {
                continue;
            }
            if !dry_run {
                if let Err(error) = state.storage.delete(key).await {
                    log(&format!("Error processing {key}: {error}"));
                    continue;
                }
            }
            report.bytes += size;
            report.removed += 1;
            log(&format!("Found and removed orphan: {key}"));
        }
    }
    report
}

/// `Media#object_storage_summary`: each kind of stored file's total size,
/// and the local accounts' share where there is one.
pub async fn usage(state: &AppState) -> sqlx::Result<Vec<(String, i64, Option<i64>)>> {
    let row = sqlx::query!(
        r#"SELECT
             (SELECT COALESCE(SUM(COALESCE(file_file_size, 0) + COALESCE(thumbnail_file_size, 0)), 0)
              FROM media_attachments)::bigint AS "attachments!",
             (SELECT COALESCE(SUM(COALESCE(m.file_file_size, 0) + COALESCE(m.thumbnail_file_size, 0)), 0)
              FROM media_attachments m JOIN accounts a ON a.id = m.account_id
              WHERE a.domain IS NULL)::bigint AS "local_attachments!",
             (SELECT COALESCE(SUM(image_file_size), 0) FROM custom_emojis)::bigint AS "emoji!",
             (SELECT COALESCE(SUM(image_file_size), 0) FROM custom_emojis
              WHERE domain IS NULL)::bigint AS "local_emoji!",
             (SELECT COALESCE(SUM(avatar_file_size), 0) FROM accounts)::bigint AS "avatars!",
             (SELECT COALESCE(SUM(avatar_file_size), 0) FROM accounts
              WHERE domain IS NULL)::bigint AS "local_avatars!",
             (SELECT COALESCE(SUM(header_file_size), 0) FROM accounts)::bigint AS "headers!",
             (SELECT COALESCE(SUM(header_file_size), 0) FROM accounts
              WHERE domain IS NULL)::bigint AS "local_headers!",
             (SELECT COALESCE(SUM(image_file_size), 0) FROM preview_cards)::bigint
               AS "preview_cards!",
             (SELECT COALESCE(SUM(dump_file_size), 0) FROM backups)::bigint AS "backups!",
             (SELECT COALESCE(SUM(file_file_size), 0) FROM site_uploads)::bigint AS "settings!""#
    )
    .fetch_one(&state.db)
    .await?;
    Ok(vec![
        (
            "Attachments".to_owned(),
            row.attachments,
            Some(row.local_attachments),
        ),
        ("Custom Emoji".to_owned(), row.emoji, Some(row.local_emoji)),
        ("Avatars".to_owned(), row.avatars, Some(row.local_avatars)),
        ("Headers".to_owned(), row.headers, Some(row.local_headers)),
        ("Preview Cards".to_owned(), row.preview_cards, None),
        ("Backups".to_owned(), row.backups, None),
        ("Settings".to_owned(), row.settings, None),
    ])
}

/// Thor's `print_table` of `usage`: each column as wide as its widest
/// cell, two spaces apart.
#[must_use]
pub fn usage_table(rows: &[(String, i64, Option<i64>)]) -> String {
    let mut cells = vec![["Object".to_owned(), "Total".to_owned(), "Local".to_owned()]];
    for (label, total, local) in rows {
        cells.push([
            label.clone(),
            super::human_size(*total),
            local.map(super::human_size).unwrap_or_default(),
        ]);
    }
    let widths: Vec<usize> = (0..3)
        .map(|i| cells.iter().map(|row| row[i].len()).max().unwrap_or(0))
        .collect();
    cells
        .iter()
        .map(|row| {
            let line = format!(
                "{:w0$}  {:w1$}  {}",
                row[0],
                row[1],
                row[2],
                w0 = widths[0],
                w1 = widths[1]
            );
            format!("{}\n", line.trim_end())
        })
        .collect()
}

fn print_table(rows: &[(String, i64, Option<i64>)]) {
    print!("{}", usage_table(rows));
}

/// `Media#lookup`: the public page showing the media `url` names.
pub async fn lookup(state: &AppState, url: &str) -> anyhow::Result<String> {
    let parsed = url::Url::parse(url)
        .or_else(|_| url::Url::parse("http://localhost/")?.join(url))
        .map_err(|_| anyhow::anyhow!("Invalid URL"))?;
    let segments: Vec<&str> = std::iter::once("")
        .chain(parsed.path().trim_start_matches('/').split('/'))
        .collect();
    // Ruby's `split('/')` drops trailing empty parts.
    let end = segments
        .iter()
        .rposition(|s| !s.is_empty())
        .map_or(0, |i| i + 1);
    let segments = &segments[..end];
    let count = VALID_PATH_SEGMENTS_SIZE
        .into_iter()
        .find(|&n| {
            n <= segments.len()
                && MODELS
                    .iter()
                    .any(|(t, _)| *t == segments[segments.len() - n])
        })
        .ok_or_else(|| anyhow::anyhow!("Not a media URL"))?;
    let segments = &segments[segments.len() - count..];
    let table = segments[0];
    let id = ruby_to_i(&segments[2..count - 2].concat());
    let domain = &state.instance.domain;
    let page = match table {
        "accounts" => {
            let account = sqlx::query!(
                "SELECT id, username, domain, url FROM accounts WHERE id = $1",
                id
            )
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Cannot find corresponding record"))?;
            if account.domain.is_some() {
                web_url(account.url)
            } else if account.id == -99 {
                Some(format!("https://{domain}/about/more?instance_actor=true"))
            } else {
                Some(format!("https://{domain}/@{}", account.username))
            }
        }
        "media_attachments" => {
            let status = sqlx::query!(
                r#"SELECT s.id, s.local, s.uri, s.url, s.reblog_of_id, a.username
                   FROM media_attachments m
                   JOIN statuses s ON s.id = m.status_id AND s.deleted_at IS NULL
                   JOIN accounts a ON a.id = s.account_id
                   WHERE m.id = $1"#,
                id
            )
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Cannot find corresponding record"))?;
            let local = status.local.unwrap_or(false) || status.uri.is_none();
            if !local {
                web_url(status.url)
            } else if status.reblog_of_id.is_some() {
                Some(format!(
                    "https://{domain}/users/{}/statuses/{}/activity",
                    status.username, status.id
                ))
            } else {
                Some(format!(
                    "https://{domain}/@{}/{}",
                    status.username, status.id
                ))
            }
        }
        _ => {
            if !record_exists(state, table, id).await? {
                anyhow::bail!("Cannot find corresponding record");
            }
            None
        }
    };
    page.ok_or_else(|| anyhow::anyhow!("No public URL for this type of record"))
}

/// A remote record's `url`, when it is a web address.
fn web_url(url: Option<String>) -> Option<String> {
    url.filter(|u| url::Url::parse(u).is_ok_and(|u| matches!(u.scheme(), "http" | "https")))
}

async fn record_exists(state: &AppState, table: &str, id: i64) -> sqlx::Result<bool> {
    let query = match table {
        "backups" => "SELECT EXISTS (SELECT 1 FROM backups WHERE id = $1)",
        "custom_emojis" => "SELECT EXISTS (SELECT 1 FROM custom_emojis WHERE id = $1)",
        "preview_cards" => "SELECT EXISTS (SELECT 1 FROM preview_cards WHERE id = $1)",
        "site_uploads" => "SELECT EXISTS (SELECT 1 FROM site_uploads WHERE id = $1)",
        _ => return Ok(false),
    };
    sqlx::query_scalar(query)
        .bind(id)
        .fetch_one(&state.db)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_read_as_tootctl_reads_them() {
        let path =
            parse_key("cache/media_attachments/files/110/000/000/000/000/001/small/a.png").unwrap();
        assert_eq!(path.table, "media_attachments");
        assert_eq!(path.attachment, "files");
        assert_eq!(path.id, 110_000_000_000_000_001);
        assert_eq!(path.style, "small");
        assert_eq!(path.file_name, "a.png");
        assert_eq!(
            parse_key("custom_emojis/images/000/000/012/static/x.png")
                .unwrap()
                .id,
            12
        );
        assert!(parse_key("instance/icon/abc.png").is_none());
        assert!(parse_key("imports/data/000/000/001/original/a.csv").is_none());
        assert!(parse_key("media_attachments/files/0a1/b2c/original/a.png").is_none());
    }

    #[test]
    fn a_file_is_an_orphan_unless_its_record_holds_it() {
        let path = parse_key("accounts/avatars/000/000/001/original/me.jpg").unwrap();
        let holding = |name: Option<&str>| -> Attachments {
            [("avatars", name.map(str::to_owned)), ("headers", None)]
                .into_iter()
                .collect()
        };
        assert!(orphaned(&path, None));
        assert!(orphaned(&path, Some(&holding(None))));
        assert!(orphaned(&path, Some(&holding(Some("other.jpg")))));
        assert!(!orphaned(&path, Some(&holding(Some("me.jpg")))));
        // The static style of a GIF is a PNG of the same name.
        let still = parse_key("accounts/avatars/000/000/001/static/me.png").unwrap();
        assert!(!orphaned(&still, Some(&holding(Some("me.gif")))));
    }

    #[test]
    fn eunha_keeps_thumbnail_previews_and_pending_sources() {
        let record: Attachments = [
            ("files", None),
            ("thumbnails", Some("thumb.png".to_owned())),
        ]
        .into_iter()
        .collect();
        let preview = parse_key("media_attachments/files/000/000/001/small/thumb.png").unwrap();
        assert!(!orphaned(&preview, Some(&record)));
        let source = parse_key("media_attachments/files/000/000/001/source/source.mp4").unwrap();
        assert!(!orphaned(&source, Some(&record)));
        assert!(orphaned(&source, None));
    }

    #[test]
    fn the_usage_table_lines_up() {
        let table = usage_table(&[
            ("Attachments".to_owned(), 2048, Some(1024)),
            ("Backups".to_owned(), 0, None),
        ]);
        assert_eq!(
            table,
            "Object       Total    Local\nAttachments  2 KB     1 KB\nBackups      0 Bytes\n"
        );
    }
}
