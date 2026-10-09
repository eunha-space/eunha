//! `tootctl statuses`: `Mastodon::CLI::Statuses`.
//!
//! The working tables `tootctl` creates in `public` and leaves behind for
//! `--continue` are eunha's alone, so they are made in the `eunha` schema.

use std::time::Instant;

use crate::state::AppState;

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Remove unreferenced statuses.
    ///
    /// Remove statuses that are not referenced by local user activity, such
    /// as ones that came from relays, or belonging to users that were once
    /// followed by someone locally but no longer are. It also removes
    /// orphaned records and performs additional cleanup tasks such as
    /// updating statistics and recovering disk space.
    Remove(RemoveOptions),
}

#[derive(clap::Args, Debug, Clone)]
pub struct RemoveOptions {
    /// How old, in days, what is removed must be.
    #[arg(long, default_value_t = 90)]
    pub days: i64,
    /// Number of records in each batch.
    #[arg(short = 'b', long, default_value_t = 1000)]
    pub batch_size: i64,
    /// If remove is not completed, execute from the previous continuation.
    #[arg(long = "continue")]
    pub resume: bool,
    /// Include the status of remote accounts that are followed by local
    /// accounts as candidates for remove.
    #[arg(long)]
    pub clean_followed: bool,
    /// Skip status remove (run only cleanup tasks).
    #[arg(long)]
    pub skip_status_remove: bool,
    /// Skip remove orphaned media attachments.
    #[arg(long)]
    pub skip_media_remove: bool,
    /// Compress database and update the statistics. This option locks the
    /// table for a long time, so run it offline.
    #[arg(long)]
    pub compress_database: bool,
}

impl Default for RemoveOptions {
    fn default() -> Self {
        Self {
            days: 90,
            batch_size: 1000,
            resume: false,
            clean_followed: false,
            skip_status_remove: false,
            skip_media_remove: false,
            compress_database: false,
        }
    }
}

pub async fn run(state: &AppState, command: Command) -> anyhow::Result<()> {
    match command {
        Command::Remove(options) => {
            remove(state, &options, |line| println!("{line}")).await?;
        }
    }
    Ok(())
}

/// What `statuses remove` removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Removed {
    pub statuses: u64,
    pub media_attachments: u64,
    pub conversations: u64,
}

/// `Statuses#remove`: the unreferenced remote statuses, then the media
/// attachments and conversations they leave orphaned, each table analysed
/// (or with `compress_database`, vacuumed and reindexed) after.
pub async fn remove(
    state: &AppState,
    options: &RemoveOptions,
    mut say: impl FnMut(&str),
) -> anyhow::Result<Removed> {
    anyhow::ensure!(
        options.batch_size >= 1,
        "Cannot run with this batch_size setting, must be at least 1"
    );
    let mut removed = Removed::default();
    if !options.skip_status_remove {
        removed.statuses = remove_statuses(state, options, &mut say).await?;
    }
    vacuum_and_analyze(state, "statuses", options.compress_database, &mut say).await?;
    if !options.skip_media_remove {
        removed.media_attachments = remove_orphan_media(state, options, &mut say).await?;
    }
    removed.conversations = remove_orphan_conversations(state, options, &mut say).await?;
    vacuum_and_analyze(state, "conversations", options.compress_database, &mut say).await?;
    Ok(removed)
}

async fn table_exists(state: &AppState, table: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT to_regclass('eunha.' || $1::text) IS NOT NULL AS "e!""#,
        table
    )
    .fetch_one(&state.db)
    .await
}

/// Delete the ids in `table` from `target`, a batch at a time, and drop the
/// table; how many ids there were and how many rows went.
async fn delete_listed(
    state: &AppState,
    table: &str,
    target: &str,
    batch_size: i64,
) -> sqlx::Result<(u64, u64)> {
    let mut processed = 0;
    let mut removed = 0;
    let mut after = i64::MIN;
    loop {
        let ids: Vec<i64> = sqlx::query_scalar(&format!(
            "SELECT id FROM eunha.{table} WHERE id > $1 ORDER BY id LIMIT $2"
        ))
        .bind(after)
        .bind(batch_size)
        .fetch_all(&state.db)
        .await?;
        let Some(&last) = ids.last() else { break };
        after = last;
        processed += ids.len() as u64;
        removed += sqlx::query(&format!("DELETE FROM {target} WHERE id = ANY($1)"))
            .bind(&ids)
            .execute(&state.db)
            .await?
            .rows_affected();
    }
    sqlx::query(&format!("DROP TABLE eunha.{table}"))
        .execute(&state.db)
        .await?;
    Ok((processed, removed))
}

/// `Statuses#remove_statuses`.
async fn remove_statuses(
    state: &AppState,
    options: &RemoveOptions,
    say: &mut impl FnMut(&str),
) -> anyhow::Result<u64> {
    let start = Instant::now();
    let max_id = super::id_days_ago(options.days);
    if !(options.resume && table_exists(state, "statuses_to_be_deleted").await?) {
        say("Extract the deletion target from statuses... This might take a while...");
        sqlx::query("DROP TABLE IF EXISTS eunha.statuses_to_be_deleted")
            .execute(&state.db)
            .await?;
        sqlx::query("CREATE TABLE eunha.statuses_to_be_deleted (id bigint PRIMARY KEY)")
            .execute(&state.db)
            .await?;
        // Unless `--clean-followed`, skip accounts followed by local accounts.
        let clean_followed = if options.clean_followed {
            ""
        } else {
            "AND NOT EXISTS (SELECT 1 FROM follows WHERE statuses.account_id = follows.target_account_id)"
        };
        sqlx::query(&format!(
            "INSERT INTO eunha.statuses_to_be_deleted (id)
             SELECT statuses.id FROM statuses WHERE deleted_at IS NULL AND NOT local AND uri IS NOT NULL AND (id < $1)
             AND NOT EXISTS (SELECT 1 FROM statuses AS statuses1 WHERE statuses.id = statuses1.in_reply_to_id)
             AND NOT EXISTS (SELECT 1 FROM statuses AS statuses1 WHERE statuses1.id = statuses.reblog_of_id AND (statuses1.uri IS NULL OR statuses1.local))
             AND NOT EXISTS (SELECT 1 FROM statuses AS statuses1 WHERE statuses.id = statuses1.reblog_of_id AND (statuses1.uri IS NULL OR statuses1.local OR statuses1.id >= $1))
             AND NOT EXISTS (SELECT 1 FROM status_pins WHERE statuses.id = status_id)
             AND NOT EXISTS (SELECT 1 FROM mentions WHERE statuses.id = mentions.status_id AND mentions.account_id IN (SELECT accounts.id FROM accounts WHERE domain IS NULL))
             AND NOT EXISTS (SELECT 1 FROM favourites WHERE statuses.id = favourites.status_id AND favourites.account_id IN (SELECT accounts.id FROM accounts WHERE domain IS NULL))
             AND NOT EXISTS (SELECT 1 FROM bookmarks WHERE statuses.id = bookmarks.status_id AND bookmarks.account_id IN (SELECT accounts.id FROM accounts WHERE domain IS NULL))
             AND NOT EXISTS (SELECT 1 FROM quotes JOIN statuses statuses1 ON quotes.status_id = statuses1.id WHERE quotes.quoted_status_id = statuses.id AND (statuses1.uri IS NULL OR statuses1.local))
             AND NOT EXISTS (SELECT 1 FROM quotes JOIN statuses statuses1 ON quotes.quoted_status_id = statuses1.id WHERE quotes.status_id = statuses.id AND (statuses1.uri IS NULL OR statuses1.local))
             {clean_followed}"
        ))
        .bind(max_id)
        .execute(&state.db)
        .await?;
        say("Removing temporary database indices to restore write performance...");
    }
    say("Beginning statuses removal... This might take a while...");
    // `Status.unscoped.where(id: ids).delete_all`: no callbacks; what hangs
    // off a status goes by its foreign keys.
    let (processed, removed) = delete_listed(
        state,
        "statuses_to_be_deleted",
        "statuses",
        options.batch_size,
    )
    .await?;
    say(&format!(
        "Done after {}s, removed {removed} out of {processed} statuses.",
        start.elapsed().as_secs_f64()
    ));
    Ok(removed)
}

/// `Statuses#remove_orphans_media_attachments`:
/// `MediaAttachment.unattached.created_before(options[:days].pred.days.ago)`,
/// each destroyed with its files. Eunha keeps a scheduled post's media in the
/// post's `params` rather than in `scheduled_status_id`, so those count as
/// attached too, as the daily vacuum counts them.
async fn remove_orphan_media(
    state: &AppState,
    options: &RemoveOptions,
    say: &mut impl FnMut(&str),
) -> anyhow::Result<u64> {
    let start = Instant::now();
    say("Beginning removal of now-orphaned media attachments to free up disk space...");
    let days = i32::try_from(options.days - 1).unwrap_or(i32::MAX);
    let mut processed = 0;
    let mut removed = 0;
    let mut after = i64::MIN;
    loop {
        let rows = sqlx::query!(
            r#"SELECT m.id, m.file_file_name, m.thumbnail_file_name FROM media_attachments m
               WHERE m.status_id IS NULL AND m.scheduled_status_id IS NULL
                 AND m.created_at < now() - make_interval(days => $1)
                 AND NOT EXISTS (
                   SELECT 1 FROM scheduled_statuses ss,
                     jsonb_array_elements_text(
                       CASE WHEN jsonb_typeof(ss.params::jsonb -> 'media_ids') = 'array'
                            THEN ss.params::jsonb -> 'media_ids' ELSE '[]'::jsonb END
                     ) e
                   WHERE e = m.id::text
                 )
                 AND m.id > $2
               ORDER BY m.id LIMIT $3"#,
            days,
            after,
            super::BATCH,
        )
        .fetch_all(&state.db)
        .await?;
        let Some(last) = rows.last() else { break };
        after = last.id;
        for row in rows {
            processed += 1;
            let deleted = sqlx::query!("DELETE FROM media_attachments WHERE id = $1", row.id)
                .execute(&state.db)
                .await;
            match deleted {
                Ok(_) => {
                    removed += 1;
                    for key in crate::media::attachment_keys(
                        row.id,
                        row.file_file_name.as_deref(),
                        row.thumbnail_file_name.as_deref(),
                    ) {
                        if let Err(error) = state.storage.delete(&key).await {
                            tracing::debug!(%error, key, "could not remove an orphaned file");
                        }
                    }
                }
                Err(error) => eprintln!("Error processing {}: {error}", row.id),
            }
        }
    }
    say(&format!(
        "Done after {}s, removed {removed} out of {processed} media_attachments.",
        start.elapsed().as_secs_f64()
    ));
    Ok(removed)
}

/// `Statuses#remove_orphans_conversations`: the conversations no status is
/// in. `tootctl` builds an index on `statuses.conversation_id` for this and
/// drops it after; the schema of the tracked release already has one,
/// `index_statuses_on_conversation_id`, so eunha builds none.
async fn remove_orphan_conversations(
    state: &AppState,
    options: &RemoveOptions,
    say: &mut impl FnMut(&str),
) -> anyhow::Result<u64> {
    let start = Instant::now();
    if !(options.resume && table_exists(state, "conversations_to_be_deleted").await?) {
        say("Extract the deletion target from conversations... This might take a while...");
        sqlx::query("DROP TABLE IF EXISTS eunha.conversations_to_be_deleted")
            .execute(&state.db)
            .await?;
        sqlx::query("CREATE TABLE eunha.conversations_to_be_deleted (id bigint PRIMARY KEY)")
            .execute(&state.db)
            .await?;
        sqlx::query(
            "INSERT INTO eunha.conversations_to_be_deleted (id)
             SELECT id FROM conversations WHERE NOT EXISTS
               (SELECT 1 FROM statuses WHERE statuses.conversation_id = conversations.id)",
        )
        .execute(&state.db)
        .await?;
    }
    say("Beginning orphans removal... This might take a while...");
    let (processed, removed) = delete_listed(
        state,
        "conversations_to_be_deleted",
        "conversations",
        options.batch_size,
    )
    .await?;
    say(&format!(
        "Done after {}s, removed {removed} out of {processed} conversations.",
        start.elapsed().as_secs_f64()
    ));
    Ok(removed)
}

/// `Statuses#vacuum_and_analyze_statuses` and `…_conversations`.
async fn vacuum_and_analyze(
    state: &AppState,
    table: &str,
    compress: bool,
    say: &mut impl FnMut(&str),
) -> sqlx::Result<()> {
    let statements = if compress {
        vec![
            format!("VACUUM FULL ANALYZE {table}"),
            format!("REINDEX TABLE {table}"),
        ]
    } else {
        vec![format!("ANALYZE {table}")]
    };
    for statement in statements {
        say(&format!("Running \"{statement}\"..."));
        sqlx::query(&statement).execute(&state.db).await?;
    }
    Ok(())
}
