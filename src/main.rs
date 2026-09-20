use anyhow::Context;
use clap::{Parser, Subcommand};
use eunha::{accounts, config, import, migrate, software_updates, tenants};
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
