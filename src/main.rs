use anyhow::Context;
use clap::{Parser, Subcommand};
use eunha::{accounts, config, import, migrate, software_updates, tenants, version};
use std::{path::PathBuf, sync::Arc};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser, Debug)]
#[command(name = "eunha", about = "A Mastodon-compatible ActivityPub server")]
struct Args {
    /// Serve every instance configured in this directory — one `*.toml` per
    /// instance, each answering to its `instance.domain` — from this one
    /// process. Without it, eunha serves the single instance in `config.toml`
    /// and the environment.
    #[arg(long, value_name = "DIR", global = true)]
    tenants: Option<PathBuf>,

    /// Override the process listener without changing tenant configuration.
    /// Intended for blue/green slots behind a stable local router.
    #[arg(long, value_name = "ADDRESS", global = true)]
    bind_address: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Apply pending database migrations and exit.
    ///
    /// Separate from serving on purpose: a migration takes as long as it takes,
    /// and some of them are destructive. Running them from a deploy script,
    /// before the new binary starts, means a failure is found with the old
    /// version still serving rather than with nothing serving at all.
    Migrate {
        /// Report what is pending without applying anything. Exits non-zero if
        /// a database is behind this binary.
        #[arg(long)]
        check: bool,
    },
    /// Rehearse this binary's pending migrations against a copy of live data.
    ///
    /// A migration gets one attempt against data nobody has tested it on. This
    /// clones a database, applies what is pending the way the server would,
    /// and reports every table whose row count moved plus whether the result
    /// still matches the Mastodon release eunha tracks. The source is only
    /// read from. Run it on the database host, where cloning does not cross a
    /// network.
    RehearseMigration {
        /// The database to copy. Only read from.
        source_database_url: String,
        /// What to call the clone. Defaults to a name with the time in it.
        #[arg(long, value_name = "NAME")]
        clone_name: Option<String>,
        /// Drop an existing database of that name first.
        #[arg(long)]
        replace: bool,
        /// Drop the clone when the rehearsal is done, instead of leaving it to
        /// be looked at.
        #[arg(long)]
        drop_clone: bool,
    },
    /// Manage local accounts, as `tootctl accounts` does.
    Accounts {
        #[command(subcommand)]
        command: AccountsCommand,
    },
    /// Import an existing Mastodon instance into this database.
    ///
    /// Eunha builds the schema of the Mastodon release it tracks, so an
    /// instance moves here by having its data restored into a database this
    /// binary has already migrated. The database must be empty of Mastodon
    /// data and the connection a superuser's, because a data-only restore
    /// loads through the schema's foreign keys.
    ImportMastodon {
        /// A custom-format dump, as `pg_dump -Fc` writes.
        dump: PathBuf,
        /// The domain the imported instance answers to afterwards.
        #[arg(long, value_name = "DOMAIN")]
        domain: String,
        /// The domain the dump was written under, when the instance is moving.
        ///
        /// Remote servers remember an instance's accounts at the domain they
        /// were seen under and will not follow them to a new one, so this
        /// abandons the identity it rewrites. Leave it out to import an
        /// instance as itself.
        #[arg(long, value_name = "DOMAIN")]
        rename_from: Option<String>,
        /// Report what the dump holds and whether it fits, writing nothing.
        #[arg(long)]
        check: bool,
        /// Restore a dump from another Mastodon release anyway.
        #[arg(long)]
        allow_schema_mismatch: bool,
    },
    /// Upload an existing Mastodon instance's media into this instance's
    /// storage.
    ///
    /// Mastodon stores a file's name rather than its address and derives the
    /// object key from the row, so the media has to arrive under the keys the
    /// instance already minted. Its `public/system` tree is laid out by exactly
    /// those keys, which makes this a copy. Each file goes under the prefix the
    /// instance's `media_storage` namespaces its objects with, which is what
    /// keeps instances sharing a bucket apart.
    ImportMedia {
        /// The instance's `public/system` tree, or a copy of its bucket.
        media_dir: PathBuf,
        /// Carry the instance's cache of other servers' media too.
        ///
        /// Most of a Mastodon media directory is usually this: every remote
        /// avatar, attachment, emoji and preview it has shown. It is a copy of
        /// somebody else's file and is fetched again when it is missing, so it
        /// is left behind by default. Carry it to spare the re-fetching, or to
        /// keep copies of media whose origin has since gone.
        #[arg(long)]
        include_cached_remote: bool,
        /// Concurrent uploads.
        #[arg(long, default_value_t = 32)]
        concurrency: usize,
        /// Ask for each object before sending it, and send only what is
        /// missing. This is how an interrupted upload resumes cheaply; a first
        /// run pays a request per file for nothing.
        #[arg(long)]
        skip_existing: bool,
        /// With `--tenants`, the instance to upload into, by its domain or one
        /// of its aliases.
        #[arg(long, value_name = "HOST")]
        instance: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum AccountsCommand {
    /// Create a new user account, and print the random password it was given.
    ///
    /// `tootctl accounts create`: sign-ups need not be open, and the account is
    /// active straight away. eunha has no unconfirmed accounts outside the
    /// sign-up flow, so `--confirmed` is required.
    Create {
        username: String,
        #[arg(long)]
        email: String,
        /// Mark the e-mail as confirmed instead of mailing a link. Required.
        #[arg(long)]
        confirmed: bool,
        /// Give the account this role, by name — for example `Owner`.
        #[arg(long)]
        role: Option<String>,
        /// Approve the account even where sign-ups need approval.
        #[arg(long)]
        approve: bool,
        /// With `--tenants`, the instance to create the account on, by its
        /// domain or one of its aliases.
        #[arg(long, value_name = "HOST")]
        instance: Option<String>,
    },
    /// Modify a user account.
    ///
    /// `tootctl accounts modify`, of which eunha implements `--reset-password`.
    Modify {
        username: String,
        /// Give the account a new random password, print it, and sign the
        /// account out of every session and app.
        #[arg(long, required = true)]
        reset_password: bool,
        /// With `--tenants`, the instance the account is on, by its domain or
        /// one of its aliases.
        #[arg(long, value_name = "HOST")]
        instance: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "eunha=debug,tower_http=info,sqlx=warn".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    match args.command {
        Some(Command::Migrate { check }) => {
            return migrate_databases(args.tenants.as_deref(), check).await;
        }
        Some(Command::Accounts {
            command:
                AccountsCommand::Create {
                    username,
                    email,
                    confirmed,
                    role,
                    approve,
                    instance,
                },
        }) => {
            let config = command_config(args.tenants.as_deref(), instance.as_deref())?;
            let password = create_account(
                config,
                accounts::CreateOptions {
                    username,
                    email,
                    role,
                    confirmed,
                    approve,
                },
            )
            .await?;
            println!("OK");
            println!("New password: {password}");
            return Ok(());
        }
        Some(Command::Accounts {
            command:
                AccountsCommand::Modify {
                    username,
                    reset_password: _,
                    instance,
                },
        }) => {
            let config = command_config(args.tenants.as_deref(), instance.as_deref())?;
            let db = command_database(&config).await?;
            let password = accounts::reset_password(&db, &username).await?;
            println!("OK");
            println!("New password: {password}");
            return Ok(());
        }
        Some(Command::ImportMastodon {
            dump,
            domain,
            rename_from,
            check,
            allow_schema_mismatch,
        }) => {
            return import_mastodon(
                import::Import {
                    dump,
                    domain,
                    rename_from,
                    allow_schema_mismatch,
                },
                check,
            )
            .await;
        }
        Some(Command::ImportMedia {
            media_dir,
            include_cached_remote,
            concurrency,
            skip_existing,
            instance,
        }) => {
            let config = command_config(args.tenants.as_deref(), instance.as_deref())?;
            // Which files are the instance's own is a question only its
            // database answers, so it is read before anything is sent.
            let own = match include_cached_remote {
                true => None,
                false => Some(import::OwnMedia::read(&command_database(&config).await?).await?),
            };
            let uploaded = import::upload_media(
                &config.media_storage,
                &media_dir,
                concurrency,
                skip_existing,
                own.as_ref(),
            )
            .await?;
            println!("OK");
            println!("files: {}", uploaded.total);
            println!("uploaded: {}", uploaded.sent);
            println!("skipped: {}", uploaded.skipped);
            println!("cached elsewhere: {}", uploaded.cached);
            println!("key prefix: {}", uploaded.key_prefix);
            return Ok(());
        }
        Some(Command::RehearseMigration {
            source_database_url,
            clone_name,
            replace,
            drop_clone,
        }) => {
            return rehearse_migration(&source_database_url, clone_name, replace, drop_clone).await;
        }
        None => {}
    }

    let configs = match &args.tenants {
        Some(dir) => tenants::load_dir(dir)?,
        None => vec![tenants::TenantConfig {
            source: "config.toml".to_string(),
            config: config::Config::from_env()?,
        }],
    };
    let tenants = Arc::new(tenants::start(configs).await?);
    let bind_address = args
        .bind_address
        .unwrap_or_else(|| tenants.bind_address().to_string());
    let serving = tenants.states().await.len();
    if let Some(dir) = args.tenants {
        reload_on_hangup(tenants.clone(), dir)?;
    }
    // One check for the process, however many instances it serves.
    tenants::spawn(software_updates::run_for_process(tenants.clone()));
    let app = tenants.router();

    let listener = tokio::net::TcpListener::bind(&bind_address).await?;
    tracing::info!(tenants = serving, "listening on {bind_address}");
    axum::serve(listener, app).await?;

    Ok(())
}

/// Reread the tenants directory each time the process is sent SIGHUP, and serve
/// what it says now: start the tenants added, restart those whose file changed,
/// stop those removed. A directory that could not be served as a whole is
/// refused, and the tenants already running go on as they were.
fn reload_on_hangup(tenants: Arc<tenants::Tenants>, dir: PathBuf) -> anyhow::Result<()> {
    let mut hangups = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    tenants::spawn(async move {
        while hangups.recv().await.is_some() {
            tracing::info!(directory = %dir.display(), "rereading the tenants directory");
            let reloaded = match tenants::load_dir(&dir) {
                Ok(configs) => tenants.reload(configs).await,
                Err(e) => Err(e),
            };
            match reloaded {
                Ok(reloaded) => tracing::info!(
                    started = ?reloaded.started,
                    restarted = ?reloaded.restarted,
                    stopped = ?reloaded.stopped,
                    unavailable = ?reloaded.unavailable,
                    kept = reloaded.kept.len(),
                    "tenants reloaded"
                ),
                Err(e) => tracing::error!(
                    error = %format!("{e:#}"),
                    "tenants not reloaded; the ones running go on as they were"
                ),
            }
        }
    });
    Ok(())
}

/// Migrate, or with `check` report on, the single instance's database or every
/// tenant's in `tenants`.
///
/// Migrating needs a database and nothing else. Loading a single instance's
/// full config would make a deploy script's migrate step fail for want of an S3
/// bucket, which has no bearing on whether the schema can be brought up to
/// date.
async fn migrate_databases(tenants: Option<&std::path::Path>, check: bool) -> anyhow::Result<()> {
    let targets: Vec<(String, String)> = match tenants {
        Some(dir) => tenants::load_dir(dir)?
            .into_iter()
            .map(|tenant| (format!("{}: ", tenant.source), tenant.config.database_url))
            .collect(),
        None => vec![(String::new(), migration_database_url()?)],
    };

    let mut behind = false;
    for (label, database_url) in targets {
        let db = tenants::connect(
            &database_url,
            &config::DatabasePoolConfig {
                max_connections: 1,
                ..Default::default()
            },
        )
        .await?;
        match (check, migrate::pending(&db).await?) {
            (true, None) => println!("{label}Database is up to date."),
            (true, Some(pending)) => {
                println!("{label}{pending}");
                behind = true;
            }
            (false, _) => {
                migrate::run(&db).await?;
                println!("{label}Migrations applied.");
            }
        }
    }
    if behind {
        std::process::exit(1);
    }
    Ok(())
}

/// Rehearse pending migrations against a copy of a live database, and report
/// what they did to it.
async fn rehearse_migration(
    source: &str,
    clone_name: Option<String>,
    replace: bool,
    drop_clone: bool,
) -> anyhow::Result<()> {
    let clone_name = clone_name
        .unwrap_or_else(|| format!("rehearsal_{}", chrono::Local::now().format("%Y%m%d_%H%M%S")));
    let rehearsal = migrate::rehearse(source, &clone_name, replace).await?;

    println!("OK");
    println!(
        "migrations applied: {}{}",
        rehearsal.applied.len(),
        match rehearsal.applied.as_slice() {
            [] => String::new(),
            versions => format!(
                " ({})",
                versions
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    );
    // How long the instance would be down for, which is the number an operator
    // is deciding a maintenance window from.
    println!("took: {:.1?}", rehearsal.elapsed);
    println!("tables: {}", rehearsal.tables);
    match rehearsal.changed.as_slice() {
        [] => println!("rows: no table gained or lost rows"),
        changed => {
            println!("rows: {} table(s) changed", changed.len());
            for change in changed {
                println!("  {change}");
            }
        }
    }
    match rehearsal.findings.as_slice() {
        [] => println!("schema: matches Mastodon {}", version::MASTODON),
        findings => {
            println!(
                "schema: {} difference(s) from Mastodon {}",
                findings.len(),
                version::MASTODON
            );
            for finding in findings {
                println!("  {finding}");
            }
        }
    }

    if drop_clone {
        migrate::drop_clone(source, &rehearsal.clone).await?;
        println!("clone: dropped");
    } else {
        println!("clone: {} — drop it when done", rehearsal.clone);
    }
    // A migration that moved rows it should not, or a schema that no longer
    // matches, is the whole point of rehearsing; say so in the exit code too.
    if !rehearsal.findings.is_empty() {
        std::process::exit(1);
    }
    Ok(())
}

/// Restore a Mastodon dump into this instance's database, or with `check`
/// report on whether it would fit.
///
/// The connection is the one `migrate` uses, for the same reason: an import
/// needs a database and nothing else, and loading a full config would make it
/// fail for want of an S3 bucket that has no bearing on the restore.
async fn import_mastodon(import: import::Import, check: bool) -> anyhow::Result<()> {
    let database_url = migration_database_url()?;
    let db = tenants::connect(
        &database_url,
        &config::DatabasePoolConfig {
            max_connections: 1,
            ..Default::default()
        },
    )
    .await?;

    if check {
        let plan = import::plan(&db, &import).await?;
        println!(
            "dump          Mastodon schema {}",
            plan.dump_schema_version.as_deref().unwrap_or("unrecorded")
        );
        println!(
            "this eunha    Mastodon schema {}",
            plan.expected_schema_version
        );
        for warning in plan.warnings() {
            println!("warning       {warning}");
        }
        let refusals = plan.refusals(&import);
        if refusals.is_empty() {
            println!(
                "OK            the dump can be imported as {}",
                import.domain
            );
            return Ok(());
        }
        for refusal in &refusals {
            println!("refused       {refusal}");
        }
        std::process::exit(1);
    }

    let report = import::run(&db, &database_url, &import).await?;
    println!("OK");
    println!("domain: {}", report.domain);
    if let Some(old) = &report.renamed_from {
        println!("renamed from: {old}");
    }
    for (table, count) in &report.counts {
        println!("{table}: {count}");
    }
    Ok(())
}

/// The configuration a one-off command acts on: the single instance's, or with
/// `tenants` the one tenant answering to `instance`.
fn command_config(
    tenants: Option<&std::path::Path>,
    instance: Option<&str>,
) -> anyhow::Result<config::Config> {
    let Some(dir) = tenants else {
        anyhow::ensure!(
            instance.is_none(),
            "--instance picks a tenant, and needs --tenants"
        );
        return config::Config::from_env();
    };
    let instance =
        instance.context("--tenants serves several instances; pick one with --instance")?;
    tenants::find(tenants::load_dir(dir)?, instance)
        .map(|tenant| tenant.config)
        .with_context(|| format!("no tenant in {} answers to {instance}", dir.display()))
}

async fn create_account(
    config: config::Config,
    options: accounts::CreateOptions,
) -> anyhow::Result<String> {
    let db = command_database(&config).await?;
    let encryptor = config.active_record_encryption.as_ref().map(|keys| {
        eunha::rails_encryption::Encryptor::new(&keys.primary_key, &keys.key_derivation_salt)
    });
    accounts::create_from_command(&db, encryptor.as_ref(), &config.instance, options).await
}

/// A one-off command's connection to its instance's database, once the schema
/// is known to match this binary.
async fn command_database(config: &config::Config) -> anyhow::Result<sqlx::PgPool> {
    let db = tenants::connect(
        &config.database_url,
        &config::DatabasePoolConfig {
            max_connections: 1,
            ..Default::default()
        },
    )
    .await?;
    // An account written into a schema this binary does not match could be
    // missing columns the running server needs; the server refuses to start in
    // that case, and so does this.
    if let Some(pending) = migrate::pending(&db).await? {
        anyhow::bail!("{pending}; run `eunha migrate` first");
    }
    Ok(db)
}

/// The database to migrate: `DATABASE_URL` if set (including from `.env`),
/// otherwise whatever the server would have used.
fn migration_database_url() -> anyhow::Result<String> {
    dotenvy::dotenv().ok();
    if let Ok(url) = std::env::var("DATABASE_URL") {
        if !url.is_empty() {
            return Ok(url);
        }
    }
    Ok(config::Config::from_env()?.database_url)
}
