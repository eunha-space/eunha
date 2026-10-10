//! Mastodon's `Scheduler::VacuumScheduler`: the parts the content
//! retention settings drive (`ContentRetentionPolicy`), and the feeds:
//!
//!  -  `Vacuum::StatusesVacuum`: remote posts older than
//!     `content_cache_retention_period` days are deleted;
//!  -  `Vacuum::MediaAttachmentsVacuum`: remote media cached longer than
//!     `media_cache_retention_period` days is forgotten (the remote URL stays),
//!     and uploads never attached to a post are deleted after a day;
//!  -  `Vacuum::PreviewCardsVacuum`: link preview images older than
//!     `media_cache_retention_period` days are forgotten;
//!  -  `Vacuum::AccessTokensVacuum`: access tokens and authorization grants
//!     that have expired or been revoked are deleted;
//!  -  `Vacuum::BackupsVacuum`: archives older than
//!     `backups_retention_period` days are deleted
//!     ([`crate::portability::backup::vacuum`]);
//!  -  `Vacuum::FeedsVacuum`: the home and list feeds of users who have not
//!     signed in for a week are removed from Redis (see [`crate::home_feed`]);
//!  -  `Vacuum::ImportsVacuum`: imports past their time are deleted
//!     ([`crate::portability::import::vacuum`]).
//!
//! It runs once a day at a time of the instance's own between 03:00 and 05:59
//! UTC ([`crate::scheduled_tasks::VACUUM`]).
//!
//! A period that is not a positive number of days keeps everything, as an
//! unset one does.

use crate::state::AppState;

/// `MediaAttachmentsVacuum::TTL`: how long an upload may wait for its post.
const ORPHAN_TTL_DAYS: i64 = 1;

/// How many rows one batch handles, as `find_in_batches` would.
const BATCH: i64 = 1000;

/// `ContentRetentionPolicy#retention_period`: `value.days if value.is_a?(Integer)
/// && value.positive?`.
fn retention_days(snapshot: &crate::settings::Snapshot, var: &str) -> Option<i64> {
    snapshot.integer(var).filter(|days| *days > 0)
}

/// `Mastodon::Snowflake.id_at(time, with_random: false)`.
fn id_at(time: chrono::DateTime<chrono::Utc>) -> i64 {
    time.timestamp_millis() << 16
}

/// `VacuumScheduler#perform`: each operation in turn, an error in one logged
/// and the rest still run.
pub async fn perform(state: &AppState) {
    let snapshot = crate::settings::Snapshot::load(state).await;
    let content = retention_days(&snapshot, "content_cache_retention_period");
    let media = retention_days(&snapshot, "media_cache_retention_period");
    if let Err(error) = vacuum_statuses(state, content).await {
        tracing::error!(%error, "statuses vacuum failed");
    }
    if let Err(error) = vacuum_media_attachments(state, media).await {
        tracing::error!(%error, "media attachments vacuum failed");
    }
    if let Err(error) = vacuum_preview_cards(state, media).await {
        tracing::error!(%error, "preview cards vacuum failed");
    }
    if let Err(error) = crate::portability::backup::vacuum(state).await {
        tracing::error!(%error, "backups vacuum failed");
    }
    if let Err(error) = vacuum_access_tokens(&state.db).await {
        tracing::error!(%error, "access tokens vacuum failed");
    }
    let mut redis = state.redis.clone();
    if let Err(error) =
        crate::feed::vacuum_inactive_feeds(&mut redis, &state.redis_keys, &state.db).await
    {
        tracing::error!(%error, "feeds vacuum failed");
    }
    if let Err(error) = crate::portability::import::vacuum(state).await {
        tracing::error!(%error, "imports vacuum failed");
    }
}

/// `Vacuum::StatusesVacuum`: delete the remote posts from before the
/// retention period, direct ones first taken out of their conversations, and
/// leave the rest to the foreign keys.
pub async fn vacuum_statuses(state: &AppState, days: Option<i64>) -> anyhow::Result<u64> {
    let Some(days) = days else { return Ok(0) };
    let before = id_at(chrono::Utc::now() - chrono::Duration::days(days));
    let mut deleted = 0;
    loop {
        let ids: Vec<i64> = sqlx::query_scalar!(
            r#"SELECT s.id FROM statuses s JOIN accounts a ON a.id = s.account_id
               WHERE a.domain IS NOT NULL AND s.deleted_at IS NULL AND s.id < $1
               ORDER BY s.id LIMIT $2"#,
            before,
            BATCH,
        )
        .fetch_all(&state.db)
        .await?;
        if ids.is_empty() {
            break;
        }
        let mut tx = state.db.begin().await?;
        // `unlink_from_conversations!`: `AccountConversation.remove_status`
        // for every conversation a direct post is in.
        sqlx::query!(
            r#"UPDATE account_conversations ac
               SET status_ids = kept.ids,
                   last_status_id = kept.ids[array_length(kept.ids, 1)]
               FROM (
                 SELECT c.id, COALESCE(array_agg(x ORDER BY ord)
                          FILTER (WHERE NOT x = ANY($1)), '{}') AS ids
                 FROM account_conversations c,
                      unnest(c.status_ids) WITH ORDINALITY AS u(x, ord)
                 WHERE c.status_ids && $1
                 GROUP BY c.id
               ) kept
               WHERE ac.id = kept.id"#,
            &ids,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!("DELETE FROM account_conversations WHERE status_ids = '{}'")
            .execute(&mut *tx)
            .await?;
        let result = sqlx::query!("DELETE FROM statuses WHERE id = ANY($1)", &ids)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        // `remove_from_index`: queued, so the next pass deletes them from
        // the indexes.
        crate::search::elasticsearch::indexing::enqueue(
            state,
            crate::search::elasticsearch::Index::Statuses,
            &ids,
        )
        .await;
        crate::search::elasticsearch::indexing::enqueue(
            state,
            crate::search::elasticsearch::Index::PublicStatuses,
            &ids,
        )
        .await;
        deleted += result.rows_affected();
    }
    Ok(deleted)
}

/// `Vacuum::AccessTokensVacuum`: the expired and the revoked access tokens,
/// then grants, deleted in batches, with no callbacks (`delete_all`): what
/// hangs off a token goes by its foreign keys.
pub async fn vacuum_access_tokens(db: &sqlx::PgPool) -> anyhow::Result<u64> {
    // `expired`: `created_at + MAKE_INTERVAL(secs => expires_in) < NOW()`;
    // `revoked`: `revoked_at` set and passed.
    let statements = [
        r#"DELETE FROM oauth_access_tokens WHERE id IN (
             SELECT id FROM oauth_access_tokens
             WHERE expires_in IS NOT NULL
               AND created_at + make_interval(secs => expires_in) < now() AT TIME ZONE 'UTC'
             ORDER BY id LIMIT $1)"#,
        r#"DELETE FROM oauth_access_tokens WHERE id IN (
             SELECT id FROM oauth_access_tokens
             WHERE revoked_at IS NOT NULL AND revoked_at < now() AT TIME ZONE 'UTC'
             ORDER BY id LIMIT $1)"#,
        r#"DELETE FROM oauth_access_grants WHERE id IN (
             SELECT id FROM oauth_access_grants
             WHERE expires_in IS NOT NULL
               AND created_at + make_interval(secs => expires_in) < now() AT TIME ZONE 'UTC'
             ORDER BY id LIMIT $1)"#,
        r#"DELETE FROM oauth_access_grants WHERE id IN (
             SELECT id FROM oauth_access_grants
             WHERE revoked_at IS NOT NULL AND revoked_at < now() AT TIME ZONE 'UTC'
             ORDER BY id LIMIT $1)"#,
    ];
    let mut deleted = 0;
    for sql in statements {
        loop {
            let n = sqlx::query(sql)
                .bind(BATCH)
                .execute(db)
                .await?
                .rows_affected();
            deleted += n;
            if n == 0 {
                break;
            }
        }
    }
    Ok(deleted)
}

/// The objects an attachment's file is kept under, local and cached.
fn media_keys(id: i64, file_name: &str) -> Vec<String> {
    let partition = crate::media::int_to_path(id);
    ["original", "small"]
        .iter()
        .flat_map(|style| {
            let key = format!("media_attachments/files/{partition}/{style}/{file_name}");
            [format!("cache/{key}"), key]
        })
        .collect()
}

async fn remove(state: &AppState, keys: Vec<String>) {
    for key in keys {
        if let Err(error) = state.storage.delete(&key).await {
            tracing::debug!(%error, key, "could not remove a vacuumed file");
        }
    }
}

/// `Vacuum::MediaAttachmentsVacuum`: delete the uploads never attached, and,
/// with a retention period, forget remote media cached longer than it.
pub async fn vacuum_media_attachments(state: &AppState, days: Option<i64>) -> anyhow::Result<()> {
    // `orphaned_media_attachments`: `unattached.created_before(TTL.ago)`.
    loop {
        let rows = sqlx::query!(
            // Eunha keeps a scheduled post's media in its `params` rather
            // than in `scheduled_status_id`, so those count as attached too.
            r#"SELECT m.id, m.file_file_name FROM media_attachments m
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
               ORDER BY m.id LIMIT $2"#,
            ORPHAN_TTL_DAYS as i32,
            BATCH,
        )
        .fetch_all(&state.db)
        .await?;
        if rows.is_empty() {
            break;
        }
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        sqlx::query!("DELETE FROM media_attachments WHERE id = ANY($1)", &ids)
            .execute(&state.db)
            .await?;
        for row in rows {
            if let Some(name) = row.file_file_name.filter(|n| !n.is_empty()) {
                remove(state, media_keys(row.id, &name)).await;
            }
        }
    }

    let Some(days) = days else { return Ok(()) };
    // `MediaAttachment.remote.cached.created_before(...).updated_before(...)`.
    loop {
        let rows = sqlx::query!(
            r#"SELECT id, file_file_name FROM media_attachments
               WHERE remote_url <> '' AND file_file_name IS NOT NULL
                 AND created_at < now() - make_interval(days => $1)
                 AND updated_at < now() - make_interval(days => $1)
               ORDER BY id LIMIT $2"#,
            days as i32,
            BATCH,
        )
        .fetch_all(&state.db)
        .await?;
        if rows.is_empty() {
            break;
        }
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        // `AttachmentBatch#clear`: the Paperclip columns of each attachment
        // nulled, `updated_at` with them.
        sqlx::query!(
            r#"UPDATE media_attachments SET
                 file_file_name = NULL, file_content_type = NULL, file_file_size = NULL,
                 file_updated_at = NULL, thumbnail_file_name = NULL,
                 thumbnail_content_type = NULL, thumbnail_file_size = NULL,
                 thumbnail_updated_at = NULL
               WHERE id = ANY($1)"#,
            &ids,
        )
        .execute(&state.db)
        .await?;
        for row in rows {
            if let Some(name) = row.file_file_name.filter(|n| !n.is_empty()) {
                remove(state, media_keys(row.id, &name)).await;
            }
        }
    }
    Ok(())
}

/// `Vacuum::PreviewCardsVacuum`: forget the images of cards not updated within
/// the retention period.
pub async fn vacuum_preview_cards(state: &AppState, days: Option<i64>) -> anyhow::Result<()> {
    let Some(days) = days else { return Ok(()) };
    loop {
        let rows = sqlx::query!(
            r#"SELECT id, image_file_name AS "image_file_name!", image_storage_schema_version
               FROM preview_cards
               WHERE image_file_name IS NOT NULL AND image_file_name <> ''
                 AND updated_at < now() - make_interval(days => $1)
               ORDER BY id LIMIT $2"#,
            days as i32,
            BATCH,
        )
        .fetch_all(&state.db)
        .await?;
        if rows.is_empty() {
            break;
        }
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        sqlx::query!(
            r#"UPDATE preview_cards SET
                 image_file_name = NULL, image_content_type = NULL, image_file_size = NULL,
                 image_updated_at = NULL
               WHERE id = ANY($1)"#,
            &ids,
        )
        .execute(&state.db)
        .await?;
        for row in rows {
            let key = crate::preview_card::image_path(
                row.id,
                &row.image_file_name,
                row.image_storage_schema_version,
            );
            remove(state, vec![key]).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn ids_at_a_time_carry_no_random_bits() {
        let time = chrono::DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();
        assert_eq!(super::id_at(time), 1_700_000_000_000 << 16);
    }

    #[test]
    fn media_is_removed_wherever_it_may_be_kept() {
        let keys = super::media_keys(5, "a.png");
        assert!(keys.contains(&"media_attachments/files/000/000/005/original/a.png".to_owned()));
        assert!(keys.contains(&"cache/media_attachments/files/000/000/005/small/a.png".to_owned()));
    }
}
