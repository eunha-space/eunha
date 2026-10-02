//! `Admin::AnnouncementsController` and its distribution, preview and test
//! (`Admin::Announcements::*Controller`), under `AnnouncementPolicy`.

use axum::{
    extract::{Extension, Path, Query},
    Json,
};
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

use super::super::extractors::{FlexBool, Params};
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::{
        action_log::{self, Target},
        role::{self, flag},
    },
    state::AppState,
};

/// An announcement as the admin pages show it.
#[derive(Debug, Serialize)]
pub struct AdminAnnouncement {
    pub id: String,
    pub text: String,
    /// The text as the public API renders it.
    pub content: String,
    pub published: bool,
    pub published_at: Option<String>,
    pub scheduled_at: Option<String>,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub all_day: bool,
    pub notification_sent_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

struct Row {
    id: i64,
    text: String,
    published: bool,
    published_at: Option<NaiveDateTime>,
    scheduled_at: Option<NaiveDateTime>,
    starts_at: Option<NaiveDateTime>,
    ends_at: Option<NaiveDateTime>,
    all_day: bool,
    notification_sent_at: Option<NaiveDateTime>,
    created_at: NaiveDateTime,
    updated_at: NaiveDateTime,
}

impl Row {
    fn into_api(self, domain: &str) -> AdminAnnouncement {
        let r = self;
        let date = super::super::convert::mastodon_date;
        AdminAnnouncement {
            id: r.id.to_string(),
            // `linkify`, without the database to look mentions up in.
            content: crate::formatter::text::format(
                &r.text,
                &crate::formatter::Options::new(domain),
            ),
            text: r.text,
            published: r.published,
            published_at: r.published_at.map(date),
            scheduled_at: r.scheduled_at.map(date),
            starts_at: r.starts_at.map(date),
            ends_at: r.ends_at.map(date),
            all_day: r.all_day,
            notification_sent_at: r.notification_sent_at.map(date),
            created_at: date(r.created_at),
            updated_at: date(r.updated_at),
        }
    }
}

async fn find(state: &AppState, id: i64) -> AppResult<Row> {
    sqlx::query_as!(
        Row,
        r#"SELECT id, text, published, published_at, scheduled_at, starts_at, ends_at, all_day,
                  notification_sent_at, created_at, updated_at
           FROM announcements WHERE id = $1"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

/// `AnnouncementPolicy#index?`, `create?`, `update?` and `destroy?`.
async fn authorize(state: &AppState, auth: &AuthenticatedUser, write: bool) -> AppResult<()> {
    auth.require_scope(if write { "admin:write" } else { "admin:read" })?;
    super::require_permission(state, auth.account_id, flag::MANAGE_ANNOUNCEMENTS).await
}

/// `AnnouncementPolicy#distribute?`: published, not yet mailed, and a role
/// that may manage settings.
async fn authorize_distribute(
    state: &AppState,
    auth: &AuthenticatedUser,
    scope: &str,
    id: i64,
) -> AppResult<Row> {
    auth.require_scope(scope)?;
    let row = find(state, id).await?;
    let acting = role::acting(&state.db, auth.account_id).await?;
    role::authorize(
        row.published && row.notification_sent_at.is_none() && acting.can(&[flag::MANAGE_SETTINGS]),
    )?;
    Ok(row)
}

fn target(row: &Row) -> Target {
    Target::announcement(row.id, &row.text)
}

#[derive(Debug, Deserialize, Default)]
pub struct AnnouncementFilter {
    pub published: Option<String>,
    pub unpublished: Option<String>,
}

/// `GET /api/v1/admin/announcements`: `AnnouncementFilter`, newest by
/// `coalesced_chronology_timestamps` first.
pub async fn list_admin_announcements(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(filter): Query<AnnouncementFilter>,
) -> AppResult<Json<Vec<AdminAnnouncement>>> {
    authorize(&state, &auth, false).await?;
    let present = |v: &Option<String>| v.as_deref().is_some_and(|v| !v.trim().is_empty());
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT id, text, published, published_at, scheduled_at, starts_at, ends_at, all_day,
                  notification_sent_at, created_at, updated_at
           FROM announcements
           WHERE (NOT $1 OR published) AND (NOT $2 OR NOT published)
           ORDER BY COALESCE(starts_at, scheduled_at, published_at, created_at) DESC"#,
        present(&filter.published),
        present(&filter.unpublished),
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| r.into_api(&state.urls.local_domain))
            .collect(),
    ))
}

/// `GET /api/v1/admin/announcements/:id`: `Announcements#edit`.
pub async fn get_admin_announcement(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAnnouncement>> {
    let row = find(&state, id).await?;
    authorize(&state, &auth, false).await?;
    Ok(Json(row.into_api(&state.urls.local_domain)))
}

#[derive(Debug, Deserialize)]
pub struct AnnouncementForm {
    pub text: Option<String>,
    pub scheduled_at: Option<String>,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub all_day: Option<FlexBool>,
}

/// A datetime as Rails casts a form's: blank is nil, and what does not parse
/// is nil too.
fn datetime(value: Option<&str>) -> Option<NaiveDateTime> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|d| d.naive_utc())
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S"))
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M"))
        .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(0, 0, 0).unwrap_or_default())
        })
        .ok()
}

/// What a save writes.
struct Draft {
    text: String,
    scheduled_at: Option<NaiveDateTime>,
    starts_at: Option<NaiveDateTime>,
    ends_at: Option<NaiveDateTime>,
    all_day: bool,
}

/// `Announcement`'s validations.
fn validate(form: AnnouncementForm, existing: Option<&Row>) -> AppResult<Draft> {
    let pick = |given: &Option<String>, current: Option<NaiveDateTime>| match given {
        Some(value) => datetime(Some(value)),
        None => current,
    };
    let draft = Draft {
        text: form
            .text
            .unwrap_or_else(|| existing.map(|r| r.text.clone()).unwrap_or_default()),
        scheduled_at: pick(&form.scheduled_at, existing.and_then(|r| r.scheduled_at)),
        starts_at: pick(&form.starts_at, existing.and_then(|r| r.starts_at)),
        ends_at: pick(&form.ends_at, existing.and_then(|r| r.ends_at)),
        all_day: form
            .all_day
            .map_or(existing.is_some_and(|r| r.all_day), |b| b.0),
    };
    let mut errors = vec![];
    if draft.text.trim().is_empty() {
        errors.push("Text can't be blank");
    }
    if draft.ends_at.is_some() && draft.starts_at.is_none() {
        errors.push("Starts at can't be blank");
    }
    if draft.starts_at.is_some() && draft.ends_at.is_none() {
        errors.push("Ends at can't be blank");
    }
    if errors.is_empty() {
        Ok(draft)
    } else {
        Err(AppError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )))
    }
}

/// `POST /api/v1/admin/announcements`: `Announcements#create`. It is published
/// at once unless it is scheduled for later (`set_published`).
pub async fn create_admin_announcement(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<AnnouncementForm>,
) -> AppResult<Json<AdminAnnouncement>> {
    authorize(&state, &auth, true).await?;
    let draft = validate(form, None)?;
    let publish_now = draft
        .scheduled_at
        .is_none_or(|at| at <= chrono::Utc::now().naive_utc());
    let mut tx = state.db.begin().await?;
    let row = sqlx::query_as!(
        Row,
        r#"INSERT INTO announcements
             (text, scheduled_at, starts_at, ends_at, all_day, published, published_at,
              created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $6 THEN now() END, now(), now())
           RETURNING id, text, published, published_at, scheduled_at, starts_at, ends_at,
                     all_day, notification_sent_at, created_at, updated_at"#,
        draft.text,
        draft.scheduled_at,
        draft.starts_at,
        draft.ends_at,
        draft.all_day,
        publish_now,
    )
    .fetch_one(&mut *tx)
    .await?;
    action_log::log(&mut *tx, auth.account_id, "create", &target(&row)).await?;
    tx.commit().await?;
    if row.published {
        crate::announcements::publish_later(&state, row.id);
    }
    Ok(Json(row.into_api(&state.urls.local_domain)))
}

/// `PATCH /api/v1/admin/announcements/:id`: `Announcements#update`. A
/// published one is sent to the streams again.
pub async fn update_admin_announcement(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<AnnouncementForm>,
) -> AppResult<Json<AdminAnnouncement>> {
    let existing = find(&state, id).await?;
    authorize(&state, &auth, true).await?;
    let draft = validate(form, Some(&existing))?;
    let mut tx = state.db.begin().await?;
    let row = sqlx::query_as!(
        Row,
        r#"UPDATE announcements
           SET text = $2, scheduled_at = $3, starts_at = $4, ends_at = $5, all_day = $6,
               updated_at = now()
           WHERE id = $1
           RETURNING id, text, published, published_at, scheduled_at, starts_at, ends_at,
                     all_day, notification_sent_at, created_at, updated_at"#,
        id,
        draft.text,
        draft.scheduled_at,
        draft.starts_at,
        draft.ends_at,
        draft.all_day,
    )
    .fetch_one(&mut *tx)
    .await?;
    action_log::log(&mut *tx, auth.account_id, "update", &target(&row)).await?;
    tx.commit().await?;
    if row.published {
        crate::announcements::publish_later(&state, row.id);
    }
    Ok(Json(row.into_api(&state.urls.local_domain)))
}

/// `POST /api/v1/admin/announcements/:id/publish`: `publish!`, logged as an
/// update.
pub async fn publish_admin_announcement(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAnnouncement>> {
    find(&state, id).await?;
    authorize(&state, &auth, true).await?;
    let mut tx = state.db.begin().await?;
    let row = sqlx::query_as!(
        Row,
        r#"UPDATE announcements
           SET published = true, published_at = now(), scheduled_at = NULL, updated_at = now()
           WHERE id = $1
           RETURNING id, text, published, published_at, scheduled_at, starts_at, ends_at,
                     all_day, notification_sent_at, created_at, updated_at"#,
        id,
    )
    .fetch_one(&mut *tx)
    .await?;
    action_log::log(&mut *tx, auth.account_id, "update", &target(&row)).await?;
    tx.commit().await?;
    crate::announcements::publish_later(&state, id);
    Ok(Json(row.into_api(&state.urls.local_domain)))
}

/// `POST /api/v1/admin/announcements/:id/unpublish`: `unpublish!`, logged as
/// an update.
pub async fn unpublish_admin_announcement(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAnnouncement>> {
    find(&state, id).await?;
    authorize(&state, &auth, true).await?;
    let mut tx = state.db.begin().await?;
    let row = sqlx::query_as!(
        Row,
        r#"UPDATE announcements
           SET published = false, scheduled_at = NULL, updated_at = now()
           WHERE id = $1
           RETURNING id, text, published, published_at, scheduled_at, starts_at, ends_at,
                     all_day, notification_sent_at, created_at, updated_at"#,
        id,
    )
    .fetch_one(&mut *tx)
    .await?;
    action_log::log(&mut *tx, auth.account_id, "update", &target(&row)).await?;
    tx.commit().await?;
    crate::announcements::unpublish(&state, id).await;
    Ok(Json(row.into_api(&state.urls.local_domain)))
}

/// `DELETE /api/v1/admin/announcements/:id`: `destroy!`, with its dismissals
/// and reactions.
pub async fn delete_admin_announcement(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let row = find(&state, id).await?;
    authorize(&state, &auth, true).await?;
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "DELETE FROM announcement_mutes WHERE announcement_id = $1",
        id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM announcement_reactions WHERE announcement_id = $1",
        id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM announcements WHERE id = $1", id)
        .execute(&mut *tx)
        .await?;
    action_log::log(&mut *tx, auth.account_id, "destroy", &target(&row)).await?;
    tx.commit().await?;
    if row.published {
        crate::announcements::unpublish(&state, id).await;
    }
    Ok(Json(serde_json::json!({})))
}

/// `Announcement#scope_for_notification`: confirmed users whose account is
/// not suspended.
async fn recipients(state: &AppState) -> AppResult<Vec<String>> {
    Ok(sqlx::query_scalar!(
        r#"SELECT u.email FROM users u JOIN accounts a ON a.id = u.account_id
           WHERE u.confirmed_at IS NOT NULL AND a.suspended_at IS NULL
           ORDER BY u.id"#
    )
    .fetch_all(&state.db)
    .await?)
}

#[derive(Debug, Serialize)]
pub struct AnnouncementPreview {
    pub announcement: AdminAnnouncement,
    /// `scope_for_notification.count`: how many would be mailed.
    pub user_count: i64,
}

/// `GET /api/v1/admin/announcements/:id/preview`: `Previews#show`.
pub async fn preview_admin_announcement(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AnnouncementPreview>> {
    let row = authorize_distribute(&state, &auth, "admin:read", id).await?;
    let user_count = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM users u JOIN accounts a ON a.id = u.account_id
           WHERE u.confirmed_at IS NOT NULL AND a.suspended_at IS NULL"#
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(AnnouncementPreview {
        announcement: row.into_api(&state.urls.local_domain),
        user_count,
    }))
}

async fn mail(state: &AppState, to: &str, text: &str) {
    let text = super::super::formatting::linkify(state, text).await;
    if let Err(error) = state
        .email
        .send_announcement_published(to, &state.instance.domain, &text)
        .await
    {
        tracing::warn!(%error, "could not mail an announcement");
    }
}

/// `POST /api/v1/admin/announcements/:id/test`: `Tests#create`, the mail to
/// the acting user alone.
pub async fn test_admin_announcement(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let row = authorize_distribute(&state, &auth, "admin:write", id).await?;
    let user = sqlx::query_scalar!(
        "SELECT email FROM users WHERE account_id = $1",
        auth.account_id
    )
    .fetch_one(&state.db)
    .await?;
    let state = state.clone();
    crate::tenants::spawn(async move {
        mail(&state, &user, &row.text).await;
    });
    Ok(Json(serde_json::json!({})))
}

/// `POST /api/v1/admin/announcements/:id/distribution`: `Distributions#create`,
/// recording that users were told, then mailing every one of them.
pub async fn distribute_admin_announcement(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<AdminAnnouncement>> {
    authorize_distribute(&state, &auth, "admin:write", id).await?;
    // `touch(:notification_sent_at)` sets `updated_at` too.
    sqlx::query!(
        "UPDATE announcements SET notification_sent_at = now(), updated_at = now() WHERE id = $1",
        id
    )
    .execute(&state.db)
    .await?;
    let row = find(&state, id).await?;
    let text = row.text.clone();
    let to = recipients(&state).await?;
    let background = state.clone();
    crate::tenants::spawn(async move {
        for email in to {
            mail(&background, &email, &text).await;
        }
    });
    Ok(Json(row.into_api(&state.urls.local_domain)))
}
