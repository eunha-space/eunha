use anyhow::Context;
use clap::{Parser, Subcommand};
use eunha::{accounts, config, import, migrate, software_updates, telemetry, tenants, version};
use std::{future::IntoFuture as _, path::PathBuf, sync::Arc};
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

    /// Optional loopback-only Prometheus listener, separate from tenant HTTP routing.
    #[arg(long, value_name = "ADDRESS", global = true)]
    metrics_bind_address: Option<String>,

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
    ///
    /// A database that was serving before eunha read the site settings from
    /// the database alone then gets `eunha settings import-config`, once: what
    /// its instance configuration says about the site is copied into the
    /// settings nobody has saved yet, so that the upgrade keeps serving it.
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
    /// Manage the site settings Mastodon keeps in its database.
    Settings {
        #[command(subcommand)]
        command: SettingsCommand,
    },
    /// Manage full-text search, as `tootctl search` does.
    Search {
        #[command(subcommand)]
        command: SearchCommand,
    },
    /// Rewrite the addresses local accounts and posts were given under a
    /// domain the instance had before.
    ///
    /// Mastodon stores the ids it has handed out, a post's among them, so
    /// after a domain change they still name the old domain, and the new one
    /// would serve posts other servers refuse for claiming to be from
    /// elsewhere. Run before the instance is served under `--to`; running it
    /// again changes nothing.
    RenameDomain {
        /// The domain the addresses were minted under.
        #[arg(long, value_name = "DOMAIN")]
        from: String,
        /// The domain the instance has now.
        #[arg(long, value_name = "DOMAIN")]
        to: String,
        /// With `--tenants`, the instance, by its domain or one of its
        /// aliases.
        #[arg(long, value_name = "HOST")]
        instance: Option<String>,
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
enum SettingsCommand {
    /// Copy the site's title, descriptions, contact address, registrations,
    /// privacy policy and terms of service from the instance configuration
    /// into the settings and terms tables, where nothing is saved yet.
    ///
    /// Eunha used to take these from the configuration until an administrator
    /// saved them; it now reads only the database, as Mastodon does. `eunha
    /// migrate` runs this once for an instance that was already serving;
    /// remove the keys from the configuration afterwards. Running it again
    /// changes nothing.
    ImportConfig {
        /// Report what would be written, writing nothing.
        #[arg(long)]
        dry_run: bool,
        /// With `--tenants`, the instance, by its domain or one of its
        /// aliases.
        #[arg(long, value_name = "HOST")]
        instance: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum SearchCommand {
    /// Create or upgrade the Elasticsearch indexes and fill them from the
    /// database, as `tootctl search deploy` does.
    ///
    /// An index that does not exist yet, or whose mapping or analysis differs
    /// from what this binary creates, is created afresh, which erases what it
    /// held. Then every row is imported, unless `--no-import`, and documents
    /// whose row is gone are deleted, unless `--no-clean`.
    Deploy {
        /// Batches written at once.
        #[arg(short = 'c', long, default_value_t = 5)]
        concurrency: usize,
        /// Rows in each batch.
        #[arg(short = 'b', long, default_value_t = 100)]
        batch_size: i64,
        /// Only these indexes: instances, accounts, tags, statuses,
        /// public_statuses. Comma-separated, or given more than once.
        #[arg(long, value_delimiter = ',', value_parser = parse_index)]
        only: Vec<eunha::search::elasticsearch::Index>,
        /// Do not import data from the database into the indexes.
        #[arg(long)]
        no_import: bool,
        /// Do not remove documents whose row is gone.
        #[arg(long)]
        no_clean: bool,
        /// Update a changed index's mapping and analysis in place, without
        /// re-creating it or importing anything.
        #[arg(long)]
        only_mapping: bool,
        /// Carry on an interrupted import from the last batch it wrote,
        /// instead of from the first row.
        #[arg(long)]
        resume: bool,
        /// With `--tenants`, the instance, by its domain or one of its
        /// aliases.
        #[arg(long, value_name = "HOST")]
        instance: Option<String>,
    },
}

fn parse_index(name: &str) -> Result<eunha::search::elasticsearch::Index, String> {
    eunha::search::elasticsearch::Index::from_option(name).ok_or_else(|| {
        format!("{name} is not one of instances, accounts, tags, statuses, public_statuses")
    })
}

#[derive(Subcommand, Debug)]
enum AccountsCommand {
    /// Create a new user account, and print the random password it was given.
    ///
    /// `tootctl accounts create`: sign-ups need not be open. Without
    /// `--confirmed` the account waits for its owner to follow the link mailed
    /// to them.
    Create {
        username: String,
        #[arg(long)]
        email: String,
        /// Mark the e-mail as confirmed instead of mailing a link, so that the
        /// account is active straight away.
        #[arg(long)]
        confirmed: bool,
        /// Give the account this role, by name — for example `Owner`.
        #[arg(long)]
        role: Option<String>,
        /// Give the new user the existing account with this username, whose
        /// user is gone, as a deleted account's is.
        #[arg(long)]
        reattach: bool,
        /// With `--reattach`, delete the user still holding the account first,
        /// and the account with it.
        #[arg(long)]
        force: bool,
        /// Approve the account even where sign-ups need approval.
        #[arg(long)]
        approve: bool,
        /// With `--tenants`, the instance to create the account on, by its
        /// domain or one of its aliases.
        #[arg(long, value_name = "HOST")]
        instance: Option<String>,
    },
    /// Send accounts' profiles again to the servers that know them.
    ///
    /// What editing a profile does, for many accounts at once: each goes out
    /// as an `Update` of its actor to its followers' servers and the others
    /// that know it. Other servers keep what they last saw of an account, its
    /// avatar and header URLs among it; after an instance's media has moved,
    /// or anything else about its accounts has changed without an edit, this
    /// is what tells them. Deliveries are queued for the running server to
    /// send.
    Update {
        /// Every local account that is not suspended or being deleted.
        #[arg(
            long,
            conflicts_with = "username",
            required_unless_present = "username"
        )]
        all: bool,
        /// This account; may be given more than once.
        #[arg(long)]
        username: Vec<String>,
        /// Say what would be sent, and to how many inboxes, sending nothing.
        #[arg(long)]
        dry_run: bool,
        /// Give up on a delivery that has not gone through by then, so that
        /// a server that is down or too slow does not hold the batch open:
        /// `90s`, `30m`, `1h`, `2d`.
        #[arg(long, value_name = "DURATION", default_value = "1h", value_parser = parse_duration)]
        give_up_after: std::time::Duration,
        /// Stay until every delivery has gone through or been given up on,
        /// then list what was given up on.
        #[arg(long)]
        wait: bool,
        /// With `--tenants`, the instance, by its domain or one of its
        /// aliases.
        #[arg(long, value_name = "HOST")]
        instance: Option<String>,
    },
    /// Move accounts' followers from a domain the instance had before.
    ///
    /// After an instance's domain changes, its followers' servers still
    /// follow its accounts' actors under the old domain. This sends each of
    /// them a `Move` from the old actor to the new one, signed with the old
    /// actor's key id: they hold that key from when they followed, so nothing
    /// has to be served on the old domain. Each server fetches the new actor,
    /// finds the old one in its `alsoKnownAs`, and follows it. The old domain
    /// has to be in `instance.previous_domains` for that, and the new domain
    /// serving, before this runs. Posts stay where they were.
    Move {
        /// The domain the accounts had.
        #[arg(long, value_name = "DOMAIN")]
        from: String,
        /// Every local account that is not suspended or being deleted.
        #[arg(
            long,
            conflicts_with = "username",
            required_unless_present = "username"
        )]
        all: bool,
        /// This account; may be given more than once.
        #[arg(long)]
        username: Vec<String>,
        /// Say what would be sent, and to how many inboxes, sending nothing.
        #[arg(long)]
        dry_run: bool,
        /// Give up on a delivery that has not gone through by then, so that
        /// a server that is down or too slow does not hold the batch open:
        /// `90s`, `30m`, `1h`, `2d`.
        #[arg(long, value_name = "DURATION", default_value = "1h", value_parser = parse_duration)]
        give_up_after: std::time::Duration,
        /// Stay until every delivery has gone through or been given up on,
        /// then list what was given up on.
        #[arg(long)]
        wait: bool,
        /// With `--tenants`, the instance, by its domain or one of its
        /// aliases.
        #[arg(long, value_name = "HOST")]
        instance: Option<String>,
    },
    /// Where a batch sent by `accounts update` or `accounts move` stands.
    BatchStatus {
        /// The batch's tag, as the command that sent it printed it.
        tag: String,
        /// With `--tenants`, the instance, by its domain or one of its
        /// aliases.
        #[arg(long, value_name = "HOST")]
        instance: Option<String>,
    },
    /// Modify a user account, as `tootctl accounts modify` does.
    Modify {
        username: String,
        /// Give the user this role, by name.
        #[arg(long)]
        role: Option<String>,
        /// Take the user's role away, leaving it an ordinary member.
        #[arg(long)]
        remove_role: bool,
        /// Change the user's address, once the link mailed to it is followed
        /// (or at once, with `--confirm`).
        #[arg(long)]
        email: Option<String>,
        /// Mark the user's address as confirmed.
        #[arg(long)]
        confirm: bool,
        /// Let a disabled user sign in again.
        #[arg(long)]
        enable: bool,
        /// Lock the user out of their account.
        #[arg(long)]
        disable: bool,
        /// Approve a user awaiting approval.
        #[arg(long)]
        approve: bool,
        /// Turn the user's two-factor authentication off.
        #[arg(long = "disable-2fa")]
        disable_2fa: bool,
        /// Give the account a new random password, print it, and sign the
        /// account out of every session and app.
        #[arg(long)]
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

    match eunha::open_files::raise() {
        Ok((before, after)) if after < eunha::open_files::COMFORTABLE => tracing::warn!(
            before,
            after,
            "the open-file limit is low for delivering to many servers at once; raise the hard limit"
        ),
        Ok((before, after)) if after > before => {
            tracing::debug!(before, after, "raised the open-file limit")
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "could not raise the open-file limit"),
    }

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
                    reattach,
                    force,
                    approve,
                    instance,
                },
        }) => {
            let config = command_config(args.tenants.as_deref(), instance.as_deref())?;
            let state = command_state(config).await?;
            let created = accounts::create_from_command(
                &state,
                accounts::CreateOptions {
                    username,
                    email,
                    role,
                    confirmed,
                    approve,
                    reattach,
                    force,
                },
            )
            .await?;
            match created {
                accounts::Created::Account(password) => {
                    println!("OK");
                    println!("New password: {password}");
                }
                accounts::Created::UsernameInUse => {
                    println!("The chosen username is currently in use");
                    println!("Use --force to reattach it anyway and delete the other user");
                }
            }
            return Ok(());
        }
        Some(Command::Accounts {
            command:
                AccountsCommand::Modify {
                    username,
                    role,
                    remove_role,
                    email,
                    confirm,
                    enable,
                    disable,
                    approve,
                    disable_2fa,
                    reset_password,
                    instance,
                },
        }) => {
            let config = command_config(args.tenants.as_deref(), instance.as_deref())?;
            let state = command_state(config).await?;
            let password = accounts::modify_from_command(
                &state,
                &username,
                accounts::ModifyOptions {
                    role,
                    remove_role,
                    email,
                    confirm,
                    enable,
                    disable,
                    approve,
                    disable_2fa,
                    reset_password,
                },
            )
            .await?;
            println!("OK");
            if let Some(password) = password {
                println!("New password: {password}");
            }
            return Ok(());
        }
        Some(Command::Accounts {
            command:
                AccountsCommand::Update {
                    all,
                    username,
                    dry_run,
                    give_up_after,
                    wait,
                    instance,
                },
        }) => {
            let config = command_config(args.tenants.as_deref(), instance.as_deref())?;
            let db = command_database(&config).await?;
            let state = eunha::state::AppState::new(db, config).await?;
            let selection = if all {
                accounts::Selection::All
            } else {
                accounts::Selection::Usernames(username)
            };
            let (tag, batch) = batch("update", give_up_after);
            let report = accounts::update_profiles(&state, &selection, &batch, dry_run).await?;
            finish_batch(&state.db, &tag, &report, dry_run, wait).await?;
            return Ok(());
        }
        Some(Command::Accounts {
            command:
                AccountsCommand::Move {
                    from,
                    all,
                    username,
                    dry_run,
                    give_up_after,
                    wait,
                    instance,
                },
        }) => {
            let config = command_config(args.tenants.as_deref(), instance.as_deref())?;
            let db = command_database(&config).await?;
            let state = eunha::state::AppState::new(db, config).await?;
            let selection = if all {
                accounts::Selection::All
            } else {
                accounts::Selection::Usernames(username)
            };
            let (tag, batch) = batch("move", give_up_after);
            let report =
                accounts::move_followers(&state, &selection, &from, &batch, dry_run).await?;
            finish_batch(&state.db, &tag, &report, dry_run, wait).await?;
            return Ok(());
        }
        Some(Command::Accounts {
            command: AccountsCommand::BatchStatus { tag, instance },
        }) => {
            let config = command_config(args.tenants.as_deref(), instance.as_deref())?;
            let db = command_database(&config).await?;
            print_status(&accounts::batch_status(&db, &tag).await?);
            return Ok(());
        }
        Some(Command::Search {
            command:
                SearchCommand::Deploy {
                    concurrency,
                    batch_size,
                    only,
                    no_import,
                    no_clean,
                    only_mapping,
                    resume,
                    instance,
                },
        }) => {
            let config = command_config(args.tenants.as_deref(), instance.as_deref())?;
            let db = command_database_sized(&config, concurrency as u32 + 1).await?;
            let state = eunha::state::AppState::new(db, config).await?;
            let options = eunha::search::elasticsearch::deploy::Options {
                concurrency,
                batch_size,
                only,
                import: !no_import,
                clean: !no_clean,
                only_mapping,
                resume,
            };
            eunha::search::elasticsearch::deploy::deploy(&state, &options, |line| {
                println!("{line}")
            })
            .await?;
            return Ok(());
        }
        Some(Command::Settings {
            command: SettingsCommand::ImportConfig { dry_run, instance },
        }) => {
            let config = command_config(args.tenants.as_deref(), instance.as_deref())?;
            let db = command_database(&config).await?;
            let report =
                eunha::settings_import::import_config(&db, &config.instance, dry_run).await?;
            print_settings_import("", &report, dry_run);
            println!("OK");
            return Ok(());
        }
        Some(Command::RenameDomain { from, to, instance }) => {
            let config = command_config(args.tenants.as_deref(), instance.as_deref())?;
            let db = command_database(&config).await?;
            import::rename(&db, &from, &to).await?;
            println!("OK");
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
    let metrics_server =
        telemetry::start(args.metrics_bind_address.as_deref(), tenants.clone()).await?;
    let app = tenants.router();

    let listener = tokio::net::TcpListener::bind(&bind_address).await?;
    tracing::info!(tenants = serving, "listening on {bind_address}");
    // The peer address, for `remote_ip` to start from.
    tokio::select! {
        result = axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).into_future() => { result?; }
        result = async {
            match metrics_server {
                Some((listener, router)) => axum::serve(listener, router).await,
                None => std::future::pending().await,
            }
        } => { result?; anyhow::bail!("private metrics listener stopped"); }
    }

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
    type Target = (String, String, anyhow::Result<config::InstanceConfig>);
    let targets: Vec<Target> = match tenants {
        Some(dir) => tenants::load_dir(dir)?
            .into_iter()
            .map(|tenant| {
                (
                    format!("{}: ", tenant.source),
                    tenant.config.database_url,
                    Ok(tenant.config.instance),
                )
            })
            .collect(),
        None => vec![(
            String::new(),
            migration_database_url()?,
            config::Config::instance_from_env(),
        )],
    };

    let mut behind = false;
    for (label, database_url, instance) in targets {
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
                // The one-time `eunha settings import-config` an instance that
                // was serving before eunha read the settings alone is owed, so
                // that no deploy has to remember it.
                if eunha::settings_import::owed(&db).await? {
                    match instance {
                        Ok(instance) => {
                            if let Some(report) =
                                eunha::settings_import::import_if_owed(&db, &instance).await?
                            {
                                print_settings_import(&label, &report, false);
                                println!("{label}Site settings imported from the configuration.");
                            }
                        }
                        Err(error) => println!(
                            "{label}Site settings not imported yet, for want of an [instance] \
                             configuration ({error:#}); the next `eunha migrate` that can read \
                             it imports them, as `eunha settings import-config` does."
                        ),
                    }
                }
            }
        }
    }
    if behind {
        std::process::exit(1);
    }
    Ok(())
}

/// What `eunha settings import-config` wrote, kept, or would write.
fn print_settings_import(label: &str, report: &eunha::settings_import::Report, dry_run: bool) {
    for var in &report.written {
        println!(
            "{label}{} {var}",
            if dry_run { "would write" } else { "wrote" }
        );
    }
    for var in &report.kept {
        println!("{label}kept saved {var}");
    }
    if report.terms_published {
        println!(
            "{label}{} the configured terms of service, effective 2025-01-01",
            if dry_run {
                "would publish"
            } else {
                "published"
            }
        );
    }
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
/// Print what a batch of account updates did, and fail when it named
/// accounts that are not there.
fn print_batch(report: &accounts::BatchReport, dry_run: bool) -> anyhow::Result<()> {
    let verb = if dry_run { "would queue" } else { "queued" };
    for (name, inboxes) in &report.sent {
        println!("{name}: {verb} for {inboxes} inboxes");
    }
    for (name, why) in &report.skipped {
        println!("{name}: skipped, {why}");
    }
    for name in &report.unknown {
        println!("{name}: no such local account");
    }
    let total: u64 = report.sent.iter().map(|(_, inboxes)| inboxes).sum();
    println!(
        "{} accounts {verb} for {total} inboxes, {} skipped",
        report.sent.len(),
        report.skipped.len()
    );
    anyhow::ensure!(
        report.unknown.is_empty(),
        "some usernames are not local accounts"
    );
    Ok(())
}

/// A new batch of `kind`, tagged to be followed, given up on after
/// `give_up_after`.
fn batch(kind: &str, give_up_after: std::time::Duration) -> (String, ojak::deliverer::Batch) {
    let tag = format!("{kind}:{}", eunha::snowflake::next_id());
    let batch = ojak::deliverer::Batch {
        tag: Some(tag.clone()),
        deadline: Some(std::time::SystemTime::now() + give_up_after),
        ..ojak::deliverer::Batch::default()
    };
    (tag, batch)
}

/// Report a batch, and with `wait`, follow it until nothing is pending.
async fn finish_batch(
    db: &sqlx::PgPool,
    tag: &str,
    report: &accounts::BatchReport,
    dry_run: bool,
    wait: bool,
) -> anyhow::Result<()> {
    print_batch(report, dry_run)?;
    if dry_run {
        return Ok(());
    }
    println!("Batch {tag}; `eunha accounts batch-status {tag}` says where it stands");
    if !wait {
        return Ok(());
    }
    let mut last = None;
    loop {
        let status = accounts::batch_status(db, tag).await?;
        let now = (status.pending, status.failed.len());
        if last != Some(now) {
            println!("{} pending, {} given up on", now.0, now.1);
            last = Some(now);
        }
        if status.pending == 0 {
            print_status(&status);
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

fn print_status(status: &accounts::BatchStatus) {
    println!(
        "{} pending, {} given up on",
        status.pending,
        status.failed.len()
    );
    for (inbox, error) in &status.failed {
        let error: String = error
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(120)
            .collect();
        println!("  {inbox}: {error}");
    }
}

/// `90s`, `30m`, `1h` or `2d`; a bare number is seconds.
fn parse_duration(text: &str) -> Result<std::time::Duration, String> {
    let text = text.trim();
    let (number, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(split) => text.split_at(split),
        None => (text, "s"),
    };
    let number: u64 = number
        .parse()
        .map_err(|_| format!("{text:?} is not a duration such as 30m or 1h"))?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(format!("{text:?} is not a duration such as 30m or 1h")),
    };
    Ok(std::time::Duration::from_secs(number * seconds))
}

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

/// The instance a one-off command acts for, as a server would: its mail goes
/// into the job queue for the running server to send, and what it publishes
/// reaches that server's streams through Redis.
async fn command_state(config: config::Config) -> anyhow::Result<eunha::state::AppState> {
    let db = command_database_sized(&config, 4).await?;
    eunha::state::AppState::new(db, config).await
}

/// A one-off command's connection to its instance's database, once the schema
/// is known to match this binary.
async fn command_database(config: &config::Config) -> anyhow::Result<sqlx::PgPool> {
    command_database_sized(config, 1).await
}

/// [`command_database`] with room for `connections` queries at once.
async fn command_database_sized(
    config: &config::Config,
    connections: u32,
) -> anyhow::Result<sqlx::PgPool> {
    let db = tenants::connect(
        &config.database_url,
        &config::DatabasePoolConfig {
            max_connections: connections.max(1),
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
