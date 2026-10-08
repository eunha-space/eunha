//! Mastodon's `Scheduler::CollectionItemCleanupScheduler`: every hour, the
//! collection items that were rejected or revoked more than a day ago are
//! deleted.

use std::time::Duration;

use crate::state::AppState;

/// The scheduler runs hourly.
pub const EVERY: Duration = Duration::from_secs(60 * 60);

/// `RETENTION_PERIOD`, 24 hours.
const RETENTION_HOURS: i32 = 24;

/// `CollectionItem` states `rejected` and `revoked`.
const REJECTED: i32 = 2;
const REVOKED: i32 = 3;

/// Run the cleanup hourly for as long as the instance runs, the first pass
/// an hour in, as a Sidekiq `interval` schedule starts.
pub async fn run(state: AppState) {
    loop {
        crate::background::rest(&state.stop, EVERY).await;
        if state.stop.is_cancelled() {
            break;
        }
        if let Err(error) = perform(&state.db).await {
            tracing::error!(%error, "collection item cleanup failed");
        }
    }
}

/// `CollectionItemCleanupScheduler#perform`. Each item's `destroy` updates
/// its collection's `item_count` counter cache; eunha's count is of the
/// pending and accepted items, which these are not, so it is recounted the
/// same way rather than decremented.
pub async fn perform(db: &sqlx::PgPool) -> anyhow::Result<u64> {
    let mut tx = db.begin().await?;
    let collections: Vec<i64> = sqlx::query_scalar!(
        r#"DELETE FROM collection_items
           WHERE state IN ($1, $2)
             AND updated_at < now() AT TIME ZONE 'UTC' - make_interval(hours => $3)
           RETURNING collection_id"#,
        REJECTED,
        REVOKED,
        RETENTION_HOURS,
    )
    .fetch_all(&mut *tx)
    .await?;
    let deleted = collections.len() as u64;
    let mut collections = collections;
    collections.sort_unstable();
    collections.dedup();
    sqlx::query!(
        r#"UPDATE collections c SET item_count =
             (SELECT count(*) FROM collection_items i
              WHERE i.collection_id = c.id AND i.state IN (0, 1))
           WHERE c.id = ANY($1)"#,
        &collections,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(deleted)
}
