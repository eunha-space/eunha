//! `tootctl maintenance fix-duplicates` (`Mastodon::CLI::Maintenance`).
//!
//! A unique index built under one collation can hold rows another collation
//! calls equal, and after an operating system upgrade changes the C library's
//! locale data (<https://wiki.postgresql.org/wiki/Locale_data_changes>) the
//! index no longer knows what it holds: it lets duplicates in, and a dump of
//! the database then cannot be restored, because recreating the index fails.
//! This drops each unique index Mastodon knows to be affected, removes or
//! merges the duplicates it hid, and builds the index again.
//!
//! Everything runs on one connection, as the Rails task does, with the
//! statement timeout off.

use anyhow::{bail, Context as _};
use sqlx::{PgConnection, Row as _};

use super::console::{Console, Instance};

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Fix duplicates in database and rebuild indexes, as `tootctl maintenance
    /// fix-duplicates` does.
    ///
    /// Deletes or merges duplicate accounts, statuses, emojis and the rest,
    /// and rebuilds their indexes: for a database whose indexes are corrupt
    /// after its collation changed. Stop every process using the database
    /// first, and have a backup; this takes long and may be destructive. Asks
    /// which local account to keep where two have one username.
    ///
    /// Works on a Mastodon database at the release eunha tracks as well as on
    /// eunha's own, so that a dump `eunha import-mastodon` refuses can be
    /// repaired at its source.
    FixDuplicates {
        #[command(flatten)]
        on: Instance,
    },
}

impl Command {
    /// The instance the command acts on, with `--tenants`.
    pub fn instance(&self) -> Option<&str> {
        match self {
            Self::FixDuplicates { on } => on.host.as_deref(),
        }
    }

    /// Run against `database_url`, which need not have eunha's migrations
    /// applied: a Mastodon database is repaired the same way.
    pub async fn run(self, database_url: &str, console: &dyn Console) -> anyhow::Result<()> {
        match self {
            Self::FixDuplicates { .. } => {
                let pool = crate::tenants::connect(
                    database_url,
                    &crate::config::DatabasePoolConfig {
                        max_connections: 1,
                        ..Default::default()
                    },
                )
                .await?;
                let mut conn = pool.acquire().await?;
                fix_duplicates(&mut conn, console).await
            }
        }
    }
}

/// `Maintenance#fix_duplicates`.
pub async fn fix_duplicates(conn: &mut PgConnection, console: &dyn Console) -> anyhow::Result<()> {
    verify_schema_version(conn, console).await?;
    verify_nothing_running(conn).await?;
    verify_backup_warning(console)?;
    disable_timeout(conn).await?;
    deduplicate(conn, console).await?;
    cleanup(conn).await?;
    console.say("Finished!");
    Ok(())
}

/// `verify_schema_version!`: the database is at the schema this binary
/// builds. An older one is refused; a newer one is gone on with only if the
/// operator says so.
pub async fn verify_schema_version(
    conn: &mut PgConnection,
    console: &dyn Console,
) -> anyhow::Result<()> {
    let newest: Option<String> =
        sqlx::query_scalar("SELECT max(version) FROM public.schema_migrations")
            .fetch_one(&mut *conn)
            .await
            .context("reading schema_migrations; is this a Mastodon database?")?;
    let newest = newest.unwrap_or_default();
    let expected = crate::version::MASTODON_SCHEMA;
    let as_number = |version: &str| version.parse::<u64>().unwrap_or(0);
    if as_number(&newest) < as_number(expected) {
        bail!(
            "Your version of the database schema is too old and is not supported by this \
             script.\nPlease update to at least Mastodon {} before running this script.",
            crate::version::MASTODON
        );
    }
    if as_number(&newest) > as_number(expected) {
        console.say(
            "Your version of the database schema is more recent than this script, this may \
             cause unexpected errors.",
        );
        if !console.yes("Continue anyway? (Yes/No)") {
            bail!("Stopping maintenance script because data is more recent than script version.");
        }
    }
    Ok(())
}

/// `verify_sidekiq_not_active!`, for any process: nothing else may be
/// connected to the database.
pub async fn verify_nothing_running(conn: &mut PgConnection) -> anyhow::Result<()> {
    let others: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity
         WHERE datname = current_database() AND pid <> pg_backend_pid()
           AND backend_type = 'client backend'",
    )
    .fetch_one(&mut *conn)
    .await?;
    if others > 0 {
        bail!(
            "It seems eunha or Mastodon is running: {others} other connection(s) to this \
             database. All processes need to be stopped when using this script."
        );
    }
    Ok(())
}

/// `verify_backup_warning!`.
pub fn verify_backup_warning(console: &dyn Console) -> anyhow::Result<()> {
    console.say("This task will take a long time to run and is potentially destructive.");
    console.say("Please make sure to stop eunha and have a backup.");
    if !console.yes("Continue? (Yes/No)") {
        bail!("Maintenance process stopped.");
    }
    Ok(())
}

/// `disable_timeout!`.
async fn disable_timeout(conn: &mut PgConnection) -> anyhow::Result<()> {
    sqlx::query("SET statement_timeout = 0")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// `deduplication_cleanup_tasks`: the `instances` view is refreshed. There is
/// no `Rails.cache` to clear.
async fn cleanup(conn: &mut PgConnection) -> anyhow::Result<()> {
    let populated: bool = sqlx::query_scalar(
        "SELECT ispopulated FROM pg_matviews WHERE schemaname = 'public' AND matviewname = 'instances'",
    )
    .fetch_optional(&mut *conn)
    .await?
    .unwrap_or(false);
    let sql = if populated {
        "REFRESH MATERIALIZED VIEW CONCURRENTLY public.instances"
    } else {
        "REFRESH MATERIALIZED VIEW public.instances"
    };
    sqlx::query(sql).execute(&mut *conn).await?;
    Ok(())
}

/// `process_deduplications`, in Mastodon's order. Each table's unique index is
/// dropped first and built again afterwards, whether or not the
/// deduplication between went through.
pub async fn deduplicate(conn: &mut PgConnection, console: &dyn Console) -> anyhow::Result<()> {
    users(conn, console).await?;
    account_domain_blocks(conn, console).await?;
    announcement_reactions(conn, console).await?;
    conversations(conn, console).await?;
    custom_emojis(conn, console).await?;
    custom_emoji_categories(conn, console).await?;
    keep_newest(
        conn,
        console,
        "domain_allows",
        "domain",
        INDEX_DOMAIN_ALLOWS,
    )
    .await?;
    domain_blocks(conn, console).await?;
    keep_newest(
        conn,
        console,
        "unavailable_domains",
        "domain",
        INDEX_UNAVAILABLE_DOMAINS,
    )
    .await?;
    email_domain_blocks(conn, console).await?;
    media_attachments(conn, console).await?;
    keep_newest(conn, console, "preview_cards", "url", INDEX_PREVIEW_CARDS).await?;
    statuses(conn, console).await?;
    accounts(conn, console).await?;
    tags(conn, console).await?;
    keep_newest(
        conn,
        console,
        "webauthn_credentials",
        "external_id",
        INDEX_WEBAUTHN_CREDENTIALS,
    )
    .await?;
    keep_newest(conn, console, "webhooks", "url", INDEX_WEBHOOKS).await?;
    // Not bothered with: the update check fetches it again.
    sqlx::query("DELETE FROM software_updates")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

// The unique indexes, as the schema of the release eunha tracks has them.
const INDEX_USERS_CONFIRMATION_TOKEN: (&str, &str) = (
    "index_users_on_confirmation_token",
    "CREATE UNIQUE INDEX index_users_on_confirmation_token ON public.users USING btree (confirmation_token)",
);
const INDEX_USERS_EMAIL: (&str, &str) = (
    "index_users_on_email",
    "CREATE UNIQUE INDEX index_users_on_email ON public.users USING btree (email)",
);
const INDEX_USERS_RESET_PASSWORD_TOKEN: (&str, &str) = (
    "index_users_on_reset_password_token",
    "CREATE UNIQUE INDEX index_users_on_reset_password_token ON public.users USING btree (reset_password_token text_pattern_ops) WHERE (reset_password_token IS NOT NULL)",
);
const INDEX_ACCOUNT_DOMAIN_BLOCKS: (&str, &str) = (
    "index_account_domain_blocks_on_account_id_and_domain",
    "CREATE UNIQUE INDEX index_account_domain_blocks_on_account_id_and_domain ON public.account_domain_blocks USING btree (account_id, domain)",
);
const INDEX_ANNOUNCEMENT_REACTIONS: (&str, &str) = (
    "index_announcement_reactions_on_account_id_and_announcement_id",
    "CREATE UNIQUE INDEX index_announcement_reactions_on_account_id_and_announcement_id ON public.announcement_reactions USING btree (account_id, announcement_id, name)",
);
const INDEX_CONVERSATIONS: (&str, &str) = (
    "index_conversations_on_uri",
    "CREATE UNIQUE INDEX index_conversations_on_uri ON public.conversations USING btree (uri text_pattern_ops) WHERE (uri IS NOT NULL)",
);
const INDEX_CUSTOM_EMOJIS: (&str, &str) = (
    "index_custom_emojis_on_shortcode_and_domain",
    "CREATE UNIQUE INDEX index_custom_emojis_on_shortcode_and_domain ON public.custom_emojis USING btree (shortcode, domain)",
);
const INDEX_CUSTOM_EMOJI_CATEGORIES: (&str, &str) = (
    "index_custom_emoji_categories_on_name",
    "CREATE UNIQUE INDEX index_custom_emoji_categories_on_name ON public.custom_emoji_categories USING btree (name)",
);
const INDEX_DOMAIN_ALLOWS: (&str, &str) = (
    "index_domain_allows_on_domain",
    "CREATE UNIQUE INDEX index_domain_allows_on_domain ON public.domain_allows USING btree (domain)",
);
const INDEX_DOMAIN_BLOCKS: (&str, &str) = (
    "index_domain_blocks_on_domain",
    "CREATE UNIQUE INDEX index_domain_blocks_on_domain ON public.domain_blocks USING btree (domain)",
);
const INDEX_UNAVAILABLE_DOMAINS: (&str, &str) = (
    "index_unavailable_domains_on_domain",
    "CREATE UNIQUE INDEX index_unavailable_domains_on_domain ON public.unavailable_domains USING btree (domain)",
);
const INDEX_EMAIL_DOMAIN_BLOCKS: (&str, &str) = (
    "index_email_domain_blocks_on_domain",
    "CREATE UNIQUE INDEX index_email_domain_blocks_on_domain ON public.email_domain_blocks USING btree (domain)",
);
const INDEX_MEDIA_ATTACHMENTS: (&str, &str) = (
    "index_media_attachments_on_shortcode",
    "CREATE UNIQUE INDEX index_media_attachments_on_shortcode ON public.media_attachments USING btree (shortcode text_pattern_ops) WHERE (shortcode IS NOT NULL)",
);
const INDEX_PREVIEW_CARDS: (&str, &str) = (
    "index_preview_cards_on_url",
    "CREATE UNIQUE INDEX index_preview_cards_on_url ON public.preview_cards USING btree (url)",
);
const INDEX_STATUSES: (&str, &str) = (
    "index_statuses_on_uri",
    "CREATE UNIQUE INDEX index_statuses_on_uri ON public.statuses USING btree (uri text_pattern_ops) WHERE (uri IS NOT NULL)",
);
const INDEX_ACCOUNTS: (&str, &str) = (
    "index_accounts_on_username_and_domain_lower",
    "CREATE UNIQUE INDEX index_accounts_on_username_and_domain_lower ON public.accounts USING btree (lower((username)::text), COALESCE(lower((domain)::text), ''::text))",
);
const INDEX_TAGS: (&str, &str) = (
    "index_tags_on_name_lower_btree",
    "CREATE UNIQUE INDEX index_tags_on_name_lower_btree ON public.tags USING btree (lower((name)::text) text_pattern_ops)",
);
const INDEX_WEBAUTHN_CREDENTIALS: (&str, &str) = (
    "index_webauthn_credentials_on_external_id",
    "CREATE UNIQUE INDEX index_webauthn_credentials_on_external_id ON public.webauthn_credentials USING btree (external_id)",
);
const INDEX_WEBHOOKS: (&str, &str) = (
    "index_webhooks_on_url",
    "CREATE UNIQUE INDEX index_webhooks_on_url ON public.webhooks USING btree (url)",
);

/// `remove_index_if_exists!`.
async fn remove_index(conn: &mut PgConnection, name: &str) -> anyhow::Result<()> {
    sqlx::query(&format!("DROP INDEX IF EXISTS public.{name}"))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// `database_connection.add_index`.
async fn add_index(conn: &mut PgConnection, index: (&str, &str)) -> anyhow::Result<()> {
    sqlx::query(index.1)
        .execute(&mut *conn)
        .await
        .with_context(|| format!("restoring {}", index.0))?;
    Ok(())
}

/// `rebuild_index`.
async fn rebuild_index(conn: &mut PgConnection, name: &str) -> anyhow::Result<()> {
    sqlx::query(&format!("REINDEX INDEX public.{name}"))
        .execute(&mut *conn)
        .await
        .with_context(|| format!("rebuilding {name}"))?;
    Ok(())
}

/// The work of a deduplication, then its `ensure`: whatever became of the
/// work, the indexes are put back, and the work's failure is the one told.
fn finish(work: anyhow::Result<()>, restored: anyhow::Result<()>) -> anyhow::Result<()> {
    work.and(restored)
}

/// `duplicate_record_ids` and `duplicate_record_ids_without_nulls`: the ids
/// of each group of rows `group_by` says are one, oldest first.
async fn duplicate_ids(
    conn: &mut PgConnection,
    table: &str,
    group_by: &str,
    without_nulls: bool,
) -> anyhow::Result<Vec<Vec<i64>>> {
    let filter = if without_nulls {
        format!("WHERE {group_by} IS NOT NULL")
    } else {
        String::new()
    };
    Ok(sqlx::query_scalar(&format!(
        "SELECT array_agg(id ORDER BY id) FROM {table} {filter}
         GROUP BY {group_by} HAVING count(*) > 1"
    ))
    .fetch_all(&mut *conn)
    .await?)
}

/// `ids` ordered by `order`.
async fn ordered(
    conn: &mut PgConnection,
    table: &str,
    ids: &[i64],
    order: &str,
) -> anyhow::Result<Vec<i64>> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT id FROM {table} WHERE id = ANY($1) ORDER BY {order}"
    ))
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?)
}

async fn delete_rows(conn: &mut PgConnection, table: &str, ids: &[i64]) -> anyhow::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query(&format!("DELETE FROM {table} WHERE id = ANY($1)"))
        .bind(ids)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// `klass.where(column => from).find_each { |r| r.update_attribute(column,
/// to) rescue RecordNotUnique }`: what can move moves, and a row that would
/// collide with one already there stays where it is.
async fn reassign(
    conn: &mut PgConnection,
    table: &str,
    column: &str,
    from: i64,
    to: i64,
    filter: &str,
) -> anyhow::Result<()> {
    let moved = sqlx::query(&format!(
        "UPDATE {table} SET {column} = $1 WHERE {column} = $2 {filter}"
    ))
    .bind(to)
    .bind(from)
    .execute(&mut *conn)
    .await;
    if moved.is_ok() {
        return Ok(());
    }
    let rows: Vec<i64> = sqlx::query_scalar(&format!(
        "SELECT id FROM {table} WHERE {column} = $1 {filter}"
    ))
    .bind(from)
    .fetch_all(&mut *conn)
    .await?;
    for row in rows {
        let _ = sqlx::query(&format!("UPDATE {table} SET {column} = $1 WHERE id = $2"))
            .bind(to)
            .bind(row)
            .execute(&mut *conn)
            .await;
    }
    Ok(())
}

/// The accts of `users`' accounts, joined as Mastodon lists them.
async fn accts_of_users(conn: &mut PgConnection, user_ids: &[i64]) -> anyhow::Result<String> {
    let accts: Vec<String> = sqlx::query_scalar(
        "SELECT CASE WHEN a.domain IS NULL THEN a.username ELSE a.username || '@' || a.domain END
         FROM users u JOIN accounts a ON a.id = u.account_id
         WHERE u.id = ANY($1)
         ORDER BY array_position($1, u.id)",
    )
    .bind(user_ids)
    .fetch_all(&mut *conn)
    .await?;
    Ok(accts.join(", "))
}

/// `deduplicate_users!`.
async fn users(conn: &mut PgConnection, console: &dyn Console) -> anyhow::Result<()> {
    for (name, _) in [
        INDEX_USERS_CONFIRMATION_TOKEN,
        INDEX_USERS_EMAIL,
        INDEX_USERS_RESET_PASSWORD_TOKEN,
    ] {
        remove_index(conn, name).await?;
    }
    console.say("Deduplicating user records…");
    let work = users_work(conn, console).await;
    console.say("Restoring users indexes…");
    let restored = async {
        add_index(conn, INDEX_USERS_CONFIRMATION_TOKEN).await?;
        add_index(conn, INDEX_USERS_EMAIL).await?;
        add_index(conn, INDEX_USERS_RESET_PASSWORD_TOKEN).await?;
        rebuild_index(conn, "index_users_on_unconfirmed_email").await
    }
    .await;
    finish(work, restored)
}

async fn users_work(conn: &mut PgConnection, console: &dyn Console) -> anyhow::Result<()> {
    // `deduplicate_users_process_email`: the most recently updated keeps the
    // address, and the others are told apart by a prefix.
    for ids in duplicate_ids(conn, "users", "email", false).await? {
        let mut users = ordered(conn, "users", &ids, "updated_at DESC").await?;
        let reference = users.remove(0);
        let email: String = sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
            .bind(reference)
            .fetch_one(&mut *conn)
            .await?;
        console.say(&format!(
            "Multiple users registered with e-mail address {email}."
        ));
        console.say(&format!(
            "e-mail will be disabled for the following accounts: {}",
            accts_of_users(conn, &users).await?
        ));
        console.say(
            "Please reach out to them and set another address with `eunha accounts modify` or \
             delete them.",
        );
        for (index, user) in users.iter().enumerate() {
            sqlx::query(
                "UPDATE users SET email = $2 || ' ' || email, updated_at = now() WHERE id = $1",
            )
            .bind(user)
            .bind(index.to_string())
            .execute(&mut *conn)
            .await?;
        }
    }
    // `deduplicate_users_process_confirmation_token`.
    for ids in duplicate_ids(conn, "users", "confirmation_token", true).await? {
        let users = ordered(conn, "users", &ids, "created_at DESC").await?;
        let others = &users[1..];
        console.say(&format!(
            "Unsetting confirmation token for those accounts: {}",
            accts_of_users(conn, others).await?
        ));
        sqlx::query(
            "UPDATE users SET confirmation_token = NULL, updated_at = now() WHERE id = ANY($1)",
        )
        .bind(others)
        .execute(&mut *conn)
        .await?;
    }
    // `deduplicate_users_process_password_token`.
    for ids in duplicate_ids(conn, "users", "reset_password_token", true).await? {
        let users = ordered(conn, "users", &ids, "updated_at DESC").await?;
        let others = &users[1..];
        console.say(&format!(
            "Unsetting password reset token for those accounts: {}",
            accts_of_users(conn, others).await?
        ));
        sqlx::query(
            "UPDATE users SET reset_password_token = NULL, updated_at = now() WHERE id = ANY($1)",
        )
        .bind(others)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// `deduplicate_account_domain_blocks!`.
async fn account_domain_blocks(
    conn: &mut PgConnection,
    console: &dyn Console,
) -> anyhow::Result<()> {
    remove_index(conn, INDEX_ACCOUNT_DOMAIN_BLOCKS.0).await?;
    console.say("Removing duplicate account domain blocks…");
    let work = async {
        for ids in duplicate_ids(conn, "account_domain_blocks", "account_id, domain", false).await?
        {
            delete_rows(conn, "account_domain_blocks", &ids[1..]).await?;
        }
        Ok(())
    }
    .await;
    console.say("Restoring account domain blocks indexes…");
    let restored = add_index(conn, INDEX_ACCOUNT_DOMAIN_BLOCKS).await;
    finish(work, restored)
}

/// The rows of each group but the newest are destroyed.
async fn drop_all_but_newest(
    conn: &mut PgConnection,
    table: &str,
    group_by: &str,
) -> anyhow::Result<()> {
    for ids in duplicate_ids(conn, table, group_by, false).await? {
        let newest_first = ordered(conn, table, &ids, "id DESC").await?;
        delete_rows(conn, table, &newest_first[1..]).await?;
    }
    Ok(())
}

/// `deduplicate_announcement_reactions!`.
async fn announcement_reactions(
    conn: &mut PgConnection,
    console: &dyn Console,
) -> anyhow::Result<()> {
    remove_index(conn, INDEX_ANNOUNCEMENT_REACTIONS.0).await?;
    console.say("Removing duplicate announcement reactions…");
    let work = drop_all_but_newest(
        conn,
        "announcement_reactions",
        "account_id, announcement_id, name",
    )
    .await;
    console.say("Restoring announcement_reactions indexes…");
    let restored = add_index(conn, INDEX_ANNOUNCEMENT_REACTIONS).await;
    finish(work, restored)
}

/// `deduplicate_domain_allows!`, `deduplicate_unavailable_domains!`,
/// `deduplicate_preview_cards!`, `deduplicate_webauthn_credentials!` and
/// `deduplicate_webhooks!`: the newest of each group stays.
async fn keep_newest(
    conn: &mut PgConnection,
    console: &dyn Console,
    table: &str,
    group_by: &str,
    index: (&str, &str),
) -> anyhow::Result<()> {
    remove_index(conn, index.0).await?;
    console.say(&format!("Deduplicating {table}…"));
    let work = drop_all_but_newest(conn, table, group_by).await;
    console.say(&format!("Restoring {table} indexes…"));
    let restored = add_index(conn, index).await;
    finish(work, restored)
}

/// `deduplicate_conversations!`: the newest of each stays, and is given the
/// others' mutes and participants.
async fn conversations(conn: &mut PgConnection, console: &dyn Console) -> anyhow::Result<()> {
    remove_index(conn, INDEX_CONVERSATIONS.0).await?;
    console.say("Deduplicating conversations…");
    let work = async {
        for ids in duplicate_ids(conn, "conversations", "uri", true).await? {
            let conversations = ordered(conn, "conversations", &ids, "id DESC").await?;
            let reference = conversations[0];
            for &other in &conversations[1..] {
                for table in ["conversation_mutes", "account_conversations"] {
                    reassign(conn, table, "conversation_id", other, reference, "").await?;
                }
                delete_rows(conn, "conversations", &[other]).await?;
            }
        }
        Ok(())
    }
    .await;
    console.say("Restoring conversations indexes…");
    let restored = add_index(conn, INDEX_CONVERSATIONS).await;
    finish(work, restored)
}

/// `deduplicate_custom_emojis!`: the newest of each stays, and is given the
/// others' announcement reactions.
async fn custom_emojis(conn: &mut PgConnection, console: &dyn Console) -> anyhow::Result<()> {
    remove_index(conn, INDEX_CUSTOM_EMOJIS.0).await?;
    console.say("Deduplicating custom_emojis…");
    let work = async {
        for ids in duplicate_ids(conn, "custom_emojis", "shortcode, domain", false).await? {
            let emojis = ordered(conn, "custom_emojis", &ids, "id DESC").await?;
            let others = &emojis[1..];
            sqlx::query(
                "UPDATE announcement_reactions SET custom_emoji_id = $1
                 WHERE custom_emoji_id = ANY($2)",
            )
            .bind(emojis[0])
            .bind(others)
            .execute(&mut *conn)
            .await?;
            delete_rows(conn, "custom_emojis", others).await?;
        }
        Ok(())
    }
    .await;
    console.say("Restoring custom_emojis indexes…");
    let restored = add_index(conn, INDEX_CUSTOM_EMOJIS).await;
    finish(work, restored)
}

/// `deduplicate_custom_emoji_categories!`: the newest of each stays, and is
/// given the others' emojis.
async fn custom_emoji_categories(
    conn: &mut PgConnection,
    console: &dyn Console,
) -> anyhow::Result<()> {
    remove_index(conn, INDEX_CUSTOM_EMOJI_CATEGORIES.0).await?;
    console.say("Deduplicating custom_emoji_categories…");
    let work = async {
        for ids in duplicate_ids(conn, "custom_emoji_categories", "name", false).await? {
            let categories = ordered(conn, "custom_emoji_categories", &ids, "id DESC").await?;
            let others = &categories[1..];
            sqlx::query("UPDATE custom_emojis SET category_id = $1 WHERE category_id = ANY($2)")
                .bind(categories[0])
                .bind(others)
                .execute(&mut *conn)
                .await?;
            delete_rows(conn, "custom_emoji_categories", others).await?;
        }
        Ok(())
    }
    .await;
    console.say("Restoring custom_emoji_categories indexes…");
    let restored = add_index(conn, INDEX_CUSTOM_EMOJI_CATEGORIES).await;
    finish(work, restored)
}

/// `deduplicate_domain_blocks!`: the most severe block stays, rejecting
/// media and reports if any of them did, with the first comments any of them
/// had.
async fn domain_blocks(conn: &mut PgConnection, console: &dyn Console) -> anyhow::Result<()> {
    remove_index(conn, INDEX_DOMAIN_BLOCKS.0).await?;
    console.say("Deduplicating domain_blocks…");
    let work = async {
        for ids in duplicate_ids(conn, "domain_blocks", "domain", false).await? {
            // `by_severity.reverse`: suspend, then silence, then noop.
            let rows = sqlx::query(
                "SELECT id, reject_media, reject_reports, private_comment, public_comment
                 FROM domain_blocks WHERE id = ANY($1)
                 ORDER BY CASE severity WHEN 1 THEN 0 WHEN 0 THEN 1 ELSE 2 END, id DESC",
            )
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await?;
            let present = |column: &str| {
                rows.iter().find_map(|row| {
                    row.get::<Option<String>, _>(column)
                        .filter(|text| !text.trim().is_empty())
                })
            };
            let private_comment = present("private_comment");
            let public_comment = present("public_comment");
            let reject_media = rows.iter().any(|row| row.get::<bool, _>("reject_media"));
            let reject_reports = rows.iter().any(|row| row.get::<bool, _>("reject_reports"));
            let reference: i64 = rows[0].get("id");
            let others: Vec<i64> = rows[1..].iter().map(|row| row.get("id")).collect();
            sqlx::query(
                "UPDATE domain_blocks
                 SET reject_media = $2, reject_reports = $3, private_comment = $4,
                     public_comment = $5, updated_at = now()
                 WHERE id = $1",
            )
            .bind(reference)
            .bind(reject_media)
            .bind(reject_reports)
            .bind(private_comment)
            .bind(public_comment)
            .execute(&mut *conn)
            .await?;
            delete_rows(conn, "domain_blocks", &others).await?;
        }
        Ok(())
    }
    .await;
    console.say("Restoring domain_blocks indexes…");
    let restored = add_index(conn, INDEX_DOMAIN_BLOCKS).await;
    finish(work, restored)
}

/// `deduplicate_email_domain_blocks!`: a top-level block stays before one
/// made for a parent's MX records.
async fn email_domain_blocks(conn: &mut PgConnection, console: &dyn Console) -> anyhow::Result<()> {
    remove_index(conn, INDEX_EMAIL_DOMAIN_BLOCKS.0).await?;
    console.say("Deduplicating email_domain_blocks…");
    let work = async {
        for ids in duplicate_ids(conn, "email_domain_blocks", "domain", false).await? {
            let blocks = ordered(
                conn,
                "email_domain_blocks",
                &ids,
                "parent_id ASC NULLS FIRST, id",
            )
            .await?;
            delete_rows(conn, "email_domain_blocks", &blocks[1..]).await?;
        }
        Ok(())
    }
    .await;
    console.say("Restoring email_domain_blocks indexes…");
    let restored = add_index(conn, INDEX_EMAIL_DOMAIN_BLOCKS).await;
    finish(work, restored)
}

/// `deduplicate_media_attachments!`: all but one lose their shortcode.
async fn media_attachments(conn: &mut PgConnection, console: &dyn Console) -> anyhow::Result<()> {
    remove_index(conn, INDEX_MEDIA_ATTACHMENTS.0).await?;
    console.say("Deduplicating media_attachments…");
    let work = async {
        for ids in duplicate_ids(conn, "media_attachments", "shortcode", true).await? {
            sqlx::query("UPDATE media_attachments SET shortcode = NULL WHERE id = ANY($1)")
                .bind(&ids[1..])
                .execute(&mut *conn)
                .await?;
        }
        Ok(())
    }
    .await;
    console.say("Restoring media_attachments indexes…");
    let restored = add_index(conn, INDEX_MEDIA_ATTACHMENTS).await;
    finish(work, restored)
}

/// `deduplicate_statuses!`: the oldest of each stays; a duplicate by the
/// same account gives it its favourites, mentions, poll, bookmarks, pin,
/// replies and reblogs first.
async fn statuses(conn: &mut PgConnection, console: &dyn Console) -> anyhow::Result<()> {
    remove_index(conn, INDEX_STATUSES.0).await?;
    console.say("Deduplicating statuses…");
    let work = async {
        for ids in duplicate_ids(conn, "statuses", "uri", true).await? {
            let rows: Vec<(i64, i64)> = sqlx::query_as(
                "SELECT id, account_id FROM statuses WHERE id = ANY($1) ORDER BY id ASC",
            )
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await?;
            let (reference, account_id) = rows[0];
            for &(other, other_account) in &rows[1..] {
                if other_account == account_id {
                    for table in ["favourites", "mentions", "polls", "bookmarks"] {
                        reassign(conn, table, "status_id", other, reference, "").await?;
                    }
                    reassign(
                        conn,
                        "status_pins",
                        "status_id",
                        other,
                        reference,
                        &format!("AND account_id = {account_id}"),
                    )
                    .await?;
                    reassign(conn, "statuses", "in_reply_to_id", other, reference, "").await?;
                    reassign(conn, "statuses", "reblog_of_id", other, reference, "").await?;
                }
                delete_rows(conn, "statuses", &[other]).await?;
            }
        }
        Ok(())
    }
    .await;
    console.say("Restoring statuses indexes…");
    let restored = add_index(conn, INDEX_STATUSES).await;
    finish(work, restored)
}

/// `deduplicate_accounts!`.
async fn accounts(conn: &mut PgConnection, console: &dyn Console) -> anyhow::Result<()> {
    remove_index(conn, INDEX_ACCOUNTS.0).await?;
    console.say(
        "Deduplicating accounts… for local accounts, you will be asked to chose which account \
         to keep unchanged.",
    );
    let work = async {
        for ids in duplicate_ids(
            conn,
            "accounts",
            "lower(username), COALESCE(lower(domain), '')",
            false,
        )
        .await?
        {
            let local: bool =
                sqlx::query_scalar("SELECT domain IS NULL FROM accounts WHERE id = $1")
                    .bind(ids[0])
                    .fetch_one(&mut *conn)
                    .await?;
            if local {
                local_accounts(conn, console, &ids).await?;
            } else {
                remote_accounts(conn, &ids).await?;
            }
        }
        Ok(())
    }
    .await;
    console.say("Restoring index_accounts_on_username_and_domain_lower…");
    let restored = async {
        add_index(conn, INDEX_ACCOUNTS).await?;
        console.say("Reindexing textual indexes on accounts…");
        for index in [
            "search_index",
            "index_accounts_on_uri",
            "index_accounts_on_url",
            "index_accounts_on_domain_and_id",
        ] {
            rebuild_index(conn, index).await?;
        }
        Ok(())
    }
    .await;
    finish(work, restored)
}

/// Ruby's `Time#to_s` for a UTC time.
fn ruby_time(at: Option<chrono::NaiveDateTime>) -> String {
    at.map_or_else(
        || "N/A".to_owned(),
        |at| at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    )
}

/// `deduplicate_local_accounts!`: the operator picks the one to keep, and
/// the others are renamed `username_1`, `username_2`…
async fn local_accounts(
    conn: &mut PgConnection,
    console: &dyn Console,
    ids: &[i64],
) -> anyhow::Result<()> {
    let rows = sqlx::query(
        "SELECT a.id, a.username, a.created_at, a.updated_at, u.last_sign_in_at,
                s.statuses_count, s.last_status_at
         FROM accounts a
         LEFT JOIN users u ON u.account_id = a.id
         LEFT JOIN account_stats s ON s.account_id = a.id
         WHERE a.id = ANY($1)
         ORDER BY a.id DESC",
    )
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;
    let first: String = rows[0].get("username");
    console.say(&format!(
        "Multiple local accounts were found for username '{first}'."
    ));
    console.say(
        "All those accounts are distinct accounts but only the most recently-created one is \
         fully-functional.",
    );
    for (index, row) in rows.iter().enumerate() {
        console.say(&format!(
            "{index:>2}. {}: created at: {}; updated at: {}; last logged in at: {}; statuses: {:>5}; last status at: {}",
            row.get::<String, _>("username"),
            ruby_time(Some(row.get("created_at"))),
            ruby_time(Some(row.get("updated_at"))),
            ruby_time(row.get("last_sign_in_at")),
            row.get::<Option<i64>, _>("statuses_count").unwrap_or(0),
            ruby_time(row.get("last_status_at")),
        ));
    }
    console
        .say("Please chose the one to keep unchanged, other ones will be automatically renamed.");
    // `ask(…).to_i`: what is not a number is the first.
    let keep: usize = console
        .ask("Account to keep unchanged:", "0")
        .trim()
        .parse()
        .unwrap_or(0);
    let mut renamed: Vec<(i64, String)> = rows
        .iter()
        .map(|row| (row.get("id"), row.get("username")))
        .collect();
    if keep < renamed.len() {
        renamed.remove(keep);
    }
    let mut i = 0;
    for (id, username) in renamed {
        i += 1;
        let mut candidate = format!("{username}_{i}");
        loop {
            let taken: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM accounts WHERE domain IS NULL AND username = $1)",
            )
            .bind(&candidate)
            .fetch_one(&mut *conn)
            .await?;
            if !taken {
                break;
            }
            i += 1;
            candidate = format!("{username}_{i}");
        }
        sqlx::query("UPDATE accounts SET username = $2, updated_at = now() WHERE id = $1")
            .bind(id)
            .bind(&candidate)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// `deduplicate_remote_accounts!`: the most recently updated stays; another
/// with the same key is the same actor and gives it what it has first.
async fn remote_accounts(conn: &mut PgConnection, ids: &[i64]) -> anyhow::Result<()> {
    let accounts = ordered(conn, "accounts", ids, "updated_at DESC").await?;
    let reference = accounts[0];
    let key = crate::federation::keypair::rsa_public_key(&mut *conn, reference).await?;
    for &other in &accounts[1..] {
        if crate::federation::keypair::rsa_public_key(&mut *conn, other).await? == key {
            for (table, column) in crate::federation::process_account::MERGED_COLUMNS
                .iter()
                .chain(&[("bulk_imports", "account_id")])
            {
                reassign(conn, table, column, other, reference, "").await?;
            }
        }
        delete_rows(conn, "accounts", &[other]).await?;
    }
    Ok(())
}

/// `deduplicate_tags!`: the tag the most of usable, trendable and listable
/// stays, and is given the others' featured tags.
async fn tags(conn: &mut PgConnection, console: &dyn Console) -> anyhow::Result<()> {
    remove_index(conn, "index_tags_on_name_lower").await?;
    remove_index(conn, INDEX_TAGS.0).await?;
    console.say("Deduplicating tags…");
    let work = async {
        for ids in duplicate_ids(conn, "tags", "lower((name)::text)", false).await? {
            let tags = ordered(
                conn,
                "tags",
                &ids,
                "(usable::int + trendable::int + listable::int) DESC",
            )
            .await?;
            let reference = tags[0];
            for &other in &tags[1..] {
                reassign(conn, "featured_tags", "tag_id", other, reference, "").await?;
                delete_rows(conn, "tags", &[other]).await?;
            }
        }
        Ok(())
    }
    .await;
    console.say("Restoring tags indexes…");
    let restored = add_index(conn, INDEX_TAGS).await;
    finish(work, restored)
}
