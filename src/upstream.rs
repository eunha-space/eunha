//! Reads the upstream Mastodon repository: releases, migration lists, and the
//! `db/schema.rb` that defines the schema Eunha must reproduce.
//!
//! Eunha promises a database a real Mastodon can be pointed at, so upstream's
//! own schema definition is the only authority worth checking against. Rather
//! than vendoring a copy that silently rots, this module fetches it from GitHub
//! for whichever tag is being asked about.
//!
//! Mastodon: none

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const API: &str = "https://api.github.com/repos/mastodon/mastodon";
const RAW: &str = "https://raw.githubusercontent.com/mastodon/mastodon";

/// An upstream release.
#[derive(Debug, Clone)]
pub struct Release {
    /// Tag as it appears upstream, e.g. `v4.7.0`.
    pub tag: String,
    /// Tag without the leading `v`, matching `mastodon.toml`.
    pub version: String,
    pub published_at: String,
    pub prerelease: bool,
}

#[derive(Deserialize)]
struct RawRelease {
    tag_name: String,
    published_at: Option<String>,
    prerelease: bool,
    draft: bool,
}

impl From<RawRelease> for Release {
    fn from(raw: RawRelease) -> Self {
        let version = raw.tag_name.trim_start_matches('v').to_string();
        Release {
            tag: raw.tag_name,
            version,
            published_at: raw
                .published_at
                .map(|t| t[..10.min(t.len())].to_string())
                .unwrap_or_default(),
            prerelease: raw.prerelease,
        }
    }
}

/// A Rails migration file in `db/migrate` or `db/post_migrate`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Migration {
    /// The 14-digit timestamp that lands in `public.schema_migrations`.
    pub version: String,
    /// Human-readable remainder of the filename, e.g. `add_local_fragment_to_keypair`.
    pub name: String,
    /// `db/post_migrate` migrations run after the new code is deployed.
    pub post_deploy: bool,
}

impl Migration {
    pub fn path(&self) -> String {
        let dir = if self.post_deploy {
            "db/post_migrate"
        } else {
            "db/migrate"
        };
        format!("{dir}/{}_{}.rb", self.version, self.name)
    }
}

/// A column as upstream declares it, normalised to how Postgres reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub sql_type: String,
    pub nullable: bool,
    /// The column's default exactly as Postgres reports it. Both sides of a
    /// comparison are Postgres, so there is nothing to normalise.
    pub default: Option<String>,
}

/// A constraint, compared by name as well as by definition.
///
/// Names matter rather than being incidental: Rails derives a foreign key's
/// name from a hash of the table and column and its own migrations drop
/// constraints by that name, so a constraint that describes the same rule under
/// a different name is still drift.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Constraint {
    pub table: String,
    pub name: String,
    /// `pg_constraint.contype`: `f` foreign key, `p` primary key, `n` not null,
    /// `c` check, `u` unique.
    pub kind: String,
    /// `pg_get_constraintdef`, with `public.` elided.
    pub definition: String,
}

/// A sequence.
///
/// Ownership — whether a column owns the sequence, as a serial column's does —
/// is deliberately not recorded. It depends on how a table came to be rather
/// than on what it is: `quotes` was created with a serial id and later switched
/// to `timestamp_id`, so a Mastodon that migrated owns that sequence while one
/// built from `schema.rb` does not. Neither is wrong, and comparing it would
/// report a difference on every check.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Sequence {
    pub name: String,
    pub data_type: String,
    pub increment: i64,
}

/// An index, compared by name, uniqueness and the definition Postgres reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    pub unique: bool,
    /// `pg_get_indexdef`, with the table name elided: the columns or expression
    /// the index covers, its method, and any partial predicate.
    pub definition: String,
}

/// A view or materialized view, compared by the SQL it is defined as.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct View {
    pub name: String,
    pub materialized: bool,
    pub definition: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Table {
    pub columns: BTreeMap<String, Column>,
    pub indexes: BTreeMap<String, Index>,
}

/// The full picture of a schema, from either upstream or a live database.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Schema {
    /// `max(version)` from a live `public.schema_migrations`.
    pub version: String,
    pub tables: BTreeMap<String, Table>,
    /// Every constraint, of every kind, keyed by table and name.
    #[serde(default)]
    pub constraints: BTreeSet<Constraint>,
    #[serde(default)]
    pub sequences: BTreeMap<String, Sequence>,
    #[serde(default)]
    pub views: BTreeMap<String, View>,
}

fn client() -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    // Unauthenticated GitHub allows 60 requests an hour, which a few `check`
    // runs can exhaust; honour a token when one is around.
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        if !token.is_empty() {
            headers.insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {token}").parse()?,
            );
        }
    }
    Ok(reqwest::Client::builder()
        .user_agent(crate::version::USER_AGENT)
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(60))
        .build()?)
}

async fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T> {
    let response = client()?
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = response.status();
    let body = response.text().await?;
    anyhow::ensure!(
        status.is_success(),
        "GET {url} failed with {status}: {}",
        body.chars().take(200).collect::<String>()
    );
    serde_json::from_str(&body).with_context(|| format!("parsing response from {url}"))
}

/// The newest stable release upstream.
pub async fn latest_release() -> Result<Release> {
    let raw: RawRelease = get_json(&format!("{API}/releases/latest")).await?;
    Ok(raw.into())
}

/// Recent releases, newest first, excluding drafts.
pub async fn releases() -> Result<Vec<Release>> {
    let raw: Vec<RawRelease> = get_json(&format!("{API}/releases?per_page=100")).await?;
    Ok(raw
        .into_iter()
        .filter(|r| !r.draft)
        .map(Release::from)
        .collect())
}

#[derive(Deserialize)]
struct TreeResponse {
    tree: Vec<TreeEntry>,
    #[serde(default)]
    truncated: bool,
}

#[derive(Deserialize)]
struct TreeEntry {
    path: String,
}

/// Every migration shipped in `tag`, ordered by version.
/// A Mastodon checkout to read tagged files from instead of the network.
///
/// Asking GitHub for one file at a time is slow and gets throttled, and a
/// clone already has every tag. `MASTODON_REPO` overrides the usual location.
/// Returns `None` when there is no clone, or it does not have the tag, in which
/// case the caller falls back to fetching.
fn local_checkout(tag: &str) -> Option<std::path::PathBuf> {
    let repo = std::env::var("MASTODON_REPO")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Git/mastodon")
        });
    if !repo.join(".git").exists() {
        return None;
    }
    // Read the tag rather than the working tree, so whatever the clone happens
    // to be checked out at cannot be recorded as upstream's.
    let found = std::process::Command::new("git")
        .args(["-C", repo.to_str()?, "rev-parse", "-q", "--verify"])
        .arg(format!("refs/tags/{tag}"))
        .output()
        .ok()?;
    found.status.success().then_some(repo)
}

fn git_output(repo: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub async fn migrations(tag: &str) -> Result<Vec<Migration>> {
    let repo = local_checkout(tag);
    let mut all = Vec::new();
    for (dir, post_deploy) in [("db/migrate", false), ("db/post_migrate", true)] {
        let paths: Vec<String> = if let Some(repo) = &repo {
            let listing = git_output(repo, &["ls-tree", "-r", "--name-only", tag, dir])
                .with_context(|| format!("listing {dir} at {tag}"))?;
            listing
                .lines()
                .filter_map(|p| p.rsplit('/').next().map(str::to_owned))
                .collect()
        } else {
            let url = format!("{API}/git/trees/{tag}:{}", dir.replace('/', "%2F"));
            let tree: TreeResponse = get_json(&url).await?;
            anyhow::ensure!(!tree.truncated, "{url} returned a truncated tree");
            tree.tree.into_iter().map(|e| e.path).collect()
        };
        for path in paths {
            let Some(stem) = path.strip_suffix(".rb") else {
                continue;
            };
            let Some((version, name)) = stem.split_once('_') else {
                continue;
            };
            if version.len() != 14 || !version.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            all.push(Migration {
                version: version.to_string(),
                name: name.to_string(),
                post_deploy,
            });
        }
    }
    all.sort();
    Ok(all)
}

/// Fetch upstream's `db/schema.rb` at `tag`, verbatim.
///
/// # Errors
/// Returns an error if the file cannot be fetched.
pub async fn schema_rb(tag: &str) -> Result<String> {
    if let Some(repo) = local_checkout(tag) {
        if let Some(body) = git_output(&repo, &["show", &format!("{tag}:db/schema.rb")]) {
            return Ok(body);
        }
    }
    let url = format!("{RAW}/{tag}/db/schema.rb");
    Ok(client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?
        .text()
        .await?)
}

/// The `define(version:)` of upstream's `db/schema.rb` at `tag` — the newest
/// migration that release contains.
///
/// # Errors
/// Returns an error if the file cannot be fetched or states no version.
pub async fn schema_rb_version(tag: &str) -> Result<String> {
    let body = schema_rb(tag).await?;
    let at = body
        .find("define(version:")
        .context("schema.rb states no version")?;
    let rest = &body[at + "define(version:".len()..];
    let end = rest.find(')').context("malformed define(version:)")?;
    Ok(rest[..end].trim().replace('_', ""))
}

/// The reference schema Eunha is checked against: what Mastodon's own
/// ActiveRecord builds from its `db/schema.rb`, introspected once and recorded
/// here.
///
/// Recorded rather than derived, because deriving it means either parsing Ruby
/// — a parser of ours standing between us and the truth — or running Rails,
/// which no test should need. `scripts/build_mastodon_schema.sh` regenerates
/// both this and the `.sql` beside it when a release is adopted.
///
/// # Panics
/// Panics if the vendored file is not valid JSON, which would mean the build
/// script wrote something broken.
#[must_use]
pub fn reference_schema() -> Schema {
    serde_json::from_str(include_str!("../mastodon/schema.json"))
        .expect("mastodon/schema.json is not a valid recorded schema")
}
