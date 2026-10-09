//! `tootctl feeds`: `Mastodon::CLI::Feeds`.

use crate::state::AppState;

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Build home and list feeds for one or all users.
    ///
    /// Build home and list feeds that are stored in Redis from the database.
    /// With `--skip-filled-timelines`, timelines which contain more than half
    /// the maximum number of posts are skipped. With `--all`, or no
    /// USERNAME, all active users are processed.
    Build {
        username: Option<String>,
        /// Every active user.
        #[arg(long)]
        all: bool,
        /// How many records to work on at once.
        #[arg(short = 'c', long, default_value_t = 5)]
        concurrency: usize,
        /// Print each record as it is processed.
        #[arg(short = 'v', long)]
        verbose: bool,
        /// Report what would be done, changing nothing.
        #[arg(long)]
        dry_run: bool,
        /// Leave feeds more than half full alone.
        #[arg(long)]
        skip_filled_timelines: bool,
    },
    /// Remove all home and list feeds from Redis.
    Clear,
    /// Remove home and list feeds of inactive users from Redis.
    ///
    /// Not needed in most cases, as the instance cleans up the feeds of
    /// inactive accounts every day; this goes over every feed Redis holds, to
    /// catch those missed because of bugs or database mishaps.
    Vacuum,
}

impl Command {
    pub(crate) fn concurrency(&self) -> usize {
        match self {
            Self::Build { concurrency, .. } => *concurrency,
            Self::Clear | Self::Vacuum => 1,
        }
    }
}

pub async fn run(state: &AppState, command: Command) -> anyhow::Result<()> {
    match command {
        Command::Build {
            username,
            all,
            concurrency,
            verbose,
            dry_run,
            skip_filled_timelines,
        } => {
            let suffix = super::dry_run_suffix(dry_run);
            match username.filter(|_| !all) {
                None => {
                    let processed =
                        build_all(state, concurrency, verbose, dry_run, skip_filled_timelines)
                            .await?;
                    println!("Regenerated feeds for {processed} accounts {suffix}");
                }
                Some(username) => {
                    let account_id = find_local(state, &username)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("No such account"))?;
                    if !dry_run {
                        crate::home_feed::precompute(state, account_id, skip_filled_timelines)
                            .await;
                    }
                    println!("OK {suffix}");
                }
            }
        }
        Command::Clear => {
            clear(state).await?;
            println!("OK");
        }
        Command::Vacuum => {
            println!("Deleting orphaned home feeds…");
            vacuum_home(state).await?;
            println!("Deleting orphaned list feeds…");
            vacuum_lists(state).await?;
        }
    }
    Ok(())
}

/// `Account.find_local(username)`.
pub async fn find_local(state: &AppState, username: &str) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE lower(username) = lower($1) AND domain IS NULL
         ORDER BY id LIMIT 1",
        username
    )
    .fetch_optional(&state.db)
    .await
}

/// `PrecomputeFeedService` for each of `Account.joins(:user).merge(User.active)`:
/// the confirmed users who signed in within `User::ACTIVE_DURATION` and whose
/// accounts are neither suspended nor waiting to be deleted. How many there
/// were.
pub async fn build_all(
    state: &AppState,
    concurrency: usize,
    verbose: bool,
    dry_run: bool,
    skip_filled_timelines: bool,
) -> anyhow::Result<u64> {
    let tally = super::parallelize_batches(
        concurrency,
        verbose,
        |after| async move {
            sqlx::query_scalar!(
                r#"SELECT a.id FROM accounts a JOIN users u ON u.account_id = a.id
                   WHERE u.confirmed_at IS NOT NULL
                     AND u.current_sign_in_at >= now() - make_interval(days => $1)
                     AND a.suspended_at IS NULL
                     AND NOT EXISTS (SELECT 1 FROM account_deletion_requests r
                                     WHERE r.account_id = a.id)
                     AND a.id > $2
                   ORDER BY a.id LIMIT $3"#,
                crate::home_feed::ACTIVE_DAYS,
                after,
                super::BATCH,
            )
            .fetch_all(&state.db)
            .await
        },
        |account_id| async move {
            if !dry_run {
                crate::home_feed::precompute(state, account_id, skip_filled_timelines).await;
            }
            Ok(None)
        },
    )
    .await?;
    Ok(tally.processed)
}

/// `redis.del(redis.keys('feed:*'))`: every home and list feed, and what
/// tracks their boosts.
pub async fn clear(state: &AppState) -> anyhow::Result<u64> {
    let mut redis = state.redis.clone();
    super::delete_matching(&mut redis, &state.redis_keys, "feed:*").await
}

/// The id a feed key carries, `feed:<type>:<id>…`'s third part, read as
/// Ruby's `to_i` reads it: the leading digits, and 0 for none.
fn feed_id(state: &AppState, key: &str) -> i64 {
    let key = state.redis_keys.strip(key).unwrap_or(key);
    let part = key.split(':').nth(2).unwrap_or_default();
    let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or(0)
}

/// The `feeds vacuum` home pass: each `feed:home:*` key whose account is not
/// a confirmed user's who signed in recently is deleted.
pub async fn vacuum_home(state: &AppState) -> anyhow::Result<u64> {
    let mut redis = state.redis.clone();
    let keys = super::scan(&mut redis, &state.redis_keys, "feed:home:*").await?;
    let mut deleted = 0;
    for slice in keys.chunks(super::BATCH as usize) {
        let ids: Vec<i64> = slice.iter().map(|k| feed_id(state, k)).collect();
        let known: Vec<i64> = sqlx::query_scalar!(
            r#"SELECT account_id AS "account_id!" FROM users
               WHERE confirmed_at IS NOT NULL
                 AND current_sign_in_at >= now() - make_interval(days => $1)
                 AND account_id = ANY($2)"#,
            crate::home_feed::ACTIVE_DAYS,
            &ids,
        )
        .fetch_all(&state.db)
        .await?;
        deleted += delete_unknown(&mut redis, slice, &ids, &known).await?;
    }
    Ok(deleted)
}

/// The `feeds vacuum` list pass: each `feed:list:*` key whose list is not
/// owned by a confirmed user who signed in recently is deleted.
pub async fn vacuum_lists(state: &AppState) -> anyhow::Result<u64> {
    let mut redis = state.redis.clone();
    let keys = super::scan(&mut redis, &state.redis_keys, "feed:list:*").await?;
    let mut deleted = 0;
    for slice in keys.chunks(super::BATCH as usize) {
        let ids: Vec<i64> = slice.iter().map(|k| feed_id(state, k)).collect();
        let known: Vec<i64> = sqlx::query_scalar!(
            r#"SELECT l.id FROM lists l JOIN users u ON u.account_id = l.account_id
               WHERE u.confirmed_at IS NOT NULL
                 AND u.current_sign_in_at >= now() - make_interval(days => $1)
                 AND l.id = ANY($2)"#,
            crate::home_feed::ACTIVE_DAYS,
            &ids,
        )
        .fetch_all(&state.db)
        .await?;
        deleted += delete_unknown(&mut redis, slice, &ids, &known).await?;
    }
    Ok(deleted)
}

async fn delete_unknown(
    redis: &mut redis::aio::ConnectionManager,
    keys: &[String],
    ids: &[i64],
    known: &[i64],
) -> anyhow::Result<u64> {
    let doomed: Vec<&String> = keys
        .iter()
        .zip(ids)
        .filter(|(_, id)| !known.contains(id))
        .map(|(key, _)| key)
        .collect();
    if doomed.is_empty() {
        return Ok(0);
    }
    Ok(redis::cmd("DEL").arg(doomed).query_async(redis).await?)
}
