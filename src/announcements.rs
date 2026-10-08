//! What happens around an announcement besides its row: publishing it to the
//! signed-in users' streams (`PublishScheduledAnnouncementWorker`), taking it
//! back off them (`UnpublishAnnouncementWorker`), streaming reaction counts
//! (`PublishAnnouncementReactionWorker`), and the part of
//! `Scheduler::ScheduledStatusesScheduler` that publishes scheduled
//! announcements and unpublishes expired ones.

use std::time::Duration;

use crate::state::AppState;

/// How often the schedule is looked at. Mastodon's scheduler runs every five
/// minutes and queues each announcement for its exact time; a minute is close
/// enough to that without a job queue.
const SCHEDULE_EVERY: Duration = Duration::from_secs(60);

/// `Status.from_text`: the posts the announcement's text links to, local ones
/// by their URL and remote ones by their URI or URL.
async fn statuses_from_text(state: &AppState, text: &str) -> anyhow::Result<Vec<i64>> {
    static URL: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r#"https?://[^\s<>"']+"#).expect("valid regex")
    });
    let local = format!("https://{}/", state.instance.domain);
    let mut ids = vec![];
    for found in URL.find_iter(text) {
        let url = found.as_str().trim_end_matches(['.', ',', ')', '!', '?']);
        let id = if let Some(path) = url.strip_prefix(&local) {
            // `/@user/:id` or `/users/:user/statuses/:id`.
            let segments: Vec<&str> = path.split('/').collect();
            match segments.as_slice() {
                [user, id] if user.starts_with('@') => id.parse::<i64>().ok(),
                ["users", _, "statuses", id] => id.parse::<i64>().ok(),
                _ => None,
            }
        } else {
            sqlx::query_scalar!(
                "SELECT id FROM statuses WHERE uri = $1 OR url = $1 LIMIT 1",
                url
            )
            .fetch_optional(&state.db)
            .await?
        };
        if let Some(id) = id {
            let exists = sqlx::query_scalar!(
                r#"SELECT EXISTS (SELECT 1 FROM statuses WHERE id = $1) AS "e!""#,
                id
            )
            .fetch_one(&state.db)
            .await?;
            if exists && !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    Ok(ids)
}

/// `PublishScheduledAnnouncementWorker#perform`: link the posts the text
/// names, publish the announcement unless it already is, and send it to every
/// signed-in user's stream.
pub async fn publish(state: &AppState, id: i64) -> anyhow::Result<()> {
    let Some(text) = sqlx::query_scalar!("SELECT text FROM announcements WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
    else {
        return Ok(());
    };
    let status_ids = statuses_from_text(state, &text).await?;
    sqlx::query!(
        "UPDATE announcements SET status_ids = $2, updated_at = now() WHERE id = $1",
        id,
        &status_ids,
    )
    .execute(&state.db)
    .await?;
    // `publish! unless published?`.
    sqlx::query!(
        r#"UPDATE announcements
           SET published = true, published_at = now(), scheduled_at = NULL, updated_at = now()
           WHERE id = $1 AND NOT published"#,
        id
    )
    .execute(&state.db)
    .await?;
    let rendered = crate::api::mastodon::announcements::render(state, &[id], None)
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    if let Some(announcement) = rendered.into_iter().next() {
        let payload = serde_json::to_value(&announcement)?;
        crate::streaming::fan_out::to_active_accounts(
            state,
            serde_json::json!({"event": "announcement", "payload": payload}),
        )
        .await;
    }
    Ok(())
}

/// `PublishScheduledAnnouncementWorker.perform_async`.
pub async fn publish_later(state: &AppState, id: i64) {
    crate::jobs::push(
        state,
        PublishScheduledAnnouncementWorker {
            announcement_id: id,
        },
    )
    .await;
}

/// `PublishScheduledAnnouncementWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PublishScheduledAnnouncementWorker {
    pub announcement_id: i64,
}

impl crate::jobs::Job for PublishScheduledAnnouncementWorker {
    const KIND: &'static str = "PublishScheduledAnnouncementWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT;

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        publish(state, self.announcement_id).await
    }
}

/// `PublishAnnouncementReactionWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PublishAnnouncementReactionWorker {
    pub announcement_id: i64,
    pub name: String,
}

impl crate::jobs::Job for PublishAnnouncementReactionWorker {
    const KIND: &'static str = "PublishAnnouncementReactionWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT;

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        publish_reaction(state, self.announcement_id, &self.name).await
    }
}

/// `UnpublishAnnouncementWorker#perform`: take it off every stream.
pub async fn unpublish(state: &AppState, id: i64) {
    crate::streaming::fan_out::to_active_accounts(
        state,
        serde_json::json!({"event": "announcement.delete", "payload": id.to_string()}),
    )
    .await;
}

/// `PublishAnnouncementReactionWorker#perform`: the reaction's new count, to
/// every signed-in user's stream.
pub async fn publish_reaction(state: &AppState, id: i64, name: &str) -> anyhow::Result<()> {
    let row = sqlx::query!(
        r#"SELECT count(*) AS "count!", max(ce.id) AS emoji_id
           FROM announcement_reactions ar
           LEFT JOIN custom_emojis ce ON ce.id = ar.custom_emoji_id
           WHERE ar.announcement_id = $1 AND ar.name = $2"#,
        id,
        name,
    )
    .fetch_one(&state.db)
    .await?;
    let mut payload = serde_json::json!({
        "name": name,
        "count": row.count,
        "me": false,
    });
    // `REST::ReactionSerializer#url` and `#static_url`, for a custom emoji.
    if let Some(emoji_id) = row.emoji_id {
        if let Some(emoji) = sqlx::query!(
            "SELECT domain, image_file_name, image_remote_url, image_storage_schema_version
             FROM custom_emojis WHERE id = $1",
            emoji_id,
        )
        .fetch_optional(&state.db)
        .await?
        {
            let image = crate::custom_emoji::ImageRef {
                id: emoji_id,
                domain: emoji.domain.as_deref(),
                image_file_name: emoji.image_file_name.as_deref(),
                image_remote_url: emoji.image_remote_url.as_deref(),
                image_storage_schema_version: emoji.image_storage_schema_version,
            };
            let domain = &state.instance.domain;
            payload["url"] = image.url(&state.storage, domain, "original").into();
            payload["static_url"] = image.url(&state.storage, domain, "static").into();
        }
    }
    payload["announcement_id"] = id.to_string().into();
    crate::streaming::fan_out::to_active_accounts(
        state,
        serde_json::json!({"event": "announcement.reaction", "payload": payload}),
    )
    .await;
    Ok(())
}

/// One pass of the schedule: publish what is due, unpublish what has ended.
pub async fn run_schedule_once(state: &AppState) -> anyhow::Result<()> {
    let due = sqlx::query_scalar!(
        r#"SELECT id FROM announcements
           WHERE NOT published AND scheduled_at IS NOT NULL AND scheduled_at <= now()"#
    )
    .fetch_all(&state.db)
    .await?;
    for id in due {
        publish(state, id).await?;
    }
    // `unpublish_expired_announcements!`: no worker, so nothing is streamed.
    sqlx::query!(
        r#"UPDATE announcements SET published = false, scheduled_at = NULL
           WHERE published AND ends_at IS NOT NULL AND ends_at <= now()"#
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// The schedule, for as long as the instance runs.
pub async fn run_schedule(state: AppState) {
    while !state.stop.is_cancelled() {
        if let Err(error) = run_schedule_once(&state).await {
            tracing::error!(%error, "announcement schedule failed");
        }
        crate::background::rest(&state.stop, SCHEDULE_EVERY).await;
    }
}
