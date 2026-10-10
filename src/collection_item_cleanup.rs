//! Mastodon's `Scheduler::CollectionItemCleanupScheduler`: every hour, the
//! collection items that were rejected or revoked more than a day ago are
//! deleted.

/// `RETENTION_PERIOD`, 24 hours.
const RETENTION_HOURS: i32 = 24;

/// `CollectionItem` states `rejected` and `revoked`.
const REJECTED: i32 = 2;
const REVOKED: i32 = 3;

/// `CollectionItemCleanupScheduler#perform`. `destroy_all` destroys each item,
/// and each `destroy` takes one off its collection's `item_count` counter
/// cache, so a collection's count drops by the number of its items deleted.
pub async fn perform(db: &sqlx::PgPool) -> anyhow::Result<u64> {
    let deleted = sqlx::query_scalar!(
        r#"WITH deleted AS (
               DELETE FROM collection_items
               WHERE state IN ($1, $2)
                 AND updated_at < now() AT TIME ZONE 'UTC' - make_interval(hours => $3)
               RETURNING collection_id
           ),
           counted AS (
               UPDATE collections c SET item_count = COALESCE(c.item_count, 0) - d.n
               FROM (SELECT collection_id, count(*)::int AS n FROM deleted GROUP BY collection_id) d
               WHERE c.id = d.collection_id
           )
           SELECT count(*) AS "deleted!" FROM deleted"#,
        REJECTED,
        REVOKED,
        RETENTION_HOURS,
    )
    .fetch_one(db)
    .await?;
    Ok(deleted as u64)
}
