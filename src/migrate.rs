//! Applying and checking eunha's own migrations.
//!
//! Migrations are embedded in the binary, but running them is a separate act
//! from serving: a migration that rewrites a large table takes as long as it
//! takes, and doing that during startup means the instance is down for the
//! duration and a deploy's health check may kill the process midway through.
//! Some of them are destructive besides — 4.7's account merge deletes rows —
//! and that is not something to do as a side effect of a restart.
//!
//! So `eunha migrate` applies them and exits, and startup only *checks*: an
//! instance whose schema is behind its binary refuses to serve rather than
//! quietly running queries against a shape that no longer matches.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use sqlx::{migrate::Migrator, PgPool};

use crate::{config::DatabasePoolConfig, schema_check, tenants, upstream};

/// The migrations compiled into this binary.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Apply every migration that has not been applied yet.
pub async fn run(db: &PgPool) -> Result<()> {
    // sqlx keeps its ledger in whichever schema comes first on the search path,
    // which the pool sets to `eunha` so that `public` stays a pure mirror of
    // Mastodon's schema.
    sqlx::query("CREATE SCHEMA IF NOT EXISTS eunha")
        .execute(db)
        .await
        .context("creating the eunha schema")?;

    MIGRATOR.run(db).await.context("running migrations")?;
    Ok(())
}

/// What a database is missing relative to the binary asking.
#[derive(Debug)]
pub struct Pending {
    pub versions: Vec<i64>,
}

impl std::fmt::Display for Pending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let list = self
            .versions
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        write!(
            f,
            "{} migration(s) not applied: {list}",
            self.versions.len()
        )
    }
}

/// Which of this binary's migrations the database has not applied.
///
/// A database *ahead* of the binary is not reported: that is a rollback, where
/// the newer schema is generally still readable by the older code, and refusing
/// to start would turn a rollback into an outage.
pub async fn pending(db: &PgPool) -> Result<Option<Pending>> {
    // No ledger at all means nothing has ever been applied.
    let ledger_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
           SELECT 1 FROM pg_class c
           JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE c.relname = '_sqlx_migrations' AND n.nspname = 'eunha'
         )",
    )
    .fetch_one(db)
    .await
    .context("looking for the migration ledger")?;

    if !ledger_exists {
        return Ok(Some(Pending {
            versions: MIGRATOR.iter().map(|m| m.version).collect(),
        }));
    }

    let applied: std::collections::HashSet<i64> =
        sqlx::query_scalar("SELECT version FROM eunha._sqlx_migrations WHERE success")
            .fetch_all(db)
            .await
            .context("reading the migration ledger")?
            .into_iter()
            .collect();

    let versions: Vec<i64> = MIGRATOR
        .iter()
        .map(|m| m.version)
        .filter(|version| !applied.contains(version))
        .collect();

    Ok((!versions.is_empty()).then_some(Pending { versions }))
}

/// `pg_dump`, `pg_restore` and friends, from `PGBIN` when they are not on PATH.
pub(crate) fn pg_command(name: &str) -> tokio::process::Command {
    let binary = match std::env::var("PGBIN") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join(name),
        _ => PathBuf::from(name),
    };
    tokio::process::Command::new(binary)
}

/// How one table's row count moved.
#[derive(Debug, PartialEq, Eq)]
pub enum TableChange {
    Rows {
        table: String,
        before: i64,
        after: i64,
    },
    Created {
        table: String,
        rows: i64,
    },
    Dropped {
        table: String,
        rows: i64,
    },
}

impl std::fmt::Display for TableChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rows {
                table,
                before,
                after,
            } => {
                let delta = after - before;
                let sign = if delta > 0 { "+" } else { "" };
                write!(f, "{table}: {before} -> {after} ({sign}{delta})")
            }
            Self::Created { table, rows } => write!(f, "{table}: created, {rows} row(s)"),
            Self::Dropped { table, rows } => write!(f, "{table}: dropped, held {rows} row(s)"),
        }
    }
}

/// What a rehearsal found.
#[derive(Debug)]
pub struct Rehearsal {
    /// The clone it ran against, left in place unless asked to drop it.
    pub clone: String,
    /// Migrations that were pending, and therefore ran.
    pub applied: Vec<i64>,
    /// How long applying them took, which is how long the instance is down for.
    pub elapsed: Duration,
    /// Tables counted before and after.
    pub tables: usize,
    /// Every table whose row count moved.
    pub changed: Vec<TableChange>,
    /// How the resulting schema departs from the Mastodon release this binary
    /// builds, if at all.
    pub findings: Vec<schema_check::Finding>,
}

/// Compare two sets of row counts.
///
/// A migration is expected to change the schema; what wants looking at is data
/// that moved, so this reports only what differs and says how.
pub fn changes(before: &BTreeMap<String, i64>, after: &BTreeMap<String, i64>) -> Vec<TableChange> {
    let mut changed = Vec::new();
    for (table, before_rows) in before {
        match after.get(table) {
            Some(after_rows) if after_rows == before_rows => {}
            Some(after_rows) => changed.push(TableChange::Rows {
                table: table.clone(),
                before: *before_rows,
                after: *after_rows,
            }),
            None => changed.push(TableChange::Dropped {
                table: table.clone(),
                rows: *before_rows,
            }),
        }
    }
    for (table, rows) in after {
        if !before.contains_key(table) {
            changed.push(TableChange::Created {
                table: table.clone(),
                rows: *rows,
            });
        }
    }
    changed
}

/// A database name that is safe to hand to `createdb` and to name in a URL.
pub fn is_safe_database_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Every table in the schemas eunha owns, with its row count.
///
/// One statement rather than one per table: `query_to_xml` runs the count
/// inside the same scan of the catalogue, so a database with a thousand tables
/// still costs one round trip. The migration ledger is left out — it gains a
/// row for each migration applied, which is the rehearsal itself rather than
/// anything the rehearsal is looking for.
async fn row_counts(db: &PgPool) -> Result<BTreeMap<String, i64>> {
    let rows = sqlx::query_as::<_, (String, Option<i64>)>(
        r#"SELECT n.nspname || '.' || c.relname,
                  (xpath('/row/cnt/text()',
                         query_to_xml(format('SELECT count(*) AS cnt FROM %I.%I', n.nspname, c.relname),
                                      false, true, '')))[1]::text::bigint
           FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
           WHERE n.nspname IN ('public', 'eunha')
             AND c.relkind = 'r'
             AND c.relname <> '_sqlx_migrations'
           ORDER BY 1"#,
    )
    .fetch_all(db)
    .await
    .context("counting rows")?;
    Ok(rows
        .into_iter()
        .map(|(table, count)| (table, count.unwrap_or(0)))
        .collect())
}

/// Clone `source` and run this binary's pending migrations over the copy.
///
/// Migrations get one attempt. The schema they are written against is known,
/// but the data is not: an instance's rows are its own, and a migration that
/// passes every test can still meet something in production that no fixture
/// contained. The way to find that out is to run it on the data, somewhere it
/// does not matter. The source is only ever read from.
///
/// Doing this here rather than from a shell script removes the two things such
/// a script has to be told and can be told wrong. It needs no sqlx CLI and no
/// path to `migrations/`, because the migrations are the ones compiled into
/// this binary — the same ones the server would run. And it does not
/// reconstruct `search_path` in a connection string: the pool sets it, as it
/// does everywhere else. Getting that second one wrong does not fail. It makes
/// sqlx miss the ledger and rehearse every migration ever written.
pub async fn rehearse(source: &str, clone_name: &str, replace: bool) -> Result<Rehearsal> {
    anyhow::ensure!(
        is_safe_database_name(clone_name),
        "'{clone_name}' is not a usable database name: lower-case letters, digits and underscores, \
         not starting with a digit"
    );
    let mut clone_url = url::Url::parse(source).context("the source is not a database URL")?;
    anyhow::ensure!(
        clone_url.path().trim_start_matches('/') != clone_name,
        "the clone would be the source database itself"
    );
    clone_url.set_path(clone_name);

    // Unlike the script this replaces, an existing database is not dropped
    // because its name was reused: a rehearsal is a thing you run twice, and
    // the second run should not be what destroys the first one's evidence — or,
    // with a name typed wrong, something else entirely.
    if database_exists(source, clone_name).await? {
        anyhow::ensure!(
            replace,
            "a database called '{clone_name}' already exists; drop it, choose another name with \
             --clone-name, or pass --replace"
        );
        run_tool("dropdb", &["--if-exists", clone_name], source).await?;
    }

    let dump = std::env::temp_dir().join(format!("eunha-rehearsal-{}.dump", uuid::Uuid::new_v4()));
    tracing::info!("cloning into '{clone_name}'; the source is only read");
    let dumped = pg_command("pg_dump")
        .args(["-Fc", "-Z1", "-d"])
        .arg(source)
        .arg("-f")
        .arg(&dump)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("running pg_dump; is PostgreSQL's bin directory on PATH, or PGBIN set?")?;
    let cleanup = || {
        let dump = dump.clone();
        async move {
            let _ = tokio::fs::remove_file(&dump).await;
        }
    };
    if !dumped.status.success() {
        cleanup().await;
        bail!(
            "could not read the source database: {}",
            String::from_utf8_lossy(&dumped.stderr).trim()
        );
    }

    let restored = async {
        run_tool("createdb", &[clone_name], source).await?;
        let restored = pg_command("pg_restore")
            .args(["-j4", "--no-owner", "--no-privileges", "-d"])
            .arg(clone_url.as_str())
            .arg(&dump)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .context("running pg_restore")?;
        anyhow::ensure!(
            restored.status.success(),
            "could not restore into the clone: {}",
            String::from_utf8_lossy(&restored.stderr).trim()
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    cleanup().await;
    restored?;

    // The pool sets `search_path`, which is what makes the ledger findable
    // without anyone having to say so in the URL.
    let db = tenants::connect(
        clone_url.as_str(),
        &DatabasePoolConfig {
            max_connections: 1,
            ..Default::default()
        },
    )
    .await?;

    let applied = pending(&db)
        .await?
        .map(|pending| pending.versions)
        .unwrap_or_default();
    let before = row_counts(&db).await?;
    tracing::info!("applying {} pending migration(s)", applied.len());
    let started = Instant::now();
    run(&db).await?;
    let elapsed = started.elapsed();
    let after = row_counts(&db).await?;

    let live = schema_check::introspect(&db).await?;
    let findings = schema_check::diff(&live, &upstream::reference_schema());
    db.close().await;

    Ok(Rehearsal {
        clone: clone_name.to_owned(),
        applied,
        elapsed,
        tables: after.len(),
        changed: changes(&before, &after),
        findings,
    })
}

/// Drop the clone a rehearsal left behind.
pub async fn drop_clone(source: &str, clone_name: &str) -> Result<()> {
    anyhow::ensure!(
        is_safe_database_name(clone_name),
        "'{clone_name}' is not a usable database name"
    );
    run_tool("dropdb", &["--if-exists", clone_name], source).await
}

/// Whether the server behind `source` already has a database by this name.
async fn database_exists(source: &str, name: &str) -> Result<bool> {
    let mut url = url::Url::parse(source)?;
    url.set_path("postgres");
    let db = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(url.as_str())
        .await
        .context("connecting to the source server")?;
    let exists =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname=$1)")
            .bind(name)
            .fetch_one(&db)
            .await?;
    db.close().await;
    Ok(exists)
}

/// Run `createdb`/`dropdb` against the server `source` names, since those take
/// a connection's worth of flags rather than a database URL.
async fn run_tool(tool: &str, args: &[&str], source: &str) -> Result<()> {
    let mut url = url::Url::parse(source)?;
    url.set_path("postgres");
    let output = pg_command(tool)
        .arg("--maintenance-db")
        .arg(url.as_str())
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("running {tool}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{tool} failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod rehearsal_tests {
    use super::*;

    fn counts(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
        pairs
            .iter()
            .map(|(table, rows)| ((*table).to_owned(), *rows))
            .collect()
    }

    #[test]
    fn only_tables_whose_rows_moved_are_reported() {
        let before = counts(&[("public.accounts", 10), ("public.statuses", 5)]);
        let after = counts(&[("public.accounts", 10), ("public.statuses", 4)]);
        assert_eq!(
            changes(&before, &after),
            vec![TableChange::Rows {
                table: "public.statuses".into(),
                before: 5,
                after: 4,
            }]
        );
        assert!(changes(&before, &before).is_empty());
    }

    #[test]
    fn a_table_a_migration_adds_or_removes_is_not_a_silent_zero() {
        let before = counts(&[("public.old", 3)]);
        let after = counts(&[("public.new", 7)]);
        let changed = changes(&before, &after);
        assert!(changed.contains(&TableChange::Dropped {
            table: "public.old".into(),
            rows: 3
        }));
        assert!(changed.contains(&TableChange::Created {
            table: "public.new".into(),
            rows: 7
        }));
    }

    #[test]
    fn a_loss_reads_as_a_loss() {
        assert_eq!(
            TableChange::Rows {
                table: "public.statuses".into(),
                before: 88214,
                after: 88210,
            }
            .to_string(),
            "public.statuses: 88214 -> 88210 (-4)"
        );
        assert_eq!(
            TableChange::Rows {
                table: "public.keypairs".into(),
                before: 0,
                after: 12,
            }
            .to_string(),
            "public.keypairs: 0 -> 12 (+12)"
        );
    }

    #[test]
    fn a_clone_name_has_to_be_one_postgres_and_a_url_both_accept() {
        assert!(is_safe_database_name("rehearsal_20260921_084000"));
        assert!(is_safe_database_name("_scratch"));
        assert!(!is_safe_database_name(""));
        assert!(!is_safe_database_name("9lives"));
        assert!(!is_safe_database_name("Mixed"));
        assert!(!is_safe_database_name("drop table; --"));
        assert!(!is_safe_database_name("a/b"));
    }
}
