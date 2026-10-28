//! Tracks upstream Mastodon releases and checks Eunha's database against them.
//!
//! Eunha aims to be a drop-in replacement on top of an existing Mastodon
//! database, which means two ongoing obligations: noticing when upstream ships
//! a release, and proving that what Eunha's migrations build is still the same
//! schema upstream builds. This tool covers both.
//!
//!   eunha-schema status            — is there a newer Mastodon release?
//!   eunha-schema plan --to v4.8.0  — what would adopting it involve?
//!   eunha-schema check             — does a live database match the target?
//!   eunha-schema changes --to v4.8.0 — which Eunha modules does it touch?
//!
//! Mastodon: none

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use eunha::{counterparts, schema_check, upstream, version};

#[derive(Parser, Debug)]
#[command(
    name = "eunha-schema",
    about = "Track Mastodon releases and verify database schema compatibility"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Compare the tracked Mastodon release against the newest one upstream.
    Status,
    /// List the upstream migrations between what Eunha covers and a target release.
    Plan {
        /// Target release tag, e.g. `v4.8.0`. Defaults to the newest stable release.
        #[arg(long)]
        to: Option<String>,
        /// Print an INSERT for public.schema_migrations covering the new versions.
        #[arg(long)]
        sql: bool,
    },
    /// Diff a live database against the recorded Mastodon schema.
    Check {
        /// Database to inspect. Defaults to $DATABASE_URL.
        #[arg(long)]
        database_url: Option<String>,
    },
    /// Record a reference database's structure as `mastodon/schema.json`.
    ///
    /// Run against the database `scripts/build_mastodon_schema.sh` builds from
    /// Mastodon's own `db/schema.rb`; that script does this for you.
    RecordReference {
        /// The reference database built from Mastodon's schema.rb.
        #[arg(long)]
        database_url: String,
    },
    /// List the upstream files changed since the tracked release, and the
    /// Eunha modules that name each one as a counterpart.
    ///
    /// Reads a local Mastodon checkout, since a release's diff runs to more
    /// files than GitHub's compare API returns.
    Changes {
        /// Target release tag, e.g. `v4.8.0`. Defaults to the newest stable release.
        #[arg(long)]
        to: Option<String>,
        /// Mastodon checkout. Defaults to $MASTODON_REPO, then ~/Git/mastodon.
        #[arg(long)]
        repo: Option<std::path::PathBuf>,
    },
    /// Record the tracked release's tree as `mastodon/files.txt`, which the
    /// counterpart headers are checked against.
    RecordFiles {
        /// Mastodon checkout. Defaults to $MASTODON_REPO, then ~/Git/mastodon.
        #[arg(long)]
        repo: Option<std::path::PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    match Args::parse().command {
        Command::Status => status().await,
        Command::Plan { to, sql } => plan(to, sql).await,
        Command::Check { database_url } => check(database_url).await,
        Command::RecordReference { database_url } => record_reference(database_url).await,
        Command::Changes { to, repo } => changes(to, repo).await,
        Command::RecordFiles { repo } => record_files(repo),
    }
}

/// Upstream migrations Eunha applies from code rather than from a file in
/// `migrations/`, because what they do depends on the instance rather than on
/// the schema. They are recorded in `public.schema_migrations` when they run.
const RUNTIME_MIGRATIONS: [(&str, &str); 1] = [(
    "20260702144128",
    "moves local signing keys into `keypairs`, encrypted; runs at startup once \
     ActiveRecord encryption keys are configured",
)];

/// The tag for the release this build targets.
fn tracked_tag() -> String {
    format!("v{}", version::MASTODON)
}

async fn status() -> Result<()> {
    println!("Eunha            {}", version::EUNHA_FULL);
    println!(
        "tracking         Mastodon {} (schema {})",
        version::MASTODON,
        version::MASTODON_SCHEMA
    );

    // `check` compares against a reference built from the vendored
    // `mastodon/schema.rb`, so it is worth knowing when that file has drifted
    // from the release it claims to be.
    match upstream::schema_rb(&tracked_tag()).await {
        Ok(upstream_rb) => {
            let vendored = include_str!("../../mastodon/schema.rb");
            if vendored == upstream_rb {
                println!("vendored schema  matches {} upstream", tracked_tag());
            } else {
                println!(
                    "\nWARNING: mastodon/schema.rb differs from {} upstream.\n\
                     Re-run scripts/build_mastodon_schema.sh after updating it.",
                    tracked_tag()
                );
            }
        }
        Err(e) => println!("vendored schema  could not be checked against upstream: {e}"),
    }

    let latest = upstream::latest_release().await?;
    println!(
        "latest upstream  Mastodon {} (released {})",
        latest.version, latest.published_at
    );

    if latest.version == version::MASTODON {
        println!("\nUp to date.");
        return Ok(());
    }

    // A newer tag is not always a newer *branch*: upstream backports fixes to
    // older series, so 4.6.7 can be published after 4.7.0. Only report a real
    // upgrade if the tag differs from the one being tracked.
    let migrations = upstream::migrations(&latest.tag).await?;
    let covered = schema_check::covered_versions(migrations_dir().as_path())?;
    let outstanding = migrations
        .iter()
        .filter(|m| !covered.contains(&m.version))
        .filter(|m| {
            !RUNTIME_MIGRATIONS
                .iter()
                .any(|(version, _)| *version == m.version)
        })
        .count();

    println!(
        "\nMastodon {} is available, with {outstanding} migration(s) Eunha does not cover yet.",
        latest.version
    );
    println!(
        "Run `eunha-schema plan --to {}` for the details.",
        latest.tag
    );
    Ok(())
}

async fn plan(to: Option<String>, sql: bool) -> Result<()> {
    let target = target_tag(to).await?;

    let migrations = upstream::migrations(&target).await?;
    let covered = schema_check::covered_versions(migrations_dir().as_path())?;
    let (runtime, outstanding): (Vec<_>, Vec<_>) = migrations
        .iter()
        .filter(|m| !covered.contains(&m.version))
        .partition(|m| {
            RUNTIME_MIGRATIONS
                .iter()
                .any(|(version, _)| *version == m.version)
        });

    let adopting = target != tracked_tag();
    if adopting {
        println!(
            "Adopting Mastodon {target} from {} ({} of {} migrations outstanding)\n",
            tracked_tag(),
            outstanding.len(),
            migrations.len()
        );
    } else {
        println!(
            "Already tracking Mastodon {target} ({} of {} migrations outstanding)\n",
            outstanding.len(),
            migrations.len()
        );
    }

    if outstanding.is_empty() {
        println!("migrations/ already covers every migration in {target}.");
    } else {
        for migration in &outstanding {
            let kind = if migration.post_deploy {
                "post"
            } else {
                "    "
            };
            println!("  {kind}  {}  {}", migration.version, migration.name);
        }
        println!("\nSource: https://github.com/mastodon/mastodon/tree/{target}/db");
    }

    if !runtime.is_empty() {
        println!("\nApplied from code, once per instance rather than once per schema:");
        for migration in &runtime {
            let note = RUNTIME_MIGRATIONS
                .iter()
                .find(|(version, _)| *version == migration.version)
                .map(|(_, note)| *note)
                .unwrap_or_default();
            println!("  {}  {}\n      {note}", migration.version, migration.name);
        }
    }

    if adopting {
        let schema_version = upstream::schema_rb_version(&target).await?;
        println!("\nSchema version at {target}: {schema_version}");
        println!(
            "Once migrations/ covers those, set mastodon.toml to version = \"{}\",",
            target.trim_start_matches('v')
        );
        println!(
            "schema_version = \"{}\", and give Cargo.toml a version ending in",
            schema_version
        );
        println!(
            "`+mastodon.{}`. Then refresh mastodon/schema.rb from that tag and run",
            target.trim_start_matches('v')
        );
        println!("scripts/build_mastodon_schema.sh, which records the new reference.");

        let divergences = eunha::divergence::all();
        if !divergences.is_empty() {
            println!(
                "\nThen re-examine {} deliberate divergence(s) against {target}. For each:\n\
                 has upstream adopted it, changed what it differs from, or ruled it out?",
                divergences.len()
            );
            for d in &divergences {
                println!("\n  {} ({})", d.id, d.kind.as_str());
                println!("    mastodon: {}", d.mastodon.trim().replace('\n', " "));
                println!("    eunha:    {}", d.eunha.trim().replace('\n', " "));
            }
            println!(
                "\nMoving each `reviewed_for` in divergences.toml is what records the answer."
            );
        }
    }

    if sql && !outstanding.is_empty() {
        println!("\n-- Record the migrations this covers, as Mastodon would.");
        println!("INSERT INTO public.schema_migrations (version) VALUES");
        let rows: Vec<String> = outstanding
            .iter()
            .map(|m| format!("    ('{}')", m.version))
            .collect();
        println!("{}\nON CONFLICT (version) DO NOTHING;", rows.join(",\n"));
    }

    Ok(())
}

async fn check(database_url: Option<String>) -> Result<()> {
    let database_url = database_url
        .or_else(|| std::env::var("DATABASE_URL").ok())
        .context("no database given: pass --database-url or set DATABASE_URL")?;

    let expected = upstream::reference_schema();
    let pool = sqlx::PgPool::connect(&database_url)
        .await
        .with_context(|| format!("connecting to {database_url}"))?;
    let live = schema_check::introspect(&pool).await?;

    let findings = schema_check::diff(&live, &expected);
    if findings.is_empty() {
        println!(
            "Schema matches Mastodon {} ({} tables, {} constraints, {} indexes).",
            version::MASTODON,
            expected.tables.len(),
            expected.constraints.len(),
            expected
                .tables
                .values()
                .map(|t| t.indexes.len())
                .sum::<usize>()
        );
        return Ok(());
    }

    println!(
        "{} difference(s) from Mastodon {}:\n",
        findings.len(),
        version::MASTODON
    );
    for finding in &findings {
        println!("  {finding}");
    }
    // A drifted schema is a failure, so CI and `mise run schema:check` notice.
    std::process::exit(1);
}

/// Record what a reference database looks like, for `check` to compare against.
async fn record_reference(database_url: String) -> Result<()> {
    let pool = sqlx::PgPool::connect(&database_url)
        .await
        .with_context(|| format!("connecting to {database_url}"))?;
    let schema = schema_check::introspect(&pool).await?;

    anyhow::ensure!(
        schema.tables.len() > 100,
        "the reference database has only {} tables; is it the right one?",
        schema.tables.len()
    );

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("mastodon/schema.json");
    // Pretty-printed so that adopting a release shows a readable diff rather
    // than one very long line.
    let json = serde_json::to_string_pretty(&schema)?;
    std::fs::write(&path, json + "\n").with_context(|| format!("writing {}", path.display()))?;

    println!(
        "Recorded {} tables, {} constraints, {} sequences and {} views to {}.",
        schema.tables.len(),
        schema.constraints.len(),
        schema.sequences.len(),
        schema.views.len(),
        path.display()
    );
    Ok(())
}

/// `--to` as a tag, or the newest stable release when it is not given.
async fn target_tag(to: Option<String>) -> Result<String> {
    Ok(match to {
        Some(tag) if tag.starts_with('v') => tag,
        Some(version) => format!("v{version}"),
        None => upstream::latest_release().await?.tag,
    })
}

/// A Mastodon checkout holding `tags`, fetching tags into it if it lacks one.
fn mastodon_repo(repo: Option<std::path::PathBuf>, tags: &[&str]) -> Result<std::path::PathBuf> {
    let repo = repo
        .or_else(|| std::env::var_os("MASTODON_REPO").map(Into::into))
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join("Git/mastodon")))
        .context("no Mastodon checkout: pass --repo or set MASTODON_REPO")?;
    anyhow::ensure!(
        repo.join(".git").exists(),
        "no Mastodon checkout at {}; clone https://github.com/mastodon/mastodon.git \
         there, or pass --repo",
        repo.display()
    );
    let has = |tag: &str| {
        git(
            &repo,
            &["rev-parse", "-q", "--verify", &format!("refs/tags/{tag}")],
        )
        .is_ok()
    };
    if !tags.iter().all(|t| has(t)) {
        // Tags only: never touches the checkout's branch or working tree.
        git(&repo, &["fetch", "--tags", "--quiet", "origin"])?;
        for tag in tags {
            anyhow::ensure!(
                has(tag),
                "{} has no tag {tag}, even after fetching",
                repo.display()
            );
        }
    }
    Ok(repo)
}

/// Run git in `repo`, returning its output.
fn git(repo: &std::path::Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("running git")?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?)
}

/// Record the tracked release's tree, for the counterpart check.
fn record_files(repo: Option<std::path::PathBuf>) -> Result<()> {
    let tag = tracked_tag();
    let repo = mastodon_repo(repo, &[&tag])?;
    // Reading the tag rather than the working tree, so whatever the checkout
    // has checked out does not leak in.
    let tree = git(&repo, &["ls-tree", "-r", "--name-only", &tag])?;
    let files: Vec<&str> = tree.lines().filter(|f| counterparts::in_scope(f)).collect();

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("mastodon/files.txt");
    let contents = format!("# {tag}\n{}\n", files.join("\n"));
    std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    println!(
        "Recorded {} files from {tag} to {}.",
        files.len(),
        path.display()
    );
    Ok(())
}

/// What changed upstream between the tracked release and `to`, by Eunha module.
async fn changes(to: Option<String>, repo: Option<std::path::PathBuf>) -> Result<()> {
    use std::collections::BTreeMap;

    let from = tracked_tag();
    let target = target_tag(to).await?;
    let repo = mastodon_repo(repo, &[&from, &target])?;
    let diff = git(&repo, &["diff", "--name-status", "-M", &from, &target])?;
    let headers = counterparts::scan(std::path::Path::new(env!("CARGO_MANIFEST_DIR")))?;

    // `M path`, `A path`, `D path`, or `R095 old new`: a rename is reported
    // under its old path, the one headers name.
    let changed: Vec<(char, &str, Option<&str>)> = diff
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let status = fields.next()?.chars().next()?;
            let path = fields.next()?;
            Some((status, path, fields.next()))
        })
        .filter(|(_, path, new)| {
            counterparts::in_scope(path) || new.is_some_and(counterparts::in_scope)
        })
        .collect();

    let describe = |status: char, path: &str, new: Option<&str>| match (status, new) {
        ('R', Some(new)) => format!("R  {path} -> {new}"),
        _ => format!("{status}  {path}"),
    };

    let mut by_module: BTreeMap<&std::path::Path, Vec<String>> = BTreeMap::new();
    let mut unclaimed = Vec::new();
    for &(status, path, new) in &changed {
        let modules: BTreeSet<&std::path::Path> = headers
            .iter()
            .filter(|c| counterparts::covers(&c.path, path))
            .map(|c| c.module.as_path())
            .collect();
        if modules.is_empty() {
            unclaimed.push(describe(status, path, new));
        }
        for module in modules {
            by_module
                .entry(module)
                .or_default()
                .push(describe(status, path, new));
        }
    }

    println!(
        "{} server file(s) changed from {from} to {target}; {} Eunha module(s) name one.\n",
        changed.len(),
        by_module.len()
    );
    for (module, files) in &by_module {
        println!("{}", module.display());
        for file in files {
            println!("    {file}");
        }
    }
    if !unclaimed.is_empty() {
        println!(
            "\nNo module names these {}, so Eunha does not implement them:",
            unclaimed.len()
        );
        for file in &unclaimed {
            println!("    {file}");
        }
    }
    println!("\nDiff: https://github.com/mastodon/mastodon/compare/{from}...{target}");
    Ok(())
}

/// Eunha's own migrations, relative to the crate rather than the cwd.
fn migrations_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations")
}
