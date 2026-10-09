//! Mastodon's archive takeout: `Settings::ExportsController#create` makes a
//! `backups` row, `BackupWorker` has `BackupService` zip up the account's
//! posts and media in ActivityPub form, stores the zip as the backup's
//! `dump` attachment, deletes the account's older archives and mails the
//! member (`UserMailer#backup_ready`).
//!
//! Upstream's worker is a Sidekiq job retried five times. Eunha's is
//! [`run_queue`], which takes its work from `eunha.backup_jobs`, a row per
//! archive still to build, so that a restart does not lose a request.
//!
//! The zip is laid out as upstream lays it out: `outbox.json`, the media
//! files under their storage paths (`media_attachments/files/…`),
//! `likes.json`, `bookmarks.json`, `avatar.*` and `header.*`, and
//! `actor.json`, which points at the others by those names. It is stored
//! where Paperclip stores a backup's dump:
//! `backups/dumps/{id partition}/original/archive-{time}-{hex}.zip`.

use std::io::Write as _;
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

use crate::db::models::Account;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// `BackupPolicy::MIN_AGE`, in days: one archive per this many days.
pub const MIN_AGE_DAYS: i32 = 6;
/// `BackupsController::BACKUP_LINK_TIMEOUT`.
pub const LINK_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// `sidekiq_options retry: 5`: the first attempt and five more.
const MAX_ATTEMPTS: i32 = 6;
/// How long a claimed job stays claimed by a worker that went quiet.
const STALE_LEASE: &str = "1 hour";

/// A `backups` row, as the API shows it.
#[derive(Debug, Serialize)]
pub struct Backup {
    pub id: String,
    pub processed: bool,
    pub dump_file_size: Option<i64>,
    pub created_at: String,
}

fn timestamp(time: chrono::NaiveDateTime) -> String {
    time.and_utc()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

async fn user_id_for(state: &AppState, account_id: i64) -> AppResult<i64> {
    sqlx::query_scalar!("SELECT id FROM users WHERE account_id = $1", account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::Forbidden)
}

/// `current_user.backups`.
pub async fn list(state: &AppState, account_id: i64) -> AppResult<Vec<Backup>> {
    let user_id = user_id_for(state, account_id).await?;
    let rows = sqlx::query!(
        "SELECT id, processed, dump_file_size, created_at FROM backups WHERE user_id = $1 ORDER BY id",
        user_id,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Backup {
            id: row.id.to_string(),
            processed: row.processed,
            dump_file_size: row.dump_file_size,
            created_at: timestamp(row.created_at),
        })
        .collect())
}

/// `BackupPolicy#create?`: no archive was requested in the last six days.
pub async fn can_create(state: &AppState, account_id: i64) -> AppResult<bool> {
    let user_id = user_id_for(state, account_id).await?;
    Ok(!sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM backups
             WHERE user_id = $1 AND created_at >= now() - make_interval(days => $2)
           ) AS "exists!""#,
        user_id,
        MIN_AGE_DAYS,
    )
    .fetch_one(&state.db)
    .await?)
}

/// `Settings::ExportsController#create`: under the `backup:{user id}` lock,
/// ask for an archive, which `BackupWorker` then builds.
pub async fn create(state: &AppState, account_id: i64) -> AppResult<Backup> {
    let user_id = user_id_for(state, account_id).await?;
    let lock = crate::redis_lock::try_acquire_lockable(
        state,
        &format!("backup:{user_id}"),
        crate::redis_lock::DEFAULT_TTL_MS,
    )
    .await
    .ok_or_else(|| {
        AppError::ServiceUnavailable(
            "There was a temporary problem serving your request, please try again".into(),
        )
    })?;
    if !can_create(state, account_id).await? {
        return Err(AppError::Forbidden);
    }
    let mut tx = state.db.begin().await?;
    let row = sqlx::query!(
        "INSERT INTO backups (user_id, processed, created_at, updated_at)
         VALUES ($1, false, now(), now())
         RETURNING id, created_at",
        user_id,
    )
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO eunha.backup_jobs (backup_id) VALUES ($1)",
        row.id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    lock.release().await;

    if crate::feed::sync_fanout() {
        drain(state).await?;
    } else {
        state.queues.backups.notify_one();
    }
    let processed = sqlx::query!(
        "SELECT processed, dump_file_size FROM backups WHERE id = $1",
        row.id
    )
    .fetch_optional(&state.db)
    .await?;
    Ok(Backup {
        id: row.id.to_string(),
        processed: processed.as_ref().is_some_and(|p| p.processed),
        dump_file_size: processed.and_then(|p| p.dump_file_size),
        created_at: timestamp(row.created_at),
    })
}

/// `user.backups.create!` and `BackupWorker.perform_async(backup.id)`, as
/// `tootctl accounts backup` asks for an archive: no lock and no six-day
/// limit. Returns the backup's id. The archive is built by the job loops of
/// the running server, which mail its owner the link.
pub async fn request(state: &AppState, user_id: i64) -> anyhow::Result<i64> {
    let mut tx = state.db.begin().await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO backups (user_id, processed, created_at, updated_at)
         VALUES ($1, false, now(), now())
         RETURNING id",
    )
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO eunha.backup_jobs (backup_id) VALUES ($1)")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    state.queues.backups.notify_one();
    Ok(id)
}

/// Paperclip's path for a backup's dump.
fn dump_key(backup_id: i64, file_name: &str) -> String {
    format!(
        "backups/dumps/{}/original/{file_name}",
        crate::media::int_to_path(backup_id)
    )
}

/// `BackupsController#download`: a link to the archive that works for an
/// hour.
pub async fn download_url(state: &AppState, account_id: i64, backup_id: i64) -> AppResult<String> {
    let user_id = user_id_for(state, account_id).await?;
    let file_name = sqlx::query_scalar!(
        "SELECT dump_file_name FROM backups WHERE id = $1 AND user_id = $2 AND processed",
        backup_id,
        user_id,
    )
    .fetch_optional(&state.db)
    .await?
    .flatten()
    .ok_or(AppError::NotFound)?;
    state
        .storage
        .presigned_url(&dump_key(backup_id, &file_name), LINK_TIMEOUT)
        .await
}

/// `Backup#destroy`: the row, and Paperclip's file with it.
async fn destroy(state: &AppState, backup_id: i64) -> AppResult<()> {
    let file_name = sqlx::query_scalar!(
        "DELETE FROM backups WHERE id = $1 RETURNING dump_file_name",
        backup_id
    )
    .fetch_optional(&state.db)
    .await?
    .flatten();
    if let Some(file_name) = file_name {
        if let Err(error) = state.storage.delete(&dump_key(backup_id, &file_name)).await {
            tracing::warn!(backup_id, %error, "could not delete an archive's file");
        }
    }
    Ok(())
}

// ── The queue ───────────────────────────────────────────────────────────

/// Build requested archives until the instance stops.
pub async fn run_queue(state: AppState) {
    let worker = format!("backups-{}", std::process::id());
    let mut idle = crate::background::IdleBackoff::new(
        Duration::from_secs(1),
        state.config.workers.sanitized().queue_idle_poll(),
    );
    while !state.stop.is_cancelled() {
        match work_once(&state, &worker).await {
            Ok(true) => idle.reset(),
            Ok(false) => idle.idle(&state.queues.backups, &state.stop).await,
            Err(error) => {
                tracing::error!(%error, "archive queue pass failed");
                crate::background::rest(&state.stop, Duration::from_secs(30)).await;
            }
        }
    }
}

/// Build every archive that is due. The tests, and a request while
/// background work is inline, use this in place of the queue.
pub async fn drain(state: &AppState) -> AppResult<()> {
    while work_once(state, "inline").await? {}
    Ok(())
}

/// Claim one due job and run `BackupWorker#perform` for it. Returns whether
/// there was one.
pub async fn work_once(state: &AppState, worker: &str) -> AppResult<bool> {
    let job = sqlx::query!(
        r#"WITH picked AS (
             SELECT backup_id FROM eunha.backup_jobs
             WHERE run_at <= now()
               AND (locked_at IS NULL OR locked_at < now() - $2::text::interval)
             ORDER BY run_at, backup_id
             LIMIT 1
             FOR UPDATE SKIP LOCKED
           )
           UPDATE eunha.backup_jobs j
           SET locked_at = now(), locked_by = $1, attempts = j.attempts + 1, updated_at = now()
           FROM picked WHERE j.backup_id = picked.backup_id
           RETURNING j.backup_id, j.attempts"#,
        worker,
        STALE_LEASE,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(job) = job else {
        return Ok(false);
    };
    match perform(state, job.backup_id).await {
        Ok(()) => {
            sqlx::query!(
                "DELETE FROM eunha.backup_jobs WHERE backup_id = $1",
                job.backup_id
            )
            .execute(&state.db)
            .await?;
        }
        Err(error) if job.attempts >= MAX_ATTEMPTS => {
            // `sidekiq_retries_exhausted`: the request is dropped.
            tracing::warn!(backup_id = job.backup_id, %error, "archive could not be built; giving up");
            destroy(state, job.backup_id).await?;
        }
        Err(error) => {
            tracing::warn!(backup_id = job.backup_id, %error, "archive could not be built; will retry");
            let backoff = 30_i64 << job.attempts.clamp(1, 8);
            sqlx::query!(
                "UPDATE eunha.backup_jobs
                 SET locked_at = NULL, locked_by = NULL, last_error = $2,
                     run_at = now() + make_interval(secs => $3), updated_at = now()
                 WHERE backup_id = $1",
                job.backup_id,
                crate::error::sanitize_error_text(&error.to_string()),
                backoff as f64,
            )
            .execute(&state.db)
            .await?;
        }
    }
    Ok(true)
}

/// `BackupWorker#perform`.
async fn perform(state: &AppState, backup_id: i64) -> AppResult<()> {
    let backup = sqlx::query!(
        r#"SELECT b.user_id, u.account_id AS "account_id?", u.email AS "email?",
                  (u.confirmed_at IS NOT NULL AND u.approved AND NOT u.disabled) AS "active?"
           FROM backups b LEFT JOIN users u ON u.id = b.user_id
           WHERE b.id = $1"#,
        backup_id,
    )
    .fetch_optional(&state.db)
    .await?;
    // `return true if backup&.user.nil?`.
    let Some(backup) = backup else {
        return Ok(());
    };
    let (Some(user_id), Some(account_id)) = (backup.user_id, backup.account_id) else {
        return Ok(());
    };
    let account = sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", account_id)
        .fetch_one(&state.db)
        .await?;

    build_archive(state, backup_id, &account).await?;

    // `user.backups.where.not(id: backup.id).destroy_all`.
    let older: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM backups WHERE user_id = $1 AND id <> $2",
        user_id,
        backup_id,
    )
    .fetch_all(&state.db)
    .await?;
    for id in older {
        destroy(state, id).await?;
    }

    // `UserMailer.backup_ready(user, backup).deliver_later`, which
    // `active_for_authentication?` keeps from a user who cannot sign in.
    if backup.active == Some(true) {
        if let Some(email) = backup.email {
            let sender = state.mailer();
            let domain = state.instance.domain.clone();
            {
                if let Err(error) = sender.send_backup_ready(&email, &domain, backup_id).await {
                    tracing::warn!(%error, "could not send an archive-ready email");
                }
            }
        }
    }
    Ok(())
}

/// `BackupService::archive_id`'s name for the file.
fn archive_filename() -> String {
    let hex: String = (0..16)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect();
    format!(
        "archive-{}-{hex}.zip",
        chrono::Utc::now().format("%Y%m%d%H%M%S")
    )
}

/// The zip being written, in a temporary file that goes when it is dropped.
struct Archive {
    /// `None` once finished.
    zip: Option<zip::ZipWriter<std::fs::File>>,
    path: std::path::PathBuf,
}

impl Archive {
    fn create() -> AppResult<Self> {
        let path = std::env::temp_dir().join(format!(
            "eunha-archive-{}-{}.zip",
            std::process::id(),
            crate::snowflake::next_id()
        ));
        let file = std::fs::File::create(&path).map_err(anyhow::Error::from)?;
        Ok(Self {
            zip: Some(zip::ZipWriter::new(file)),
            path,
        })
    }

    fn entry(&mut self, name: &str, bytes: &[u8]) -> AppResult<()> {
        let zip = self
            .zip
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("archive already finished"))?;
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(name, options).map_err(anyhow::Error::from)?;
        zip.write_all(bytes).map_err(anyhow::Error::from)?;
        Ok(())
    }

    fn finish(&mut self) -> AppResult<()> {
        if let Some(zip) = self.zip.take() {
            zip.finish().map_err(anyhow::Error::from)?;
        }
        Ok(())
    }
}

impl Drop for Archive {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// `BackupService#build_archive!`.
async fn build_archive(state: &AppState, backup_id: i64, account: &Account) -> AppResult<()> {
    let mut archive = Archive::create()?;

    dump_outbox(state, &mut archive, account).await?;
    dump_media_attachments(state, &mut archive, account.id).await?;
    // `favourite_statuses` and `bookmark_statuses`, which `find_in_batches`
    // walks in the order of the posts' ids.
    let likes = collection_uris(
        state,
        r#"SELECT s.id, s.uri, (s.reblog_of_id IS NOT NULL) AS reblog,
                  a.id AS account_id, a.id_scheme, a.username, a.domain
           FROM favourites f
           JOIN statuses s ON s.id = f.status_id AND s.deleted_at IS NULL
           JOIN accounts a ON a.id = s.account_id
           WHERE f.account_id = $1 ORDER BY s.id"#,
        account.id,
    )
    .await?;
    archive.entry("likes.json", &uri_collection("likes.json", likes)?)?;
    let bookmarks = collection_uris(
        state,
        r#"SELECT s.id, s.uri, (s.reblog_of_id IS NOT NULL) AS reblog,
                  a.id AS account_id, a.id_scheme, a.username, a.domain
           FROM bookmarks b
           JOIN statuses s ON s.id = b.status_id AND s.deleted_at IS NULL
           JOIN accounts a ON a.id = s.account_id
           WHERE b.account_id = $1 ORDER BY s.id"#,
        account.id,
    )
    .await?;
    archive.entry(
        "bookmarks.json",
        &uri_collection("bookmarks.json", bookmarks)?,
    )?;
    dump_actor(state, &mut archive, account).await?;
    archive.finish()?;

    let file_name = archive_filename();
    let size = std::fs::metadata(&archive.path)
        .map_err(anyhow::Error::from)?
        .len() as i64;
    state
        .storage
        .store_file(
            &archive.path,
            &dump_key(backup_id, &file_name),
            "application/zip",
        )
        .await?;
    sqlx::query!(
        "UPDATE backups
         SET dump_file_name = $2, dump_content_type = 'application/zip', dump_file_size = $3,
             dump_updated_at = now(), processed = true, updated_at = now()
         WHERE id = $1",
        backup_id,
        file_name,
        size,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `BackupService#download_to_zip`: a file the storage no longer has is
/// left out, as upstream rescues `Errno::ENOENT`.
async fn download_to_zip(
    state: &AppState,
    archive: &mut Archive,
    key: &str,
    name: &str,
) -> AppResult<()> {
    match state.storage.get(key).await {
        Ok(bytes) => archive.entry(name, &bytes),
        Err(error) => {
            tracing::warn!(key, %error, "could not back up a file");
            Ok(())
        }
    }
}

/// `dump_media_attachments!`'s name for a file: its Paperclip `path`, from
/// after the last `/system/` and without leading slashes.
fn zip_path(path: &str) -> &str {
    let path = match path.rfind("/system/") {
        Some(at) => &path[at + "/system/".len()..],
        None => path,
    };
    path.trim_start_matches('/')
}

/// `build_outbox_json!`'s rewrite of an attachment's URL: its path, without
/// a leading `/system/`.
fn attachment_path(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    let path = url.path();
    Some(path.strip_prefix("/system/").unwrap_or(path).to_owned())
}

/// `dump_media_attachments!`: the original of each of the account's
/// attached media (`MediaAttachment.attached.where(account:)`), in the
/// order of their ids.
async fn dump_media_attachments(
    state: &AppState,
    archive: &mut Archive,
    account_id: i64,
) -> AppResult<()> {
    let media = sqlx::query!(
        "SELECT id, file_file_name FROM media_attachments
         WHERE account_id = $1 AND (status_id IS NOT NULL OR scheduled_status_id IS NOT NULL)
         ORDER BY id",
        account_id,
    )
    .fetch_all(&state.db)
    .await?;
    for m in media {
        let Some(file_name) = m.file_file_name.filter(|f| !f.is_empty()) else {
            continue;
        };
        let key = format!(
            "media_attachments/files/{}/original/{file_name}",
            crate::media::int_to_path(m.id)
        );
        let path = state.storage.object_key(&key);
        download_to_zip(state, archive, &key, zip_path(&path)).await?;
    }
    Ok(())
}

/// `ActivityPub::NoteSerializer` on a local post: the note eunha federates,
/// with the members upstream's serializer adds besides.
async fn archived_note(
    state: &AppState,
    domain: &str,
    status_id: i64,
) -> AppResult<Option<crate::api::ap::note::NoteBundle>> {
    let Some(mut bundle) = crate::api::ap::note::build_note(state, domain, status_id).await? else {
        return Ok(None);
    };
    let extras = crate::api::ap::note::serializer_extras(state, domain, status_id).await?;
    if let Some(note) = bundle.note.as_object_mut() {
        note.extend(extras);
    }
    Ok(Some(bundle))
}

/// `BackupService#build_outbox_json!`: every post, oldest first, as the
/// `Create` (`ActivityPub::CreateNoteSerializer`) or `Announce`
/// (`ActivityPub::AnnounceNoteSerializer`) that made it, each through
/// `serialize_payload` without a signer, so unsigned, and without its
/// `@context`; the collection carries `full_context`. A `Create`'s
/// attachments point at their paths after `/system/`.
async fn dump_outbox(state: &AppState, archive: &mut Archive, account: &Account) -> AppResult<()> {
    let statuses = sqlx::query!(
        r#"SELECT s.id, s.reblog_of_id, s.visibility, s.created_at
           FROM statuses s WHERE s.account_id = $1 AND s.deleted_at IS NULL ORDER BY s.id"#,
        account.id,
    )
    .fetch_all(&state.db)
    .await?;
    let domain = state.instance.domain.as_str();
    let mut items = Vec::with_capacity(statuses.len());
    for status in &statuses {
        let item = match status.reblog_of_id {
            Some(reblog_of_id) => {
                announce_item(
                    state,
                    account,
                    status.id,
                    reblog_of_id,
                    status.visibility,
                    status.created_at,
                )
                .await?
            }
            None => archived_note(state, domain, status.id)
                .await?
                .map(|bundle| {
                    let mut item = bundle.into_create();
                    if let Some(object) = item.as_object_mut() {
                        object.remove("@context");
                    }
                    if let Some(attachments) = item["object"]["attachment"].as_array_mut() {
                        for attachment in attachments {
                            let path = attachment["url"].as_str().and_then(attachment_path);
                            if let Some(path) = path {
                                attachment["url"] = json!(path);
                            }
                        }
                    }
                    item
                }),
        };
        if let Some(item) = item {
            items.push(item);
        }
    }
    let outbox = json!({
        "@context": crate::api::ap::context_helper::full_context(),
        "id": "outbox.json",
        "type": "OrderedCollection",
        "totalItems": statuses.len(),
        "orderedItems": items,
    });
    archive.entry(
        "outbox.json",
        &serde_json::to_vec(&outbox).map_err(anyhow::Error::from)?,
    )
}

/// `ActivityPub::AnnounceNoteSerializer` on a boost: addressed by
/// `TagManager#to` and `#cc` (the boosted author first in `cc`), and
/// carrying the boosted post inline when it is one of the account's own
/// followers-only posts, its URI otherwise.
pub(crate) async fn announce_item(
    state: &AppState,
    account: &Account,
    status_id: i64,
    reblog_of_id: i64,
    visibility: i32,
    created_at: chrono::NaiveDateTime,
) -> AppResult<Option<Value>> {
    announce_note(
        state,
        account,
        status_id,
        reblog_of_id,
        visibility,
        created_at,
        true,
    )
    .await
}

/// [`announce_item`], with `allow_inlining` as the serializer's instance
/// option: `UndoAnnounceSerializer` sets it false, so the `Announce` an
/// `Undo` carries names the boosted post by its URI, which it does even
/// once the boosted post is gone. Inlining, a boost of a post that is gone
/// is not served (`None`).
pub(crate) async fn announce_note(
    state: &AppState,
    account: &Account,
    status_id: i64,
    reblog_of_id: i64,
    visibility: i32,
    created_at: chrono::NaiveDateTime,
    allow_inlining: bool,
) -> AppResult<Option<Value>> {
    use crate::db::models::vis;
    const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";
    let original = sqlx::query!(
        r#"SELECT s.id, s.uri, s.visibility, a.id AS account_id, a.id_scheme, a.username,
                  a.domain, a.uri AS account_uri
           FROM statuses s JOIN accounts a ON a.id = s.account_id
           WHERE s.id = $1 AND (s.deleted_at IS NULL OR NOT $2)"#,
        reblog_of_id,
        allow_inlining,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(original) = original else {
        return Ok(None);
    };
    let domain = state.instance.domain.as_str();
    let actor_url = crate::federation::tag::account_uri_of(domain, account);
    let followers_url = format!("{actor_url}/followers");
    let original_author = if original.domain.is_none() {
        Some(crate::federation::tag::account_uri(
            domain,
            original.account_id,
            original.id_scheme,
            &original.username,
        ))
    } else {
        original.account_uri.clone()
    };
    let to: Vec<String> = match visibility {
        vis::PUBLIC => vec![PUBLIC.to_owned()],
        vis::UNLISTED | vis::PRIVATE => vec![followers_url.clone()],
        _ => Vec::new(),
    };
    let mut cc: Vec<Value> = vec![json!(original_author)];
    match visibility {
        vis::PUBLIC => cc.push(json!(followers_url)),
        vis::UNLISTED => cc.push(json!(PUBLIC)),
        _ => {}
    }
    // `virtual_object`: the booster's own followers-only post goes inline.
    let inline =
        allow_inlining && original.account_id == account.id && original.visibility == vis::PRIVATE;
    let object = if inline {
        match archived_note(state, domain, original.id).await? {
            Some(bundle) => bundle.note,
            None => return Ok(None),
        }
    } else {
        json!(super::status_uri(
            state,
            super::StatusUriParts {
                status_id: original.id,
                uri: original.uri.as_deref(),
                reblog: false,
                account_id: original.account_id,
                account_id_scheme: original.id_scheme,
                account_username: &original.username,
                account_domain: original.domain.as_deref(),
            },
        ))
    };
    let id = crate::federation::tag::activity_uri(
        domain,
        account.id,
        account.id_scheme,
        &account.username,
        status_id,
    );
    Ok(Some(json!({
        "id": id,
        "type": "Announce",
        "actor": actor_url,
        "published": crate::api::ap::note::iso8601(created_at.and_utc()),
        "to": to,
        "cc": cc,
        "object": object,
    })))
}

/// An `Announce` from [`announce_note`] as a document of its own, under the
/// `@context` the adapter writes: the note's when the boosted post is
/// inline, the plain ActivityStreams one otherwise.
pub(crate) fn announce_document(mut announce: Value) -> Value {
    let context = if announce["object"].is_object() {
        crate::api::ap::note::note_context()
    } else {
        json!("https://www.w3.org/ns/activitystreams")
    };
    if let Some(members) = announce.as_object_mut() {
        let mut with_context = serde_json::Map::new();
        with_context.insert("@context".into(), context);
        with_context.append(members);
        *members = with_context;
    }
    announce
}

/// The URIs a likes or bookmarks query names.
async fn collection_uris(state: &AppState, query: &str, account_id: i64) -> AppResult<Vec<String>> {
    use sqlx::Row as _;
    let rows = sqlx::query(query)
        .bind(account_id)
        .fetch_all(&state.db)
        .await?;
    let mut uris = Vec::with_capacity(rows.len());
    for row in &rows {
        let username: String = row.try_get("username")?;
        let uri: Option<String> = row.try_get("uri")?;
        let domain: Option<String> = row.try_get("domain")?;
        uris.push(super::status_uri(
            state,
            super::StatusUriParts {
                status_id: row.try_get("id")?,
                uri: uri.as_deref(),
                reblog: row.try_get("reblog")?,
                account_id: row.try_get("account_id")?,
                account_id_scheme: row.try_get("id_scheme")?,
                account_username: &username,
                account_domain: domain.as_deref(),
            },
        ));
    }
    Ok(uris)
}

/// `dump_likes!` and `dump_bookmarks!`: a collection of URIs, without a
/// count.
fn uri_collection(id: &str, uris: Vec<String>) -> AppResult<Vec<u8>> {
    Ok(serde_json::to_vec(&json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": id,
        "type": "OrderedCollection",
        "orderedItems": uris,
    }))
    .map_err(anyhow::Error::from)?)
}

/// `File.extname`.
fn extname(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(0) | None => "",
        Some(at) => &name[at..],
    }
}

/// `ActivityPub::ActorSerializer` through `ActivityPub::Adapter`: the
/// members and the `@context` upstream's serializer gives a local account.
/// What the two share is taken from the actor eunha federates.
async fn mastodon_actor(state: &AppState, account: &Account) -> AppResult<Value> {
    use crate::api::ap::context_helper::serialized_context;
    let domain = state.instance.domain.as_str();
    let base = crate::api::ap::objects::actor_json(state, domain, account).await?;
    let unavailable = account.is_unavailable();
    let actor_url = base["id"].as_str().unwrap_or_default().to_owned();

    // `virtual_tags`: the emoji of the profile, then its hashtags.
    let emojis: Vec<Value> = base["tag"].as_array().cloned().unwrap_or_default();
    let hashtags: Vec<Value> = if unavailable {
        Vec::new()
    } else {
        sqlx::query_scalar!(
            "SELECT t.name FROM accounts_tags at JOIN tags t ON t.id = at.tag_id
             WHERE at.account_id = $1 ORDER BY t.id",
            account.id,
        )
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .map(|name| {
            json!({
                "type": "Hashtag",
                "href": format!("https://{domain}/tags/{name}"),
                "name": format!("#{name}"),
            })
        })
        .collect()
    };
    let mut tag = emojis.clone();
    tag.extend(hashtags.iter().cloned());

    // `icon` and `image`, through `ImageSerializer` with the description.
    let image =
        |file_name: Option<&str>, content_type: &Option<String>, url: &Value, description: &str| {
            file_name.filter(|f| !f.is_empty() && !unavailable)?;
            let mut image = json!({ "type": "Image", "mediaType": content_type, "url": url });
            if !description.is_empty() {
                image["summary"] = json!(description);
            }
            Some(image)
        };
    let icon = image(
        account.avatar_file_name.as_deref(),
        &account.avatar_content_type,
        &base["icon"]["url"],
        &account.avatar_description,
    );
    let header = image(
        account.header_file_name.as_deref(),
        &account.header_content_type,
        &base["image"]["url"],
        &account.header_description,
    );

    let discoverable = account.discoverable.unwrap_or(false);
    let can_feature = if !discoverable {
        actor_url.clone()
    } else if account.locked {
        format!("{actor_url}/followers")
    } else {
        "https://www.w3.org/ns/activitystreams#Public".to_owned()
    };
    let name = if unavailable || account.display_name.is_empty() {
        account.username.clone()
    } else {
        account.display_name.clone()
    };

    let mut actor = json!({
        "id": actor_url,
        "webfinger": format!("{}@{domain}", account.username),
        "type": base["type"],
        "following": base["following"],
        "followers": base["followers"],
        "inbox": base["inbox"],
        "outbox": base["outbox"],
        "featured": base["featured"],
        "featuredTags": format!("{actor_url}/collections/tags"),
        "preferredUsername": account.username,
        "name": name,
        "summary": base["summary"],
        "url": base["url"],
        "manuallyApprovesFollowers": !unavailable && account.locked,
        "discoverable": !unavailable && discoverable,
        "indexable": !unavailable && account.indexable,
        // `created_at.midnight.iso8601`.
        "published": format!("{}T00:00:00Z", account.created_at.format("%Y-%m-%d")),
        "memorial": account.memorial,
        "showFeatured": account.show_featured,
        "showMedia": account.show_media,
        "showRepliesInMedia": account.show_media_replies,
        "interactionPolicy": { "canFeature": { "automaticApproval": [can_feature] } },
        "featuredCollections": format!("https://{domain}/ap/users/{}/featured_collections", account.id),
        "publicKey": base["publicKey"],
        "tag": tag,
        "attachment": base["attachment"],
        "endpoints": base["endpoints"],
    });
    if !unavailable && !base["movedTo"].is_null() {
        actor["movedTo"] = base["movedTo"].clone();
    }
    let also_known_as = account.also_known_as.clone().unwrap_or_default();
    if !unavailable && !also_known_as.is_empty() {
        actor["alsoKnownAs"] = json!(also_known_as);
    }
    if account.suspended_at.is_some() {
        actor["suspended"] = json!(true);
    }
    let attribution_domains = account.attribution_domains.clone().unwrap_or_default();
    if !attribution_domains.is_empty() {
        actor["attributionDomains"] = json!(attribution_domains);
    }
    if let Some(icon) = &icon {
        actor["icon"] = icon.clone();
    }
    if let Some(header) = &header {
        actor["image"] = header.clone();
    }

    // The contexts the serializer and the ones it nests declared, in the
    // order the adapter met them.
    let mut extensions = vec![
        "manually_approves_followers",
        "featured",
        "also_known_as",
        "moved_to",
        "property_value",
        "discoverable",
        "suspended",
        "memorial",
        "indexable",
        "attribution_domains",
        "profile_settings",
        "interaction_policies",
    ];
    if !emojis.is_empty() {
        extensions.extend(["emoji", "focal_point"]);
    }
    if !hashtags.is_empty() {
        extensions.push("hashtag");
    }
    if icon.is_some() || header.is_some() {
        extensions.push("focal_point");
    }
    actor["@context"] =
        serialized_context(&["activitystreams", "security", "webfinger"], &extensions);
    Ok(actor)
}

/// `dump_actor!`: the actor, its images in the zip beside it as `avatar.*`
/// and `header.*`, and its collections the zip's own files.
async fn dump_actor(state: &AppState, archive: &mut Archive, account: &Account) -> AppResult<()> {
    let mut actor = mastodon_actor(state, account).await?;
    if let Some(icon_url) = actor["icon"]["url"].as_str().map(str::to_owned) {
        actor["icon"]["url"] = json!(format!("avatar{}", extname(&icon_url)));
    }
    if let Some(image_url) = actor["image"]["url"].as_str().map(str::to_owned) {
        actor["image"]["url"] = json!(format!("header{}", extname(&image_url)));
    }
    actor["outbox"] = json!("outbox.json");
    actor["likes"] = json!("likes.json");
    actor["bookmarks"] = json!("bookmarks.json");

    // `account.avatar.exists?` and `account.header.exists?`: the account's
    // own files, whether or not the actor shows them.
    let partition = crate::media::int_to_path(account.id);
    if let Some(file_name) = account
        .avatar_file_name
        .as_deref()
        .filter(|f| !f.is_empty())
    {
        let key = format!("accounts/avatars/{partition}/original/{file_name}");
        download_to_zip(
            state,
            archive,
            &key,
            &format!("avatar{}", extname(file_name)),
        )
        .await?;
    }
    if let Some(file_name) = account
        .header_file_name
        .as_deref()
        .filter(|f| !f.is_empty())
    {
        let key = format!("accounts/headers/{partition}/original/{file_name}");
        download_to_zip(
            state,
            archive,
            &key,
            &format!("header{}", extname(file_name)),
        )
        .await?;
    }
    archive.entry(
        "actor.json",
        &serde_json::to_vec(&actor).map_err(anyhow::Error::from)?,
    )
}

// ── Vacuum::BackupsVacuum ────────────────────────────────────────────────

/// `Vacuum::BackupsVacuum` with `ContentRetentionPolicy#backups_retention_period`:
/// archives older than `backups_retention_period` days go, files and all.
/// A setting that is not a positive number of days keeps them.
pub async fn vacuum(state: &AppState) -> AppResult<u64> {
    let days = match crate::settings::get(state, "backups_retention_period").await {
        serde_yaml::Value::Number(n) => n.as_i64().filter(|d| *d > 0),
        _ => None,
    };
    let Some(days) = days else {
        return Ok(0);
    };
    let expired: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM backups WHERE created_at < now() - make_interval(days => $1)",
        days as i32,
    )
    .fetch_all(&state.db)
    .await?;
    let count = expired.len() as u64;
    for id in expired {
        destroy(state, id).await?;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_read_as_ruby_reads_them() {
        assert_eq!(extname("https://cdn.example/a/b/c.png"), ".png");
        assert_eq!(extname("abc"), "");
        assert_eq!(extname(".hidden"), "");
    }

    #[test]
    fn media_paths_are_cut_where_upstream_cuts_them() {
        assert_eq!(
            zip_path("public/system/media_attachments/files/1/original/a.png"),
            "media_attachments/files/1/original/a.png"
        );
        assert_eq!(
            zip_path("prefix/media_attachments/files/1/original/a.png"),
            "prefix/media_attachments/files/1/original/a.png"
        );
        assert_eq!(
            attachment_path("https://example.com/system/media_attachments/a.png").as_deref(),
            Some("media_attachments/a.png")
        );
        assert_eq!(
            attachment_path("https://files.example.com/media_attachments/a.png").as_deref(),
            Some("/media_attachments/a.png")
        );
    }

    #[test]
    fn dumps_are_stored_where_paperclip_puts_them() {
        assert_eq!(
            dump_key(12, "archive-x.zip"),
            "backups/dumps/000/000/012/original/archive-x.zip"
        );
        assert!(archive_filename().starts_with("archive-"));
    }
}
