//! `tootctl preview_cards`: `Mastodon::CLI::PreviewCards`.

use crate::state::AppState;

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Remove preview card media.
    ///
    /// Removes local thumbnails for preview cards. `--days` sets the age a
    /// preview card must be before its image is removed. A card is not
    /// fetched again unless the link is posted again two weeks after its last
    /// use, so removing images from within the last 14 days is not
    /// recommended. With `--link`, only link-type cards' images are removed,
    /// skipping video and photo cards.
    Remove(RemoveOptions),
}

#[derive(clap::Args, Debug, Clone)]
pub struct RemoveOptions {
    /// How old, in days, what is removed must be.
    #[arg(long, default_value_t = 180)]
    pub days: i64,
    /// How many records to work on at once.
    #[arg(short = 'c', long, default_value_t = 5)]
    pub concurrency: usize,
    /// Print each record as it is processed.
    #[arg(short = 'v', long)]
    pub verbose: bool,
    /// Report what would be done, changing nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Only link-type preview cards.
    #[arg(long)]
    pub link: bool,
}

impl Default for RemoveOptions {
    fn default() -> Self {
        Self {
            days: 180,
            concurrency: 5,
            verbose: false,
            dry_run: false,
            link: false,
        }
    }
}

impl Command {
    pub(crate) fn concurrency(&self) -> usize {
        match self {
            Self::Remove(options) => options.concurrency,
        }
    }
}

pub async fn run(state: &AppState, command: Command) -> anyhow::Result<()> {
    match command {
        Command::Remove(options) => println!("{}", remove(state, &options).await?),
    }
    Ok(())
}

/// `PreviewCards#remove`: the image of each card holding one
/// (`PreviewCard.cached`), of type link with `link`, not updated within
/// `days`, removed; what it prints.
pub async fn remove(state: &AppState, options: &RemoveOptions) -> anyhow::Result<String> {
    let days = i32::try_from(options.days).unwrap_or(i32::MAX);
    let link_only = options.link;
    let dry_run = options.dry_run;
    let tally = super::parallelize_batches(
        options.concurrency,
        options.verbose,
        |after| async move {
            sqlx::query_scalar!(
                "SELECT id FROM preview_cards
                 WHERE image_file_name IS NOT NULL AND image_file_name <> ''
                   AND (NOT $1 OR type = 0)
                   AND updated_at < now() - make_interval(days => $2)
                   AND id > $3
                 ORDER BY id LIMIT $4",
                link_only,
                days,
                after,
                super::BATCH,
            )
            .fetch_all(&state.db)
            .await
        },
        |id| async move { remove_image(state, dry_run, id).await },
    )
    .await?;
    let link = if options.link { "link-type " } else { "" };
    Ok(format!(
        "Removed media from {} {link}preview cards (approx. {}){}",
        tally.processed,
        super::human_size(tally.aggregate),
        super::dry_run_suffix(dry_run)
    ))
}

/// `preview_card.image.destroy` and `save`.
async fn remove_image(state: &AppState, dry_run: bool, id: i64) -> anyhow::Result<Option<i64>> {
    let card = sqlx::query!(
        "SELECT image_file_name, image_file_size, image_storage_schema_version
         FROM preview_cards WHERE id = $1",
        id
    )
    .fetch_one(&state.db)
    .await?;
    let Some(name) = card.image_file_name.filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    if !dry_run {
        sqlx::query!(
            "UPDATE preview_cards SET
               image_file_name = NULL, image_content_type = NULL, image_file_size = NULL,
               image_updated_at = NULL, updated_at = now()
             WHERE id = $1",
            id
        )
        .execute(&state.db)
        .await?;
        let key = crate::preview_card::image_path(id, &name, card.image_storage_schema_version);
        if let Err(error) = state.storage.delete(&key).await {
            tracing::debug!(%error, key, "could not remove a preview card image");
        }
    }
    Ok(Some(i64::from(card.image_file_size.unwrap_or(0))))
}
