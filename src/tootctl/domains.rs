//! `tootctl domains` (`Mastodon::CLI::Domains`): purging what is known of
//! other servers, and crawling the fediverse for statistics.

use std::collections::{BTreeMap, VecDeque};

use anyhow::bail;
use futures::stream::FuturesUnordered;
use futures::StreamExt as _;
use serde_json::Value;

use super::console::{dry_run_suffix, parallelize, Console};
use crate::state::AppState;

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Remove accounts from a DOMAIN without a trace, as `tootctl domains
    /// purge` does.
    ///
    /// Unlike a suspension, nothing is kept: if the domain is still out
    /// there, its accounts come back when they are resolved again. `*.DOMAIN`
    /// purges its subdomains.
    Purge {
        domains: Vec<String>,
        #[arg(short = 'c', long, default_value_t = 5)]
        concurrency: usize,
        #[arg(short, long)]
        verbose: bool,
        /// Say what would be removed, removing nothing.
        #[arg(long)]
        dry_run: bool,
        /// Instead of DOMAIN, every domain that has not been explicitly
        /// allowed.
        #[arg(long)]
        limited_federation_mode: bool,
        /// Match DOMAIN against the host of the accounts' actor ids rather
        /// than their handles' domain.
        #[arg(long)]
        by_uri: bool,
        /// Purge DOMAIN's subdomains too.
        #[arg(long)]
        include_subdomains: bool,
        /// Remove the domain blocks of DOMAIN as well.
        #[arg(long)]
        purge_domain_blocks: bool,
    },
    /// Crawl all known peers, optionally beginning at START, as `tootctl
    /// domains crawl` does.
    ///
    /// Asks each server for its `/api/v1/instance`, its peers and its weekly
    /// activity, and each peer in turn. Without START, begins at the servers
    /// this instance knows.
    Crawl {
        start: Option<String>,
        /// How many servers are asked at once.
        #[arg(short = 'c', long, default_value_t = 50)]
        concurrency: usize,
        /// `summary` of the statistics, the `domains` found, a line each, or
        /// all of it as `json`.
        #[arg(short = 'f', long, default_value = "summary")]
        format: String,
        /// Leave out servers suspended here, and their subdomains.
        #[arg(short = 'x', long)]
        exclude_suspended: bool,
    },
}

impl Command {
    /// How many accounts the command works on at once.
    pub fn concurrency(&self) -> usize {
        match self {
            Self::Purge { concurrency, .. } => *concurrency,
            Self::Crawl { .. } => 1,
        }
    }

    pub async fn run(self, state: &AppState, console: &dyn Console) -> anyhow::Result<()> {
        match self {
            Self::Purge {
                domains,
                concurrency,
                verbose,
                dry_run,
                limited_federation_mode,
                by_uri,
                include_subdomains,
                purge_domain_blocks,
                ..
            } => {
                purge(
                    state,
                    console,
                    &domains,
                    &PurgeOptions {
                        concurrency,
                        verbose,
                        dry_run,
                        limited_federation_mode,
                        by_uri,
                        include_subdomains,
                        purge_domain_blocks,
                    },
                )
                .await
            }
            Self::Crawl {
                start,
                concurrency,
                format,
                exclude_suspended,
                ..
            } => {
                let format = Format::parse(&format)?;
                let crawled = crawl(
                    state,
                    start.as_deref(),
                    concurrency,
                    exclude_suspended,
                    "https",
                )
                .await?;
                crawled.print(console, format);
                Ok(())
            }
        }
    }
}

// ── purge ─────────────────────────────────────────────────────────────────

/// What `domains purge` was asked to do.
#[derive(Debug, Clone)]
pub struct PurgeOptions {
    pub concurrency: usize,
    pub verbose: bool,
    pub dry_run: bool,
    pub limited_federation_mode: bool,
    pub by_uri: bool,
    pub include_subdomains: bool,
    pub purge_domain_blocks: bool,
}

/// `Account.sanitize_sql_like`.
fn sanitize_like(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// `TagManager.instance.normalize_domain`, keeping a leading `*.`.
fn normalize(domain: &str) -> String {
    let trimmed = domain.trim();
    match trimmed.strip_prefix("*.") {
        Some(rest) => format!(
            "*.{}",
            crate::federation::tag_manager::normalize_domain(rest)
                .unwrap_or_else(|_| rest.to_lowercase())
        ),
        None => crate::federation::tag_manager::normalize_domain(trimmed)
            .unwrap_or_else(|_| trimmed.to_lowercase()),
    }
}

/// `Domains#purge`.
pub async fn purge(
    state: &AppState,
    console: &dyn Console,
    domains: &[String],
    options: &PurgeOptions,
) -> anyhow::Result<()> {
    let domains: Vec<String> = domains.iter().map(|domain| normalize(domain)).collect();
    if options.limited_federation_mode && !domains.is_empty() {
        bail!("DOMAIN parameter not supported with --limited-federation-mode");
    }
    if domains.is_empty() && !options.limited_federation_mode {
        bail!("No domain(s) given");
    }

    // The scopes, as SQL conditions over their table, and their binds.
    let (accounts, emojis, blocks, binds): (String, String, Option<String>, Binds) =
        if options.limited_federation_mode {
            (
                "domain IS NOT NULL AND domain NOT IN (SELECT domain FROM domain_allows)".into(),
                "domain IS NOT NULL AND domain NOT IN (SELECT domain FROM domain_allows)".into(),
                None,
                Binds::default(),
            )
        } else {
            let mut subdomain_patterns: Vec<String> = domains
                .iter()
                .filter_map(|domain| domain.strip_prefix("*."))
                .map(|rest| format!("%.{}", sanitize_like(rest)))
                .collect();
            let exact: Vec<String> = domains
                .iter()
                .filter(|domain| !domain.starts_with("*."))
                .cloned()
                .collect();
            if options.include_subdomains {
                subdomain_patterns.extend(
                    exact
                        .iter()
                        .map(|domain| format!("%.{}", sanitize_like(domain))),
                );
            }
            let uri_patterns: Vec<String> = exact
                .iter()
                .map(|domain| sanitize_like(domain))
                .chain(subdomain_patterns.iter().cloned())
                .map(|pattern| format!("https://{pattern}/%"))
                .collect();
            let blocks = options
                .purge_domain_blocks
                .then(|| "domain = ANY($1) OR domain ILIKE ANY($2)".to_owned());
            let (accounts, emojis) = if options.by_uri {
                (
                    "domain IS NOT NULL AND uri LIKE ANY($3)".to_owned(),
                    "domain IS NOT NULL AND uri LIKE ANY($3)".to_owned(),
                )
            } else {
                (
                    "domain IS NOT NULL AND (domain = ANY($1) OR domain ILIKE ANY($2))".to_owned(),
                    "domain = ANY($1) OR (domain IS NOT NULL AND uri ILIKE ANY($2))".to_owned(),
                )
            };
            (
                accounts,
                emojis,
                blocks,
                Binds {
                    exact,
                    subdomain_patterns,
                    uri_patterns,
                },
            )
        };

    let suffix = dry_run_suffix(options.dry_run);
    let ids: Vec<i64> = binds
        .apply(sqlx::query_scalar(&format!(
            "SELECT id FROM accounts WHERE {accounts} ORDER BY id"
        )))
        .fetch_all(&state.db)
        .await?;
    let dry_run = options.dry_run;
    let (processed, _) = parallelize(
        console,
        ids,
        options.concurrency,
        options.verbose,
        |id| async move {
            if !dry_run {
                crate::delete_account::call(
                    state,
                    id,
                    crate::delete_account::Options {
                        reserve_username: false,
                        skip_side_effects: true,
                        ..Default::default()
                    },
                )
                .await?;
            }
            Ok(0)
        },
    )
    .await?;
    console.say(&format!("Removed {processed} accounts{suffix}"));

    if let Some(blocks) = blocks {
        let count: i64 = binds
            .apply(sqlx::query_scalar(&format!(
                "SELECT count(*) FROM domain_blocks WHERE {blocks}"
            )))
            .fetch_one(&state.db)
            .await?;
        if !dry_run {
            binds
                .apply_query(sqlx::query(&format!(
                    "DELETE FROM domain_blocks WHERE {blocks}"
                )))
                .execute(&state.db)
                .await?;
        }
        console.say(&format!("Removed {count} domain blocks{suffix}"));
    }

    let emoji_count: i64 = binds
        .apply(sqlx::query_scalar(&format!(
            "SELECT count(*) FROM custom_emojis WHERE {emojis}"
        )))
        .fetch_one(&state.db)
        .await?;
    if !dry_run {
        let ids: Vec<i64> = binds
            .apply(sqlx::query_scalar(&format!(
                "SELECT id FROM custom_emojis WHERE {emojis}"
            )))
            .fetch_all(&state.db)
            .await?;
        super::emoji::delete_ids(state, &ids).await?;
        crate::background::refresh_instances(state).await?;
    }
    console.say(&format!("Removed {emoji_count} custom emojis{suffix}"));
    Ok(())
}

/// What the purge scopes are bound to: `$1` the exact domains, `$2` the
/// subdomain patterns, `$3` the actor id patterns.
#[derive(Debug, Default)]
struct Binds {
    exact: Vec<String>,
    subdomain_patterns: Vec<String>,
    uri_patterns: Vec<String>,
}

impl Binds {
    fn apply<'q, O>(
        &'q self,
        query: sqlx::query::QueryScalar<'q, sqlx::Postgres, O, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::QueryScalar<'q, sqlx::Postgres, O, sqlx::postgres::PgArguments> {
        query
            .bind(&self.exact)
            .bind(&self.subdomain_patterns)
            .bind(&self.uri_patterns)
    }

    fn apply_query<'q>(
        &'q self,
        query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
        query
            .bind(&self.exact)
            .bind(&self.subdomain_patterns)
            .bind(&self.uri_patterns)
    }
}

// ── crawl ─────────────────────────────────────────────────────────────────

/// How `crawl` shows what it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Summary,
    Domains,
    Json,
}

impl Format {
    pub fn parse(format: &str) -> anyhow::Result<Self> {
        match format {
            "summary" => Ok(Self::Summary),
            "domains" => Ok(Self::Domains),
            "json" => Ok(Self::Json),
            other => bail!("{other} is not one of summary, domains, json"),
        }
    }
}

/// What a crawl found: each domain visited and what its `/api/v1/instance`
/// said, with its `activity` added, or nothing for a server that did not say.
#[derive(Debug, Default)]
pub struct Crawled {
    pub stats: BTreeMap<String, Value>,
    pub processed: u64,
    pub failed: u64,
    pub elapsed: std::time::Duration,
}

impl Crawled {
    /// `stats_to_summary`, `stats_to_domains` or `stats_to_json`.
    pub fn print(&self, console: &dyn Console, format: Format) {
        let answered = || self.stats.iter().filter(|(_, stats)| !stats.is_null());
        match format {
            Format::Summary => {
                let week = |key: &str| -> i64 {
                    answered()
                        .filter_map(|(_, stats)| {
                            let activity = stats.get("activity")?.as_array()?;
                            if activity.len() <= 2 {
                                return None;
                            }
                            Some(to_i(activity[1].as_object()?.get(key)))
                        })
                        .sum()
                };
                let registered: i64 = answered()
                    .filter_map(|(_, stats)| {
                        Some(to_i(stats.get("stats")?.as_object()?.get("user_count")))
                    })
                    .sum();
                console.say(&format!(
                    "Visited {} domains, {} failed ({}s elapsed)",
                    self.processed,
                    self.failed,
                    self.elapsed.as_secs_f64().round()
                ));
                console.say(&format!("Total servers: {}", answered().count()));
                console.say(&format!("Total registered: {registered}"));
                console.say(&format!("Total active last week: {}", week("logins")));
                console.say(&format!(
                    "Total joined last week: {}",
                    week("registrations")
                ));
            }
            Format::Domains => {
                for domain in self.stats.keys() {
                    console.say(domain);
                }
            }
            Format::Json => {
                let compact: serde_json::Map<String, Value> = answered()
                    .map(|(domain, stats)| (domain.clone(), stats.clone()))
                    .collect();
                console.say(&Value::Object(compact).to_string());
            }
        }
    }
}

/// Ruby's `to_i` on what a server said a count was: a number, or a string
/// of one.
fn to_i(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => {
            let digits: String = s
                .trim()
                .chars()
                .enumerate()
                .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && *c == '-'))
                .map(|(_, c)| c)
                .collect();
            digits.parse().unwrap_or(0)
        }
        _ => 0,
    }
}

/// `Domains#crawl`. `scheme` is `https` but in the tests, whose servers
/// listen on plain HTTP.
pub async fn crawl(
    state: &AppState,
    start: Option<&str>,
    concurrency: usize,
    exclude_suspended: bool,
    scheme: &str,
) -> anyhow::Result<Crawled> {
    anyhow::ensure!(
        concurrency >= 1,
        "Cannot run with this concurrency setting, must be at least 1"
    );
    let started = std::time::Instant::now();
    let seed: Vec<String> = match start {
        Some(start) => vec![start.to_owned()],
        None => {
            sqlx::query_scalar("SELECT domain FROM instances")
                .fetch_all(&state.db)
                .await?
        }
    };
    // `/\.?(#{Regexp.union(suspended)})$/`.
    let suspended: Vec<String> =
        sqlx::query_scalar("SELECT domain FROM domain_blocks WHERE severity = 1")
            .fetch_all(&state.db)
            .await?;
    let blocked = |domain: &str| {
        suspended
            .iter()
            .any(|blocked| domain.ends_with(blocked.as_str()))
    };

    let mut crawled = Crawled::default();
    let mut queue: VecDeque<String> = seed.into();
    let mut running = FuturesUnordered::new();
    loop {
        while running.len() < concurrency {
            let Some(domain) = queue.pop_front() else {
                break;
            };
            if crawled.stats.contains_key(&domain) || (exclude_suspended && blocked(&domain)) {
                continue;
            }
            crawled.stats.insert(domain.clone(), Value::Null);
            running.push(visit(state, domain, scheme));
        }
        let Some((domain, visit)) = running.next().await else {
            break;
        };
        crawled.processed += 1;
        if visit.failed {
            crawled.failed += 1;
        }
        for peer in visit.peers {
            if !crawled.stats.contains_key(&peer) {
                queue.push_back(peer);
            }
        }
        if let Some(mut instance) = visit.instance {
            if let (Some(object), Some(activity)) = (instance.as_object_mut(), visit.activity) {
                object.insert("activity".into(), activity);
            }
            crawled.stats.insert(domain, instance);
        }
    }
    crawled.elapsed = started.elapsed();
    Ok(crawled)
}

/// What one server said, and whether asking it failed part of the way.
#[derive(Default)]
struct Visit {
    instance: Option<Value>,
    peers: Vec<String>,
    activity: Option<Value>,
    failed: bool,
}

/// The work unit: a server's instance, its peers and its activity, in that
/// order. A request that fails, or an answer that is not JSON, ends the visit
/// as a failure, keeping what was read before it; so does an activity from a
/// server that did not say what it is, which `stats[domain]['activity'] =`
/// raises on.
async fn visit(state: &AppState, domain: String, scheme: &str) -> (String, Visit) {
    let mut visit = Visit::default();
    let base = format!("{scheme}://{domain}");
    let result: Result<(), ()> = async {
        visit.instance = get_json(state, &format!("{base}/api/v1/instance")).await?;
        if let Some(peers) = get_json(state, &format!("{base}/api/v1/instance/peers")).await? {
            visit.peers = peers
                .as_array()
                .ok_or(())?
                .iter()
                .filter_map(|peer| peer.as_str().map(str::to_owned))
                .collect();
        }
        let activity = get_json(state, &format!("{base}/api/v1/instance/activity")).await?;
        if activity.is_some() && visit.instance.is_none() {
            return Err(());
        }
        visit.activity = activity;
        Ok(())
    }
    .await;
    visit.failed = result.is_err();
    (domain, visit)
}

/// A `GET` through the guarded client: the JSON of a 200, `None` for any
/// other status, and an error for a request that failed or a body that is
/// not JSON.
async fn get_json(state: &AppState, url: &str) -> Result<Option<Value>, ()> {
    let url = url::Url::parse(url).map_err(|_| ())?;
    let request = state
        .fetch
        .request(reqwest::Method::GET, &url)
        .map_err(|_| ())?
        .header(reqwest::header::ACCEPT, "application/json");
    let response = request.send().await.map_err(|_| ())?;
    if response.status() != reqwest::StatusCode::OK {
        return Ok(None);
    }
    let body = response.bytes().await.map_err(|_| ())?;
    serde_json::from_slice(&body).map(Some).map_err(|_| ())
}
