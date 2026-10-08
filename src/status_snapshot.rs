//! `Status::SnapshotConcern`: a post's edit history is a `status_edits` row
//! for each version, the first and the current one included.
//!
//! Mastodon writes the history in one shape, whoever edits the post: before
//! the first edit, a snapshot of the original stamped with the post's
//! `created_at` (`create_previous_edit!`, `record_previous_edit!`), then after
//! every edit a snapshot of the version it made, stamped with the new
//! `edited_at` (`create_edit!`, `create_edits!`). The rows are the versions
//! oldest first, the last one the post as it is; `HistoriesController`
//! serves exactly those, or a snapshot built on the spot for a post never
//! edited.

use chrono::NaiveDateTime;
use sqlx::PgConnection;

/// `Status#build_snapshot`: what a `StatusEdit` keeps of one version.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub account_id: i64,
    pub text: String,
    pub spoiler_text: String,
    pub sensitive: bool,
    /// `ordered_media_attachment_ids&.dup || media_attachments.pluck(:id)`.
    pub ordered_media_attachment_ids: Vec<i64>,
    /// `ordered_media_attachments.map(&:description)`.
    pub media_descriptions: Vec<Option<String>>,
    /// `preloadable_poll&.options&.dup`.
    pub poll_options: Option<Vec<String>>,
    /// `quote&.id`.
    pub quote_id: Option<i64>,
}

/// `@status.edits.any?`.
pub async fn has_edits(conn: &mut PgConnection, status_id: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM status_edits WHERE status_id = $1) AS "e!""#,
        status_id
    )
    .fetch_one(conn)
    .await
}

/// `Status#build_snapshot` of the post as the database now has it, or `None`
/// for a post that is not there.
pub async fn build(conn: &mut PgConnection, status_id: i64) -> sqlx::Result<Option<Snapshot>> {
    let row = sqlx::query!(
        r#"SELECT s.account_id, s.text, s.spoiler_text, s.sensitive,
                  COALESCE(s.ordered_media_attachment_ids,
                           ARRAY(SELECT m.id FROM media_attachments m
                                 WHERE m.status_id = s.id ORDER BY m.id)) AS "media_ids!",
                  -- `ordered_media_attachments`: by id when no order is
                  -- recorded, else the recorded order of those still
                  -- attached; at most four.
                  CASE WHEN s.ordered_media_attachment_ids IS NULL THEN
                      ARRAY(SELECT m.description FROM media_attachments m
                            WHERE m.status_id = s.id ORDER BY m.id LIMIT 4)
                  ELSE
                      ARRAY(SELECT m.description
                            FROM unnest(s.ordered_media_attachment_ids) WITH ORDINALITY AS o(id, n)
                            JOIN media_attachments m ON m.id = o.id AND m.status_id = s.id
                            ORDER BY o.n LIMIT 4)
                  END AS "descriptions!: Vec<Option<String>>",
                  (SELECT p.options FROM polls p WHERE p.id = s.poll_id) AS "poll_options: Vec<String>",
                  (SELECT q.id FROM quotes q WHERE q.status_id = s.id LIMIT 1) AS quote_id
           FROM statuses s WHERE s.id = $1"#,
        status_id
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|row| Snapshot {
        account_id: row.account_id,
        text: row.text,
        spoiler_text: row.spoiler_text,
        sensitive: row.sensitive,
        ordered_media_attachment_ids: row.media_ids,
        media_descriptions: row.descriptions,
        poll_options: row.poll_options,
        quote_id: row.quote_id,
    }))
}

impl Snapshot {
    /// `build_snapshot(account_id:, at_time:).save!`: the edit by
    /// `account_id` (the author when `None`), stamped `created_at`.
    pub async fn save(
        &self,
        conn: &mut PgConnection,
        status_id: i64,
        account_id: Option<i64>,
        created_at: NaiveDateTime,
    ) -> sqlx::Result<()> {
        sqlx::query!(
            r#"INSERT INTO status_edits
                 (status_id, account_id, text, spoiler_text, sensitive,
                  ordered_media_attachment_ids, media_descriptions, poll_options, quote_id,
                  created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, now())"#,
            status_id,
            account_id.unwrap_or(self.account_id),
            self.text,
            self.spoiler_text,
            self.sensitive,
            &self.ordered_media_attachment_ids,
            &self.media_descriptions as &[Option<String>],
            self.poll_options.as_deref(),
            self.quote_id,
            created_at,
        )
        .execute(conn)
        .await?;
        Ok(())
    }
}

/// `create_previous_edit!`: the original, stamped with the post's
/// `created_at` and by its author, when the post has no history yet. Run
/// before an edit changes anything.
pub async fn create_previous_edit(conn: &mut PgConnection, status_id: i64) -> sqlx::Result<()> {
    if has_edits(&mut *conn, status_id).await? {
        return Ok(());
    }
    let Some(original) = build(&mut *conn, status_id).await? else {
        return Ok(());
    };
    let created_at =
        sqlx::query_scalar!("SELECT created_at FROM statuses WHERE id = $1", status_id)
            .fetch_one(&mut *conn)
            .await?;
    original.save(conn, status_id, None, created_at).await
}

/// `create_edit!`: the version an edit by `account_id` made, stamped with
/// the post's new `edited_at`. Run once the edit has changed everything.
pub async fn create_edit(
    conn: &mut PgConnection,
    status_id: i64,
    account_id: i64,
) -> sqlx::Result<()> {
    let Some(current) = build(&mut *conn, status_id).await? else {
        return Ok(());
    };
    let at = sqlx::query_scalar!(
        r#"SELECT COALESCE(edited_at, created_at) AS "at!" FROM statuses WHERE id = $1"#,
        status_id
    )
    .fetch_one(&mut *conn)
    .await?;
    current.save(conn, status_id, Some(account_id), at).await
}
