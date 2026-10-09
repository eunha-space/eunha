//! `tootctl cache`: `Mastodon::CLI::Cache`.

use crate::state::AppState;

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Clear out the cache storage.
    ///
    /// Deletes what eunha keeps where Mastodon keeps `Rails.cache`, under the
    /// instance's Redis key prefix only.
    Clear,
    /// Update hard-cached counters.
    ///
    /// Update hard-cached counters of TYPE by counting referenced records
    /// from scratch. TYPE can be "accounts" or "statuses". It may take a very
    /// long time to finish, depending on the size of the database.
    Recount {
        #[arg(value_name = "TYPE")]
        kind: String,
        /// How many records to work on at once.
        #[arg(short = 'c', long, default_value_t = 5)]
        concurrency: usize,
        /// Print each record as it is processed.
        #[arg(short = 'v', long)]
        verbose: bool,
    },
}

impl Command {
    pub(crate) fn concurrency(&self) -> usize {
        match self {
            Self::Recount { concurrency, .. } => *concurrency,
            Self::Clear => 1,
        }
    }
}

pub async fn run(state: &AppState, command: Command) -> anyhow::Result<()> {
    match command {
        Command::Clear => {
            clear(state).await?;
            println!("OK");
        }
        Command::Recount {
            kind,
            concurrency,
            verbose,
        } => {
            let processed = match kind.as_str() {
                "accounts" => recount_accounts(state, concurrency, verbose).await?,
                "statuses" => recount_statuses(state, concurrency, verbose).await?,
                _ => anyhow::bail!("Unknown type: {kind}"),
            };
            println!();
            println!("OK, recounted {processed} records");
        }
    }
    Ok(())
}

/// The key families eunha keeps where Mastodon would use `Rails.cache`.
///
/// Mastodon's cache store namespaces every entry under `cache:`, and
/// `Rails.cache.clear` deletes that namespace. Eunha writes some of its
/// entries there under Mastodon's own names, and others, which Mastodon reads
/// from its cache by these names, without the namespace; each is something
/// worked out again, or fetched again, when it is missing.
pub const CACHE_PATTERNS: &[&str] = &[
    // The rate limits' counts, the donation campaigns, and the instance
    // activity: under Mastodon's own `cache:` keys.
    "cache:*",
    // The followers digests (`followers-digest-cache-of-its-own`).
    "followers_hash:*",
    "oembed_endpoint:*",
    "feature_approval_policy_availability:*",
    "translation_service/languages",
    "v2:translations/*",
    "active_user_count/*",
    "jsonld:context:*",
];

/// `Rails.cache.clear`, for the instance's prefix: every key in
/// [`CACHE_PATTERNS`]. How many were deleted.
pub async fn clear(state: &AppState) -> anyhow::Result<u64> {
    let mut redis = state.redis.clone();
    let mut deleted = 0;
    for pattern in CACHE_PATTERNS {
        deleted += super::delete_matching(&mut redis, &state.redis_keys, pattern).await?;
    }
    Ok(deleted)
}

/// `recount_account_stats` for each of `Account.local`: the follows it makes
/// and receives, and its statuses that are not direct, counted afresh. A
/// stat row is written only where a count changed, and created only where one
/// is not zero, as `account_stat.save if account_stat.changed?` does.
pub async fn recount_accounts(
    state: &AppState,
    concurrency: usize,
    verbose: bool,
) -> anyhow::Result<u64> {
    let tally = super::parallelize_batches(
        &super::Terminal,
        concurrency,
        verbose,
        |after| async move {
            sqlx::query_scalar!(
                "SELECT id FROM accounts WHERE domain IS NULL AND id > $1 ORDER BY id LIMIT $2",
                after,
                super::BATCH,
            )
            .fetch_all(&state.db)
            .await
        },
        |account_id| async move {
            recount_account(&state.db, account_id).await?;
            Ok(None)
        },
    )
    .await?;
    Ok(tally.processed)
}

/// `Cache#recount_account_stats` for one account.
pub async fn recount_account(db: &sqlx::PgPool, account_id: i64) -> sqlx::Result<()> {
    sqlx::query!(
        r#"WITH c AS (
             SELECT (SELECT count(*) FROM follows WHERE account_id = $1) AS following,
                    (SELECT count(*) FROM follows WHERE target_account_id = $1) AS followers,
                    (SELECT count(*) FROM statuses
                     WHERE account_id = $1 AND deleted_at IS NULL AND visibility <> 3)
                      AS statuses
           ), updated AS (
             UPDATE account_stats s
             SET following_count = c.following, followers_count = c.followers,
                 statuses_count = c.statuses, updated_at = now()
             FROM c
             WHERE s.account_id = $1
               AND (s.following_count, s.followers_count, s.statuses_count)
                   IS DISTINCT FROM (c.following, c.followers, c.statuses)
             RETURNING s.id
           )
           INSERT INTO account_stats
             (account_id, following_count, followers_count, statuses_count,
              created_at, updated_at)
           SELECT $1, c.following, c.followers, c.statuses, now(), now() FROM c
           WHERE (c.following, c.followers, c.statuses) <> (0, 0, 0)
             AND NOT EXISTS (SELECT 1 FROM account_stats WHERE account_id = $1)
           ON CONFLICT (account_id) DO NOTHING"#,
        account_id
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `recount_status_stats` for each status not discarded: its replies that
/// are not direct, boosts, favourites and accepted quotes, counted afresh,
/// written only where a count changed.
pub async fn recount_statuses(
    state: &AppState,
    concurrency: usize,
    verbose: bool,
) -> anyhow::Result<u64> {
    let tally = super::parallelize_batches(
        &super::Terminal,
        concurrency,
        verbose,
        |after| async move {
            sqlx::query_scalar!(
                "SELECT id FROM statuses WHERE deleted_at IS NULL AND id > $1
                 ORDER BY id LIMIT $2",
                after,
                super::BATCH,
            )
            .fetch_all(&state.db)
            .await
        },
        |status_id| async move {
            recount_status(&state.db, status_id).await?;
            Ok(None)
        },
    )
    .await?;
    Ok(tally.processed)
}

/// `Cache#recount_status_stats` for one status.
pub async fn recount_status(db: &sqlx::PgPool, status_id: i64) -> sqlx::Result<()> {
    sqlx::query!(
        r#"WITH c AS (
             SELECT (SELECT count(*) FROM statuses
                     WHERE in_reply_to_id = $1 AND deleted_at IS NULL AND visibility <> 3)
                      AS replies,
                    (SELECT count(*) FROM statuses
                     WHERE reblog_of_id = $1 AND deleted_at IS NULL) AS reblogs,
                    (SELECT count(*) FROM favourites WHERE status_id = $1) AS favourites,
                    (SELECT count(*) FROM quotes WHERE quoted_status_id = $1 AND state = 1)
                      AS quotes
           ), updated AS (
             UPDATE status_stats s
             SET replies_count = c.replies, reblogs_count = c.reblogs,
                 favourites_count = c.favourites, quotes_count = c.quotes,
                 updated_at = now()
             FROM c
             WHERE s.status_id = $1
               AND (s.replies_count, s.reblogs_count, s.favourites_count, s.quotes_count)
                   IS DISTINCT FROM (c.replies, c.reblogs, c.favourites, c.quotes)
             RETURNING s.id
           )
           INSERT INTO status_stats
             (status_id, replies_count, reblogs_count, favourites_count, quotes_count,
              created_at, updated_at)
           SELECT $1, c.replies, c.reblogs, c.favourites, c.quotes, now(), now() FROM c
           WHERE (c.replies, c.reblogs, c.favourites, c.quotes) <> (0, 0, 0, 0)
             AND NOT EXISTS (SELECT 1 FROM status_stats WHERE status_id = $1)
           ON CONFLICT (status_id) DO NOTHING"#,
        status_id
    )
    .execute(db)
    .await?;
    Ok(())
}
