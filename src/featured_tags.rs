//! `FeaturedTag`'s counters: how many of its account's posts that everyone
//! may see carry the tag, and when the newest of them was posted.
//!
//! Mastodon counts them once, when the featured tag is created
//! (`before_create :reset_data`: its account's kept public and unlisted posts
//! with the tag, and the date of the one with the highest id, which the
//! creating `INSERT`s count), and from then on moves them as posts come
//! and go: `ProcessHashtagsService` and `ActivityPub::Activity::Create` count a
//! public or unlisted post's tags in (`FeaturedTag#increment`),
//! `ProcessHashtagsService` and `ActivityPub::ProcessStatusUpdateService`
//! count out the tags an edit took away, and `RemoveStatusService` the tags of
//! a post it removes (`FeaturedTag#decrement`). Only the newest post's date is
//! ever looked up again, and only when the post going was the newest.

use chrono::NaiveDateTime;
use sqlx::PgPool;

use crate::db::models::vis;

/// `FeaturedTag#increment(timestamp)` for each of the account's featured
/// tags among `tag_ids`.
pub async fn increment(
    db: &PgPool,
    account_id: i64,
    tag_ids: &[i64],
    timestamp: NaiveDateTime,
) -> sqlx::Result<()> {
    if tag_ids.is_empty() {
        return Ok(());
    }
    sqlx::query!(
        r#"UPDATE featured_tags
           SET statuses_count = statuses_count + 1, last_status_at = $3, updated_at = now()
           WHERE account_id = $1 AND tag_id = ANY($2)"#,
        account_id,
        tag_ids,
        timestamp,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `FeaturedTag#decrement(deleted_status)` for each of the account's featured
/// tags among `tag_ids`: down to nought with no date when it was the last
/// post; down by one when a newer post is the newest; otherwise down by one
/// with the newest earlier post's date.
pub async fn decrement(
    db: &PgPool,
    account_id: i64,
    tag_ids: &[i64],
    status_id: i64,
    status_created_at: NaiveDateTime,
) -> sqlx::Result<()> {
    if tag_ids.is_empty() {
        return Ok(());
    }
    let featured = sqlx::query!(
        r#"SELECT id, tag_id, statuses_count, last_status_at FROM featured_tags
           WHERE account_id = $1 AND tag_id = ANY($2) ORDER BY id"#,
        account_id,
        tag_ids,
    )
    .fetch_all(db)
    .await?;
    for ft in featured {
        if ft.statuses_count <= 1 {
            // `update` saves nothing when nothing changed.
            sqlx::query!(
                r#"UPDATE featured_tags
                   SET statuses_count = 0, last_status_at = NULL, updated_at = now()
                   WHERE id = $1 AND (statuses_count <> 0 OR last_status_at IS NOT NULL)"#,
                ft.id,
            )
            .execute(db)
            .await?;
        } else if ft.last_status_at.is_some_and(|at| at > status_created_at) {
            sqlx::query!(
                r#"UPDATE featured_tags
                   SET statuses_count = statuses_count - 1, updated_at = now()
                   WHERE id = $1"#,
                ft.id,
            )
            .execute(db)
            .await?;
        } else {
            sqlx::query!(
                r#"UPDATE featured_tags SET
                     statuses_count = statuses_count - 1,
                     last_status_at = (
                       SELECT s.created_at FROM statuses s
                       JOIN statuses_tags st ON st.status_id = s.id AND st.tag_id = $2
                       WHERE s.account_id = $3 AND s.deleted_at IS NULL
                         AND s.visibility IN (0, 1) AND s.id < $4
                       ORDER BY s.id DESC LIMIT 1),
                     updated_at = now()
                   WHERE id = $1"#,
                ft.id,
                ft.tag_id,
                account_id,
                status_id,
            )
            .execute(db)
            .await?;
        }
    }
    Ok(())
}

/// `ProcessHashtagsService#update_featured_tags!` (and its copy in
/// `ActivityPub::ProcessStatusUpdateService#update_tags!`): a post that is not
/// public or unlisted moves nothing; otherwise the tags it gained are counted
/// in, and the ones it lost counted out.
#[allow(clippy::too_many_arguments)]
pub async fn update_for_status(
    db: &PgPool,
    account_id: i64,
    status_id: i64,
    visibility: i32,
    created_at: NaiveDateTime,
    previous_tag_ids: &[i64],
    current_tag_ids: &[i64],
) -> sqlx::Result<()> {
    if !vis::distributable(visibility) {
        return Ok(());
    }
    let added: Vec<i64> = current_tag_ids
        .iter()
        .filter(|id| !previous_tag_ids.contains(id))
        .copied()
        .collect();
    increment(db, account_id, &added, created_at).await?;
    let removed: Vec<i64> = previous_tag_ids
        .iter()
        .filter(|id| !current_tag_ids.contains(id))
        .copied()
        .collect();
    decrement(db, account_id, &removed, status_id, created_at).await
}

/// `RemoveStatusService#remove_from_hashtags`: each of the author's featured
/// tags that the removed post carries is counted out, whatever the post's
/// visibility.
pub async fn status_removed(
    db: &PgPool,
    account_id: i64,
    status_id: i64,
    status_created_at: NaiveDateTime,
) -> sqlx::Result<()> {
    let tag_ids: Vec<i64> = sqlx::query_scalar!(
        "SELECT tag_id FROM statuses_tags WHERE status_id = $1",
        status_id
    )
    .fetch_all(db)
    .await?;
    decrement(db, account_id, &tag_ids, status_id, status_created_at).await
}
