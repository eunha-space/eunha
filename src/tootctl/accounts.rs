//! The rest of `tootctl accounts` (`Mastodon::CLI::Accounts`): `create`,
//! `modify`, and eunha's own `update`, `move` and `batch-status` are wired in
//! `main.rs`; these are the others.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Context as _};
use serde_json::Value;
use sqlx::PgPool;

use super::console::{connections_for, dry_run_suffix, parallelize, Console, Instance};
use crate::db::models::Account;
use crate::state::AppState;

/// How long `cull` leaves an account alone after it was last seen to exist.
const CULL_SKIP_THRESHOLD: chrono::TimeDelta = chrono::TimeDelta::days(7);

/// How many accounts `rotate --all` hands out at once, and how long it waits
/// between each such batch's broadcasts: `find_in_batches` and
/// `delay += 5.minutes`.
const ROTATE_BATCH: usize = 1000;
const ROTATE_DELAY: Duration = Duration::from_secs(5 * 60);

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Generate and broadcast new keys, as `tootctl accounts rotate` does.
    ///
    /// Each account is given a new RSA key, and its profile goes out as an
    /// `Update` signed with the old one, which the servers that know it still
    /// hold. With `--all`, every local account that is not suspended, a
    /// thousand at a time, five minutes apart.
    Rotate {
        username: Option<String>,
        /// Every local account that is not suspended.
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        on: Instance,
    },
    /// Delete a user, as `tootctl accounts delete` does.
    ///
    /// Its posts and everything else go, and the user with its address. The
    /// username stays taken.
    Delete {
        username: Option<String>,
        /// Pick the user by its e-mail address instead.
        #[arg(long)]
        email: Option<String>,
        /// Say what would be deleted, deleting nothing.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        on: Instance,
    },
    /// Merge two remote accounts into one, as `tootctl accounts merge` does.
    ///
    /// FROM and TO are `username@domain`. What belongs to FROM is given to
    /// TO, and FROM is removed: for duplicates left behind by a server that
    /// changed its domain. Only when both have the same public key, unless
    /// `--force`.
    Merge {
        from: String,
        to: String,
        /// Merge accounts whose public keys differ.
        #[arg(short, long)]
        force: bool,
        #[command(flatten)]
        on: Instance,
    },
    /// Find duplicate remote accounts and merge them: deprecated.
    ///
    /// Mastodon 4.7.0's schema keeps an actor's id unique, so this only says
    /// so.
    FixDuplicates {
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        on: Instance,
    },
    /// Request a backup for a user, as `tootctl accounts backup` does.
    ///
    /// The running server builds it and mails its owner the link.
    Backup {
        username: String,
        #[command(flatten)]
        on: Instance,
    },
    /// Remove remote accounts that no longer exist, as `tootctl accounts
    /// cull` does.
    ///
    /// Asks every remote account's server for it, and removes those it
    /// answers with 404 or 410 for. Accounts seen within the last week are
    /// left alone, as is every account of a server that could not be reached.
    Cull {
        /// Only accounts on these domains.
        domains: Vec<String>,
        #[arg(short = 'c', long, default_value_t = 5)]
        concurrency: usize,
        /// Say what would be removed, removing nothing.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        on: Instance,
    },
    /// Fetch remote accounts again from their servers, as `tootctl accounts
    /// refresh` does.
    ///
    /// USERNAMES are `username@domain`. With `--all`, every remote account;
    /// with `--domain`, those on one domain.
    Refresh {
        usernames: Vec<String>,
        /// Every remote account.
        #[arg(long)]
        all: bool,
        /// Every remote account on this domain.
        #[arg(long)]
        domain: Option<String>,
        #[arg(short = 'c', long, default_value_t = 5)]
        concurrency: usize,
        /// Say which account is being processed.
        #[arg(short, long)]
        verbose: bool,
        /// Fetch nothing.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        on: Instance,
    },
    /// Make every local account follow the local account USERNAME, as
    /// `tootctl accounts follow` does.
    Follow {
        username: String,
        #[arg(short = 'c', long, default_value_t = 5)]
        concurrency: usize,
        #[arg(short, long)]
        verbose: bool,
        #[command(flatten)]
        on: Instance,
    },
    /// Make every local account unfollow ACCT, as `tootctl accounts
    /// unfollow` does.
    Unfollow {
        acct: String,
        #[arg(short = 'c', long, default_value_t = 5)]
        concurrency: usize,
        #[arg(short, long)]
        verbose: bool,
        #[command(flatten)]
        on: Instance,
    },
    /// Reset all follows and/or followers for a user, as `tootctl accounts
    /// reset-relationships` does.
    ///
    /// `--follows` unfollows everyone the account follows, then follows what
    /// a new account would; `--followers` removes every follower.
    ResetRelationships {
        username: String,
        #[arg(long)]
        follows: bool,
        #[arg(long)]
        followers: bool,
        #[command(flatten)]
        on: Instance,
    },
    /// Approve pending accounts, as `tootctl accounts approve` does.
    ///
    /// All of them, the oldest NUMBER, or USERNAME's.
    Approve {
        username: Option<String>,
        /// The oldest this many.
        #[arg(short = 'n', long, allow_negative_numbers = true)]
        number: Option<i64>,
        /// Every pending account.
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        on: Instance,
    },
    /// Prune remote accounts that never interacted with local users, as
    /// `tootctl accounts prune` does.
    ///
    /// That is, with no posts, follows, follow requests, mentions,
    /// favourites, blocks, mutes or reports. Bots, groups, and suspended or
    /// silenced accounts are kept.
    Prune {
        #[arg(short = 'c', long, default_value_t = 5)]
        concurrency: usize,
        /// Say how many would be pruned, pruning nothing.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        on: Instance,
    },
}

impl Command {
    /// The instance the command acts on, with `--tenants`.
    pub fn instance(&self) -> Option<&str> {
        match self {
            Self::Rotate { on, .. }
            | Self::Delete { on, .. }
            | Self::Merge { on, .. }
            | Self::FixDuplicates { on, .. }
            | Self::Backup { on, .. }
            | Self::Cull { on, .. }
            | Self::Refresh { on, .. }
            | Self::Follow { on, .. }
            | Self::Unfollow { on, .. }
            | Self::ResetRelationships { on, .. }
            | Self::Approve { on, .. }
            | Self::Prune { on, .. } => on.host.as_deref(),
        }
    }

    /// The database connections the command needs.
    pub fn connections(&self) -> u32 {
        match self {
            Self::Cull { concurrency, .. }
            | Self::Refresh { concurrency, .. }
            | Self::Follow { concurrency, .. }
            | Self::Unfollow { concurrency, .. }
            | Self::Prune { concurrency, .. } => connections_for(*concurrency),
            _ => 4,
        }
    }

    pub async fn run(self, state: &AppState, console: &dyn Console) -> anyhow::Result<()> {
        match self {
            Self::Rotate { username, all, .. } => rotate(state, console, username, all).await,
            Self::Delete {
                username,
                email,
                dry_run,
                ..
            } => delete(state, console, username, email, dry_run).await,
            Self::Merge {
                from, to, force, ..
            } => merge(state, console, &from, &to, force).await,
            Self::FixDuplicates { .. } => {
                fix_duplicates(console);
                Ok(())
            }
            Self::Backup { username, .. } => backup(state, console, &username).await,
            Self::Cull {
                domains,
                concurrency,
                dry_run,
                ..
            } => cull(state, console, &domains, concurrency, dry_run).await,
            Self::Refresh {
                usernames,
                all,
                domain,
                concurrency,
                verbose,
                dry_run,
                ..
            } => {
                refresh(
                    state,
                    console,
                    &usernames,
                    Scope { all, domain },
                    concurrency,
                    verbose,
                    dry_run,
                )
                .await
            }
            Self::Follow {
                username,
                concurrency,
                verbose,
                ..
            } => follow(state, console, &username, concurrency, verbose).await,
            Self::Unfollow {
                acct,
                concurrency,
                verbose,
                ..
            } => unfollow(state, console, &acct, concurrency, verbose).await,
            Self::ResetRelationships {
                username,
                follows,
                followers,
                ..
            } => reset_relationships(state, console, &username, follows, followers).await,
            Self::Approve {
                username,
                number,
                all,
                ..
            } => approve(state, console, username, number, all).await,
            Self::Prune {
                concurrency,
                dry_run,
                ..
            } => prune(state, console, concurrency, dry_run).await,
        }
    }
}

/// `Account.find_local(username)`.
pub async fn find_local(state: &AppState, username: &str) -> anyhow::Result<Option<Account>> {
    Ok(crate::search::accounts::find_remote(state, username, None).await?)
}

/// `Account.find_remote(*acct.split('@'))`: an acct without a domain names a
/// local account.
async fn find_by_acct(state: &AppState, acct: &str) -> anyhow::Result<Option<Account>> {
    let mut parts = acct.split('@');
    let username = parts.next().unwrap_or_default();
    let domain = parts.next().filter(|domain| !domain.is_empty());
    Ok(crate::search::accounts::find_remote(state, username, domain).await?)
}

async fn load(db: &PgPool, id: i64) -> anyhow::Result<Option<Account>> {
    Ok(sqlx::query_as("SELECT * FROM accounts WHERE id = $1")
        .bind(id)
        .fetch_optional(db)
        .await?)
}

// ── rotate ────────────────────────────────────────────────────────────────

/// `Accounts#rotate`.
pub async fn rotate(
    state: &AppState,
    console: &dyn Console,
    username: Option<String>,
    all: bool,
) -> anyhow::Result<()> {
    if all {
        // `Account.local.without_suspended`, but for the instance actor, whose
        // key a running server keeps for its life and whose actor is not a
        // profile to broadcast.
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM accounts
             WHERE domain IS NULL AND id > 0 AND suspended_at IS NULL
             ORDER BY id",
        )
        .fetch_all(&state.db)
        .await?;
        let mut rotated = 0;
        let mut delay = Duration::ZERO;
        for batch in ids.chunks(ROTATE_BATCH) {
            for &id in batch {
                match rotate_keys_for_account(state, id, delay).await {
                    Ok(()) => {}
                    Err(error) => console.say(&format!("Error processing {id}: {error:#}")),
                }
                rotated += 1;
            }
            delay += ROTATE_DELAY;
        }
        console.say(&format!("OK, rotated keys for {rotated} accounts"));
        return Ok(());
    }
    let Some(username) = username.filter(|name| !name.is_empty()) else {
        bail!("No account(s) given");
    };
    let account = find_local(state, &username)
        .await?
        .filter(|account| account.id > 0)
        .context("No such account")?;
    rotate_keys_for_account(state, account.id, Duration::ZERO).await?;
    console.say("OK");
    Ok(())
}

/// `rotate_keys_for_account`: a new RSA key, and the profile's `Update`,
/// signed with the old one, distributed after `delay`.
async fn rotate_keys_for_account(
    state: &AppState,
    account_id: i64,
    delay: Duration,
) -> anyhow::Result<()> {
    let old = crate::federation::keypair::signing_key(state, account_id)
        .await
        .ok();
    let (private_key, public_key) = crate::tenants::spawn_blocking(|| {
        ojak::sig::signature::generate_rsa_keypair(&mut rsa::rand_core::OsRng)
    })
    .await?
    .map_err(|e| anyhow::anyhow!("generating a key: {e}"))?;
    crate::federation::keypair::replace_local(state, account_id, &private_key, &public_key).await?;

    let account = load(&state.db, account_id)
        .await?
        .context("the account is gone")?;
    let domain = &state.instance.domain;
    let actor_url = crate::federation::tag::account_uri_of(domain, &account);
    let actor = crate::api::ap::objects::actor_json(state, domain, &account)
        .await
        .map_err(|e| anyhow::anyhow!("building the actor: {e:?}"))?;
    let update_id = format!(
        "{actor_url}#updates/{}",
        chrono::Utc::now().timestamp_millis()
    );
    let mut activity = crate::federation::activity::update_actor(&update_id, &actor_url, actor)?;
    // `serialize_payload(…, signer: @account, sign_with:)`, which signs when
    // `Payloadable#signing_enabled?`, with the key the others still hold.
    if !crate::settings::authorized_fetch_mode(state).await {
        let key_id: String = crate::api::ap::serving::AccountUris::of(&state.uris, &account)
            .key_id()?
            .into();
        let signer = old.map_or(private_key, |old| old.private_key);
        activity = crate::federation::delivery::sign_linked_data_with(activity, &key_id, &signer);
    }
    crate::jobs::perform_in(
        state,
        delay,
        UpdateDistributionWorker {
            account_id,
            activity,
        },
    )
    .await?;
    Ok(())
}

/// `ActivityPub::UpdateDistributionWorker` with `sign_with`, as a key
/// rotation queues it: the profile's `Update`, already signed with the key
/// the account had, to the inboxes of every server that knows the account.
/// The deliveries are signed with the key it has now.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct UpdateDistributionWorker {
    pub account_id: i64,
    pub activity: Value,
}

impl crate::jobs::Job for UpdateDistributionWorker {
    const KIND: &'static str = "ActivityPub::UpdateDistributionWorker";
    const OPTIONS: crate::jobs::Options =
        crate::jobs::Options::DEFAULT.queue(crate::jobs::Queue::Push);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        let Some(account) = load(&state.db, self.account_id).await? else {
            return Ok(());
        };
        let inboxes = crate::federation::delivery::account_reach_inboxes(state, account.id).await?;
        let key_id = crate::api::ap::serving::AccountUris::of(&state.uris, &account)
            .key_id()?
            .into();
        crate::federation::delivery::deliver_to_inboxes_signed(
            state,
            self.activity,
            inboxes,
            key_id,
            crate::federation::delivery::LinkedData::Unsigned,
        )
        .await?;
        Ok(())
    }
}

// ── delete ────────────────────────────────────────────────────────────────

/// `Accounts#delete`.
pub async fn delete(
    state: &AppState,
    console: &dyn Console,
    username: Option<String>,
    email: Option<String>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let username = username.filter(|name| !name.trim().is_empty());
    let email = email.filter(|email| !email.trim().is_empty());
    let account = match (username, email) {
        (Some(_), Some(_)) => bail!("Use username or --email, not both"),
        (None, None) => bail!("No username provided"),
        (Some(username), None) => find_local(state, &username)
            .await?
            .context("No user with such username")?,
        (None, Some(email)) => sqlx::query_as(
            "SELECT a.* FROM accounts a JOIN users u ON u.account_id = a.id
             WHERE u.email = $1 LIMIT 1",
        )
        .bind(email)
        .fetch_optional(&state.db)
        .await?
        .context("No user with such email")?,
    };
    let statuses: i64 =
        sqlx::query_scalar("SELECT statuses_count FROM account_stats WHERE account_id = $1")
            .bind(account.id)
            .fetch_optional(&state.db)
            .await?
            .unwrap_or(0);
    let suffix = dry_run_suffix(dry_run);
    console.say(&format!(
        "Deleting user with {statuses} statuses, this might take a while...{suffix}"
    ));
    if !dry_run {
        crate::delete_account::call(
            state,
            account.id,
            crate::delete_account::Options {
                reserve_email: false,
                ..Default::default()
            },
        )
        .await?;
    }
    console.say(&format!("OK{suffix}"));
    Ok(())
}

// ── merge and fix-duplicates ──────────────────────────────────────────────

/// `Accounts#merge`: `to_account.merge_with!(from_account)` and
/// `from_account.destroy`.
pub async fn merge(
    state: &AppState,
    console: &dyn Console,
    from_acct: &str,
    to_acct: &str,
    force: bool,
) -> anyhow::Result<()> {
    let from = find_by_acct(state, from_acct)
        .await?
        .filter(|account| !account.is_local())
        .with_context(|| format!("No such account ({from_acct})"))?;
    let to = find_by_acct(state, to_acct)
        .await?
        .filter(|account| !account.is_local())
        .with_context(|| format!("No such account ({to_acct})"))?;
    let from_key = crate::federation::keypair::rsa_public_key(&state.db, from.id).await?;
    let to_key = crate::federation::keypair::rsa_public_key(&state.db, to.id).await?;
    if from_key != to_key && !force {
        bail!(
            "Accounts don't have the same public key, might not be duplicates!\n\
             Override with --force"
        );
    }
    crate::federation::process_account::merge_with(state, to.id, from.id).await?;
    crate::search::elasticsearch::indexing::account(state, from.id).await;
    console.say("OK");
    Ok(())
}

/// `Accounts#fix_duplicates`, which Mastodon 4.7.0 made a notice.
pub fn fix_duplicates(console: &dyn Console) {
    console.say(
        "This command is deprecated as Mastodon v4.7.0 migrations enforce ActivityPub actor \
         identifier uniqueness",
    );
}

// ── backup ────────────────────────────────────────────────────────────────

/// `Accounts#backup`.
pub async fn backup(state: &AppState, console: &dyn Console, username: &str) -> anyhow::Result<()> {
    let account = find_local(state, username)
        .await?
        .context("No user with such username")?;
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE account_id = $1")
        .bind(account.id)
        .fetch_optional(&state.db)
        .await?
        .context("No user with such username")?;
    crate::portability::backup::request(state, user_id).await?;
    console.say("OK");
    Ok(())
}

// ── cull ──────────────────────────────────────────────────────────────────

/// `Accounts#cull`.
pub async fn cull(
    state: &AppState,
    console: &dyn Console,
    domains: &[String],
    concurrency: usize,
    dry_run: bool,
) -> anyhow::Result<()> {
    let threshold = chrono::Utc::now().naive_utc() - CULL_SKIP_THRESHOLD;
    // `Account.remote.activitypub`.
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM accounts
         WHERE domain IS NOT NULL AND protocol = 1
           AND (cardinality($1::text[]) = 0 OR domain = ANY($1))
         ORDER BY id",
    )
    .bind(domains)
    .fetch_all(&state.db)
    .await?;
    let skip_domains: Mutex<BTreeSet<String>> = Mutex::default();
    let skip_domains = &skip_domains;
    let (processed, culled) = parallelize(console, ids, concurrency, false, |id| async move {
        let Some(account) = load(&state.db, id).await? else {
            return Ok(0);
        };
        let domain = account.domain.clone().unwrap_or_default();
        let skipped = skip_domains
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&domain);
        if account.updated_at >= threshold
            || account
                .last_webfingered_at
                .is_some_and(|at| at >= threshold)
            || skipped
        {
            return Ok(0);
        }
        let code = match head(state, account.uri.as_deref().unwrap_or_default()).await {
            Some(code) => code,
            None => {
                skip_domains
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(domain);
                0
            }
        };
        if matches!(code, 404 | 410) {
            if !dry_run {
                crate::delete_account::call(
                    state,
                    account.id,
                    crate::delete_account::Options {
                        reserve_username: false,
                        ..Default::default()
                    },
                )
                .await?;
            }
            return Ok(1);
        }
        // Touched even in a dry run, so that the account leaves the window.
        sqlx::query("UPDATE accounts SET updated_at = now() WHERE id = $1")
            .bind(account.id)
            .execute(&state.db)
            .await?;
        Ok(0)
    })
    .await?;
    console.say(&format!(
        "Visited {processed} accounts, removed {culled}{}",
        dry_run_suffix(dry_run)
    ));
    let skipped = skip_domains.lock().unwrap_or_else(|e| e.into_inner());
    if !skipped.is_empty() {
        console.say("The following domains were not available during the check:");
        for domain in skipped.iter() {
            console.say(&format!("  {domain}"));
        }
    }
    Ok(())
}

/// `Request.new(:head, uri).perform(&:code)`: the status a server answers
/// with, or `None` for one that could not be reached or may not be.
async fn head(state: &AppState, uri: &str) -> Option<u16> {
    let url = url::Url::parse(uri).ok()?;
    let request = state.fetch.request(reqwest::Method::HEAD, &url).ok()?;
    let response = request.send().await.ok()?;
    Some(response.status().as_u16())
}

// ── refresh ───────────────────────────────────────────────────────────────

/// Which remote accounts `refresh` acts on, besides those named.
pub struct Scope {
    pub all: bool,
    pub domain: Option<String>,
}

/// `Accounts#refresh`. Mastodon downloads the avatar and header again; eunha
/// keeps no copies of remote images, and fetches the actor again instead,
/// which is where their URLs come from.
pub async fn refresh(
    state: &AppState,
    console: &dyn Console,
    usernames: &[String],
    scope: Scope,
    concurrency: usize,
    verbose: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let suffix = dry_run_suffix(dry_run);
    if scope.all || scope.domain.is_some() {
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM accounts
             WHERE domain IS NOT NULL AND ($1::text IS NULL OR domain = $1)
             ORDER BY id",
        )
        .bind(&scope.domain)
        .fetch_all(&state.db)
        .await?;
        let (processed, _) = parallelize(console, ids, concurrency, verbose, |id| async move {
            if dry_run {
                return Ok(0);
            }
            let Some(account) = load(&state.db, id).await? else {
                return Ok(0);
            };
            refetch(state, &account).await?;
            Ok(0)
        })
        .await?;
        console.say(&format!("Refreshed {processed} accounts{suffix}"));
        return Ok(());
    }
    if usernames.is_empty() {
        bail!("No account(s) given");
    }
    for acct in usernames {
        let account = find_by_acct(state, acct)
            .await?
            .context("No such account")?;
        if dry_run || account.is_local() {
            continue;
        }
        if refetch(state, &account).await.is_err() {
            console.say(&format!("Account failed: {acct}"));
        }
    }
    console.say(&format!("OK{suffix}"));
    Ok(())
}

/// `ActivityPub::FetchRemoteAccountService` for a known remote account.
async fn refetch(state: &AppState, account: &Account) -> anyhow::Result<()> {
    let uri = account
        .stored_uri()
        .with_context(|| format!("{} has no actor id", account.acct()))?;
    if crate::federation::moderation::domain_not_allowed(state, uri).await {
        bail!("{} is not allowed to federate", account.acct());
    }
    let actor = crate::federation::fetch::signed_get_json(state, uri).await?;
    crate::federation::process_account::process_fetched_actor(
        state, uri, &actor, false, false, None,
    )
    .await?
    .with_context(|| format!("{uri} was not stored"))?;
    Ok(())
}

// ── follow, unfollow and reset-relationships ──────────────────────────────

/// `Accounts#follow`: every local account that is not suspended follows the
/// local account `username`, past the follow limit.
pub async fn follow(
    state: &AppState,
    console: &dyn Console,
    username: &str,
    concurrency: usize,
    verbose: bool,
) -> anyhow::Result<()> {
    let target = find_local(state, username)
        .await?
        .context("No such account")?;
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM accounts
         WHERE domain IS NULL AND id > 0 AND suspended_at IS NULL
         ORDER BY id",
    )
    .fetch_all(&state.db)
    .await?;
    let target = &target;
    let (processed, _) = parallelize(console, ids, concurrency, verbose, |id| async move {
        let Some(source) = load(&state.db, id).await? else {
            return Ok(0);
        };
        crate::api::mastodon::accounts::follow(
            state,
            &source,
            target,
            crate::api::mastodon::accounts::FollowOptions {
                bypass_limit: true,
                ..Default::default()
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        Ok(0)
    })
    .await?;
    console.say(&format!("OK, followed target from {processed} accounts"));
    Ok(())
}

/// `Accounts#unfollow`: every local follower of `acct` unfollows it.
pub async fn unfollow(
    state: &AppState,
    console: &dyn Console,
    acct: &str,
    concurrency: usize,
    verbose: bool,
) -> anyhow::Result<()> {
    let target = find_by_acct(state, acct)
        .await?
        .context("No such account")?;
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT f.account_id FROM follows f JOIN accounts a ON a.id = f.account_id
         WHERE f.target_account_id = $1 AND a.domain IS NULL
         ORDER BY f.id",
    )
    .bind(target.id)
    .fetch_all(&state.db)
    .await?;
    let target_id = target.id;
    let (processed, _) = parallelize(console, ids, concurrency, verbose, |id| async move {
        unfollow_one(state, id, target_id).await?;
        Ok(0)
    })
    .await?;
    console.say(&format!("OK, unfollowed target from {processed} accounts"));
    Ok(())
}

/// `UnfollowService.new.call(follower, target)`.
async fn unfollow_one(state: &AppState, follower: i64, target: i64) -> anyhow::Result<()> {
    crate::api::mastodon::accounts::unfollow(state, follower, target, false)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))
}

/// `Accounts#reset_relationships`.
pub async fn reset_relationships(
    state: &AppState,
    console: &dyn Console,
    username: &str,
    follows: bool,
    followers: bool,
) -> anyhow::Result<()> {
    if !follows && !followers {
        bail!("Please specify either --follows or --followers, or both");
    }
    let account = find_local(state, username)
        .await?
        .context("No such account")?;
    let mut processed = 0;
    if follows {
        let following: Vec<i64> =
            sqlx::query_scalar("SELECT target_account_id FROM follows WHERE account_id = $1")
                .bind(account.id)
                .fetch_all(&state.db)
                .await?;
        for target in following {
            if let Err(error) = unfollow_one(state, account.id, target).await {
                console.say(&format!("Error processing {target}: {error:#}"));
            }
            processed += 1;
        }
        // `BootstrapTimelineWorker.perform_async(account.id)`.
        let invite_id: Option<i64> =
            sqlx::query_scalar("SELECT invite_id FROM users WHERE account_id = $1")
                .bind(account.id)
                .fetch_optional(&state.db)
                .await?
                .flatten();
        crate::accounts::bootstrap_timeline(state, account.id, invite_id).await;
    }
    if followers {
        let followed_by: Vec<i64> =
            sqlx::query_scalar("SELECT account_id FROM follows WHERE target_account_id = $1")
                .bind(account.id)
                .fetch_all(&state.db)
                .await?;
        for follower in followed_by {
            if let Err(error) = unfollow_one(state, follower, account.id).await {
                console.say(&format!("Error processing {follower}: {error:#}"));
            }
            processed += 1;
        }
    }
    console.say(&format!("Processed {processed} relationships"));
    Ok(())
}

// ── approve ───────────────────────────────────────────────────────────────

/// `Accounts#approve`.
pub async fn approve(
    state: &AppState,
    console: &dyn Console,
    username: Option<String>,
    number: Option<i64>,
    all: bool,
) -> anyhow::Result<()> {
    if number.is_some_and(|n| n < 0) {
        bail!("Number must be positive");
    }
    let pending: Vec<i64> = if all {
        sqlx::query_scalar("SELECT account_id FROM users WHERE NOT approved ORDER BY id")
            .fetch_all(&state.db)
            .await?
    } else if let Some(number) = number.filter(|n| *n > 0) {
        sqlx::query_scalar(
            "SELECT account_id FROM users WHERE NOT approved ORDER BY created_at ASC LIMIT $1",
        )
        .bind(number)
        .fetch_all(&state.db)
        .await?
    } else if let Some(username) = username.filter(|name| !name.is_empty()) {
        let account = find_local(state, &username)
            .await?
            .context("No such account")?;
        vec![account.id]
    } else {
        return Ok(());
    };
    for account_id in pending {
        crate::accounts::approve(state, account_id).await?;
    }
    console.say("OK");
    Ok(())
}

// ── prune ─────────────────────────────────────────────────────────────────

/// `prunable_accounts`: remote, not automated, and referenced by nothing
/// that would make it matter here.
const PRUNABLE: &str = "SELECT a.id FROM accounts a
    WHERE a.domain IS NOT NULL
      AND a.actor_type NOT IN ('Application', 'Service')
      AND NOT EXISTS (SELECT 1 FROM mentions x WHERE x.account_id = a.id)
      AND NOT EXISTS (SELECT 1 FROM favourites x WHERE x.account_id = a.id)
      AND NOT EXISTS (SELECT 1 FROM statuses x WHERE x.account_id = a.id AND x.deleted_at IS NULL)
      AND NOT EXISTS (SELECT 1 FROM follows x WHERE x.account_id = a.id)
      AND NOT EXISTS (SELECT 1 FROM follows x WHERE x.target_account_id = a.id)
      AND NOT EXISTS (SELECT 1 FROM blocks x WHERE x.account_id = a.id)
      AND NOT EXISTS (SELECT 1 FROM blocks x WHERE x.target_account_id = a.id)
      AND NOT EXISTS (SELECT 1 FROM mutes x WHERE x.target_account_id = a.id)
      AND NOT EXISTS (SELECT 1 FROM reports x WHERE x.target_account_id = a.id)
      AND NOT EXISTS (SELECT 1 FROM follow_requests x WHERE x.account_id = a.id)
      AND NOT EXISTS (SELECT 1 FROM follow_requests x WHERE x.target_account_id = a.id)
    ORDER BY a.id";

/// `Accounts#prune`: each prunable account that is not a bot or a group,
/// suspended or silenced, is destroyed.
pub async fn prune(
    state: &AppState,
    console: &dyn Console,
    concurrency: usize,
    dry_run: bool,
) -> anyhow::Result<()> {
    let ids: Vec<i64> = sqlx::query_scalar(PRUNABLE).fetch_all(&state.db).await?;
    let (_, deleted) = parallelize(console, ids, concurrency, false, |id| async move {
        let Some(account) = load(&state.db, id).await? else {
            return Ok(0);
        };
        let automated = matches!(
            account.actor_type.as_deref(),
            Some("Application" | "Service" | "Group")
        );
        if automated || account.suspended_at.is_some() || account.silenced_at.is_some() {
            return Ok(0);
        }
        if !dry_run {
            sqlx::query("DELETE FROM accounts WHERE id = $1")
                .bind(account.id)
                .execute(&state.db)
                .await?;
            crate::search::elasticsearch::indexing::account(state, account.id).await;
        }
        Ok(1)
    })
    .await?;
    console.say(&format!(
        "OK, pruned {deleted} accounts{}",
        dry_run_suffix(dry_run)
    ));
    Ok(())
}
