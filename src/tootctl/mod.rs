//! The `tootctl` maintenance commands that look after what an instance keeps:
//! `feeds`, `cache`, `statuses remove`, `media` and `preview_cards remove`,
//! and `emoji` and `domains`. `accounts` is part of `eunha accounts`, and
//! `maintenance` needs only a database; both are wired in the binary.
//!
//! Each is a group of subcommands in the shape `tootctl` gives it, with its
//! options, its output and its semantics (`lib/mastodon/cli/*.rb`). The
//! binary flattens [`Command`] into its own, and runs it against the instance
//! `--instance` picks. See *docs/operating/maintenance.md*.

pub mod accounts;
pub mod cache;
pub mod console;
pub mod domains;
pub mod emoji;
pub mod feeds;
pub mod maintenance;
pub mod media;
pub mod preview_cards;
pub mod statuses;

pub use console::{Console, Recorder, Terminal};

use std::future::Future;

use crate::state::AppState;

/// The groups, as `eunha <group> <command>`.
#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Build and clear home and list feeds, as `tootctl feeds` does.
    Feeds {
        #[command(subcommand)]
        command: feeds::Command,
        /// With `--tenants`, the instance, by its domain or one of its
        /// aliases.
        #[arg(long, value_name = "HOST", global = true)]
        instance: Option<String>,
    },
    /// Recount counters and clear the cache, as `tootctl cache` does.
    Cache {
        #[command(subcommand)]
        command: cache::Command,
        /// With `--tenants`, the instance, by its domain or one of its
        /// aliases.
        #[arg(long, value_name = "HOST", global = true)]
        instance: Option<String>,
    },
    /// Remove unreferenced statuses, as `tootctl statuses` does.
    Statuses {
        #[command(subcommand)]
        command: statuses::Command,
        /// With `--tenants`, the instance, by its domain or one of its
        /// aliases.
        #[arg(long, value_name = "HOST", global = true)]
        instance: Option<String>,
    },
    /// Remove, measure and look up stored media, as `tootctl media` does.
    Media {
        #[command(subcommand)]
        command: media::Command,
        /// With `--tenants`, the instance, by its domain or one of its
        /// aliases.
        #[arg(long, value_name = "HOST", global = true)]
        instance: Option<String>,
    },
    /// Import, export and purge custom emoji, as `tootctl emoji` does.
    Emoji {
        #[command(subcommand)]
        command: emoji::Command,
        /// With `--tenants`, the instance, by its domain or one of its
        /// aliases.
        #[arg(long, value_name = "HOST", global = true)]
        instance: Option<String>,
    },
    /// Purge and crawl other servers, as `tootctl domains` does.
    Domains {
        #[command(subcommand)]
        command: domains::Command,
        /// With `--tenants`, the instance, by its domain or one of its
        /// aliases.
        #[arg(long, value_name = "HOST", global = true)]
        instance: Option<String>,
    },
    /// Remove preview card images, as `tootctl preview_cards` does.
    #[command(name = "preview_cards", alias = "preview-cards")]
    PreviewCards {
        #[command(subcommand)]
        command: preview_cards::Command,
        /// With `--tenants`, the instance, by its domain or one of its
        /// aliases.
        #[arg(long, value_name = "HOST", global = true)]
        instance: Option<String>,
    },
}

impl Command {
    /// The tenant `--instance` names, if any.
    #[must_use]
    pub fn instance(&self) -> Option<&str> {
        match self {
            Self::Feeds { instance, .. }
            | Self::Cache { instance, .. }
            | Self::Statuses { instance, .. }
            | Self::Media { instance, .. }
            | Self::Emoji { instance, .. }
            | Self::Domains { instance, .. }
            | Self::PreviewCards { instance, .. } => instance.as_deref(),
        }
    }

    /// How many database connections the command can use at once:
    /// `reset_connection_pools!`'s `concurrency + 1`.
    #[must_use]
    pub fn connections(&self) -> u32 {
        let concurrency = match self {
            Self::Feeds { command, .. } => command.concurrency(),
            Self::Cache { command, .. } => command.concurrency(),
            Self::Media { command, .. } => command.concurrency(),
            Self::PreviewCards { command, .. } => command.concurrency(),
            Self::Domains { command, .. } => command.concurrency(),
            Self::Statuses { .. } | Self::Emoji { .. } => 1,
        };
        u32::try_from(concurrency)
            .unwrap_or(u32::MAX)
            .saturating_add(1)
    }

    /// Run the command, printing what `tootctl` prints.
    pub async fn run(self, state: &AppState) -> anyhow::Result<()> {
        match self {
            Self::Feeds { command, .. } => feeds::run(state, command).await,
            Self::Cache { command, .. } => cache::run(state, command).await,
            Self::Statuses { command, .. } => statuses::run(state, command).await,
            Self::Media { command, .. } => media::run(state, command).await,
            Self::PreviewCards { command, .. } => preview_cards::run(state, command).await,
            Self::Emoji { command, .. } => command.run(state, &Terminal).await,
            Self::Domains { command, .. } => command.run(state, &Terminal).await,
        }
    }
}

/// How many rows one batch reads: `find_in_batches`'s default.
pub(crate) const BATCH: i64 = 1000;

/// What `parallelize_with_progress` hands back: how many items it visited,
/// and the sum of the integers their blocks returned.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Tally {
    pub processed: u64,
    pub aggregate: i64,
}

impl Tally {
    fn add(&mut self, other: Self) {
        self.processed += other.processed;
        self.aggregate += other.aggregate;
    }
}

/// [`console::parallelize`] over every id `batch` returns, read a batch at a
/// time by the last id of the one before, as `find_in_batches` reads them.
pub(crate) async fn parallelize_batches<B, BFut, F, Fut>(
    console: &dyn Console,
    concurrency: usize,
    verbose: bool,
    mut batch: B,
    work: F,
) -> anyhow::Result<Tally>
where
    B: FnMut(i64) -> BFut,
    BFut: Future<Output = sqlx::Result<Vec<i64>>>,
    F: Fn(i64) -> Fut,
    Fut: Future<Output = anyhow::Result<Option<i64>>>,
{
    check_concurrency(concurrency)?;
    let mut tally = Tally::default();
    let mut after = i64::MIN;
    loop {
        let ids = batch(after).await?;
        let Some(&last) = ids.last() else {
            return Ok(tally);
        };
        after = last;
        tally.add(console::parallelize(console, ids, concurrency, verbose, &work).await?);
    }
}

/// `parallelize_with_progress`'s refusal.
pub(crate) fn check_concurrency(concurrency: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        concurrency >= 1,
        "Cannot run with this concurrency setting, must be at least 1"
    );
    Ok(())
}

/// Delete every key in `redis` matching `pattern` under the instance's
/// prefix, found with `SCAN`, as `redis.del(redis.keys(pattern))` does; how
/// many went.
pub(crate) async fn delete_matching(
    redis: &mut redis::aio::ConnectionManager,
    keys: &crate::redis_keys::RedisKeyspace,
    pattern: &str,
) -> anyhow::Result<u64> {
    let matched = scan(redis, keys, pattern).await?;
    let mut deleted = 0u64;
    for chunk in matched.chunks(BATCH as usize) {
        let n: u64 = redis::cmd("DEL").arg(chunk).query_async(redis).await?;
        deleted += n;
    }
    Ok(deleted)
}

/// The keys under the instance's prefix matching `pattern`, prefix and all.
pub(crate) async fn scan(
    redis: &mut redis::aio::ConnectionManager,
    keys: &crate::redis_keys::RedisKeyspace,
    pattern: &str,
) -> anyhow::Result<Vec<String>> {
    let pattern = keys.key(pattern);
    let mut cursor: u64 = 0;
    let mut found = Vec::new();
    loop {
        let (next, mut batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(BATCH)
            .query_async(redis)
            .await?;
        found.append(&mut batch);
        if next == 0 {
            found.sort();
            found.dedup();
            return Ok(found);
        }
        cursor = next;
    }
}

/// `Base#dry_run_mode_suffix`.
pub(crate) fn dry_run_suffix(dry_run: bool) -> &'static str {
    if dry_run {
        " (DRY RUN)"
    } else {
        ""
    }
}

/// `Mastodon::Snowflake.id_at(days.days.ago, with_random: false)`.
pub(crate) fn id_days_ago(days: i64) -> i64 {
    (chrono::Utc::now() - chrono::Duration::days(days)).timestamp_millis() << 16
}

/// `ActionView::Helpers::NumberHelper#number_to_human_size` with its
/// defaults: binary multiples, three significant digits, trailing zeros
/// dropped.
#[must_use]
pub fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 6] = ["KB", "MB", "GB", "TB", "PB", "EB"];
    if bytes.unsigned_abs() < 1024 {
        return if bytes == 1 {
            "1 Byte".to_owned()
        } else {
            format!("{bytes} Bytes")
        };
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    value /= 1024.0;
    while value.abs() >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    // Three significant digits, as `number_to_rounded(precision: 3,
    // significant: true)`; a value that rounds up to 1024 stays in its unit,
    // as Rails leaves it.
    let digits = (value.abs().log10().floor() as i32) + 1;
    let decimals = (3 - digits).max(0) as usize;
    let text = format!("{value:.decimals$}");
    let text = if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        text
    };
    format!("{text} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn sizes_read_as_rails_prints_them() {
        assert_eq!(human_size(0), "0 Bytes");
        assert_eq!(human_size(1), "1 Byte");
        assert_eq!(human_size(1023), "1023 Bytes");
        assert_eq!(human_size(1024), "1 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1_234_567), "1.18 MB");
        assert_eq!(human_size(12_345_678_901), "11.5 GB");
        assert_eq!(human_size(1024 * 1024 * 1024 * 1024), "1 TB");
    }
}
