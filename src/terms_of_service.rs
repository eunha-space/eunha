//! Mastodon's `TermsOfService`: versions of the instance's terms, each a draft
//! until an administrator publishes it, and live from its effective date.
//!
//! Before the table existed eunha served one text from the instance
//! configuration's `terms_of_service`. It is read no longer;
//! `eunha settings import-config` publishes it once as a version effective on
//! 2025-01-01, the date eunha served it as effective from.

use chrono::{Duration, NaiveDate, NaiveDateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;

use crate::{
    error::{AppError, AppResult},
    state::AppState,
};

/// `TermsOfService::NOTIFICATION_ACTIVITY_CUTOFF`.
const NOTIFICATION_ACTIVITY_CUTOFF_DAYS: i64 = 365;

/// The date the configured text was served as effective from, which
/// `eunha settings import-config` publishes it as.
pub fn config_effective_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(2025, 1, 1).expect("valid date")
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TermsOfService {
    /// 0 for an unsaved draft.
    pub id: i64,
    pub text: String,
    pub changelog: String,
    pub published_at: Option<NaiveDateTime>,
    pub notification_sent_at: Option<NaiveDateTime>,
    pub effective_date: Option<NaiveDate>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// The time Rails compares against, in UTC as the columns store it.
fn now() -> NaiveDateTime {
    Utc::now().naive_utc()
}

/// `Time.zone.today`, with Rails' default zone.
pub fn today() -> NaiveDate {
    now().date()
}

impl TermsOfService {
    /// An unsaved draft, as `TermsOfService.new` builds one.
    pub fn new_draft(text: String, effective_date: Option<NaiveDate>) -> Self {
        let now = now();
        Self {
            id: 0,
            text,
            changelog: String::new(),
            published_at: None,
            notification_sent_at: None,
            effective_date,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn published(&self) -> bool {
        self.published_at.is_some()
    }

    /// `effective?`: published, with an effective date that is `past?` —
    /// strictly before today, so a version going live today is live but not
    /// yet "effective".
    pub fn effective(&self) -> bool {
        self.published() && self.effective_date.is_some_and(|d| d < today())
    }

    pub fn usable_effective_date(&self) -> NaiveDate {
        self.effective_date.unwrap_or_else(today)
    }

    pub fn notification_sent(&self) -> bool {
        self.notification_sent_at.is_some()
    }
}

const COLUMNS: &str = "id, text, changelog, published_at, notification_sent_at, effective_date, \
                       created_at, updated_at";

/// `published`'s order.
const PUBLISHED_ORDER: &str = "COALESCE(effective_date, published_at) DESC";

async fn first(db: &PgPool, condition: &str, order: &str) -> AppResult<Option<TermsOfService>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM terms_of_services WHERE {condition} ORDER BY {order} LIMIT 1"
    );
    let query = sqlx::query_as::<_, TermsOfService>(&sql);
    let query = if condition.contains("$1") {
        query.bind(now())
    } else {
        query
    };
    Ok(query.fetch_optional(db).await?)
}

/// `live`: published, and effective before now. `$1` is now.
const LIVE: &str = "published_at IS NOT NULL AND (effective_date IS NULL OR effective_date < $1)";
/// `upcoming`: published, effective after now.
const UPCOMING: &str =
    "published_at IS NOT NULL AND effective_date IS NOT NULL AND effective_date > $1";

/// `TermsOfService.live.first`.
pub async fn live_first(state: &AppState) -> AppResult<Option<TermsOfService>> {
    first(&state.db, LIVE, PUBLISHED_ORDER).await
}

/// `TermsOfService.current`: the live version, or for terms none of which is
/// in effect yet, the first to come.
pub async fn current(state: &AppState) -> AppResult<Option<TermsOfService>> {
    if let Some(tos) = first(&state.db, LIVE, PUBLISHED_ORDER).await? {
        return Ok(Some(tos));
    }
    first(&state.db, UPCOMING, "effective_date ASC").await
}

/// `TermsOfService.published.first`: the latest, whether or not in effect.
pub async fn published_first(state: &AppState) -> AppResult<Option<TermsOfService>> {
    first(&state.db, "published_at IS NOT NULL", PUBLISHED_ORDER).await
}

/// `TermsOfService.published.all`.
pub async fn published_all(state: &AppState) -> AppResult<Vec<TermsOfService>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM terms_of_services WHERE published_at IS NOT NULL \
         ORDER BY {PUBLISHED_ORDER}"
    );
    Ok(sqlx::query_as::<_, TermsOfService>(&sql)
        .fetch_all(&state.db)
        .await?)
}

/// `TermsOfService.published.find_by!(effective_date:)`. A date Rails cannot
/// cast is nil, and finds a version published without one.
pub async fn published_by_date(state: &AppState, date: &str) -> AppResult<TermsOfService> {
    let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok();
    let sql = format!(
        "SELECT {COLUMNS} FROM terms_of_services WHERE published_at IS NOT NULL \
         AND effective_date IS NOT DISTINCT FROM $1 ORDER BY {PUBLISHED_ORDER} LIMIT 1"
    );
    sqlx::query_as::<_, TermsOfService>(&sql)
        .bind(date)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

/// `TermsOfService.find(id)`.
pub async fn find(db: &PgPool, id: i64) -> AppResult<TermsOfService> {
    let sql = format!("SELECT {COLUMNS} FROM terms_of_services WHERE id = $1");
    sqlx::query_as::<_, TermsOfService>(&sql)
        .bind(id)
        .fetch_optional(db)
        .await?
        .ok_or(AppError::NotFound)
}

/// `TermsOfService.draft.first`.
pub async fn draft_first(db: &PgPool) -> AppResult<Option<TermsOfService>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM terms_of_services WHERE published_at IS NULL \
         ORDER BY id DESC LIMIT 1"
    );
    Ok(sqlx::query_as::<_, TermsOfService>(&sql)
        .fetch_optional(db)
        .await?)
}

/// `succeeded_by`: of the published versions effective on or after this one,
/// other than itself, the first in `published` order — which, that order being
/// newest first, is the latest of them rather than the next.
pub async fn succeeded_by(db: &PgPool, tos: &TermsOfService) -> AppResult<Option<NaiveDate>> {
    let sql = format!(
        "SELECT {COLUMNS} FROM terms_of_services WHERE published_at IS NOT NULL \
         AND ($1::date IS NULL OR effective_date >= $1) AND id <> $2 \
         ORDER BY {PUBLISHED_ORDER} LIMIT 1"
    );
    Ok(sqlx::query_as::<_, TermsOfService>(&sql)
        .bind(tos.effective_date)
        .bind(tos.id)
        .fetch_optional(db)
        .await?
        .and_then(|t| t.effective_date))
}

/// `REST::TermsOfServiceSerializer`.
#[derive(Debug, Serialize)]
pub struct Rest {
    pub effective_date: String,
    pub effective: bool,
    pub content: String,
    pub succeeded_by: Option<String>,
}

/// `Date#iso8601`, or for a version without one, `published_at`'s
/// `Time#iso8601`.
fn effective_date_iso(tos: &TermsOfService) -> String {
    match (tos.effective_date, tos.published_at) {
        (Some(date), _) => date.to_string(),
        (None, Some(at)) => at.and_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        (None, None) => String::new(),
    }
}

pub async fn serialize(state: &AppState, tos: &TermsOfService) -> AppResult<Rest> {
    Ok(Rest {
        effective_date: effective_date_iso(tos),
        effective: tos.effective(),
        content: crate::markdown::render_policy(&tos.text, &state.instance.domain)
            .map_err(AppError::Unrescued)?,
        succeeded_by: succeeded_by(&state.db, tos).await?.map(|d| d.to_string()),
    })
}

// ── Saving ─────────────────────────────────────────────────────────────────

/// The model's validations, as `errors.full_messages`.
pub async fn validate(db: &PgPool, tos: &TermsOfService) -> AppResult<Vec<String>> {
    let mut errors = Vec::new();
    // `validates :text, presence: true`: blank is empty or whitespace.
    if tos.text.trim().is_empty() {
        errors.push("Text can't be blank".to_owned());
    }
    if tos.published() {
        if tos.changelog.trim().is_empty() {
            errors.push("Changelog can't be blank".to_owned());
        }
        if tos.effective_date.is_none() {
            errors.push("Effective date can't be blank".to_owned());
        }
    }
    // `validates :effective_date, uniqueness: true`, which without
    // `allow_nil` holds nil to it too.
    let taken = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM terms_of_services
             WHERE effective_date IS NOT DISTINCT FROM $1 AND id <> $2
           ) AS "e!""#,
        tos.effective_date,
        tos.id,
    )
    .fetch_one(db)
    .await?;
    if taken {
        errors.push("Effective date has already been taken".to_owned());
    }
    // `effective_date_cannot_be_in_the_past`.
    if let Some(date) = tos.effective_date {
        let live = first(db, LIVE, PUBLISHED_ORDER).await?;
        let min_date = live.and_then(|t| t.effective_date).unwrap_or_else(today);
        if date < min_date {
            errors.push(format!(
                "Effective date is too soon, must be later than {min_date}"
            ));
        }
    }
    Ok(errors)
}

fn invalid(errors: Vec<String>) -> AppError {
    AppError::Unprocessable(format!("Validation failed: {}", errors.join(", ")))
}

/// `save`: validates, then inserts or updates. Returns the saved row.
pub async fn save(db: &PgPool, mut tos: TermsOfService) -> AppResult<TermsOfService> {
    let errors = validate(db, &tos).await?;
    if !errors.is_empty() {
        return Err(invalid(errors));
    }
    let now = now();
    tos.updated_at = now;
    let saved = if tos.id == 0 {
        tos.created_at = now;
        sqlx::query_scalar!(
            r#"INSERT INTO terms_of_services
                 (text, changelog, published_at, notification_sent_at, effective_date,
                  created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $6)
               RETURNING id"#,
            tos.text,
            tos.changelog,
            tos.published_at,
            tos.notification_sent_at,
            tos.effective_date,
            now,
        )
        .fetch_one(db)
        .await
    } else {
        sqlx::query_scalar!(
            r#"UPDATE terms_of_services
               SET text = $2, changelog = $3, published_at = $4, effective_date = $5,
                   updated_at = $6
               WHERE id = $1
               RETURNING id"#,
            tos.id,
            tos.text,
            tos.changelog,
            tos.published_at,
            tos.effective_date,
            now,
        )
        .fetch_one(db)
        .await
    };
    // The unique index on `effective_date` is the last word on a race the
    // validation lost.
    let id = match saved {
        Ok(id) => id,
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(invalid(vec![
                "Effective date has already been taken".to_owned()
            ]))
        }
        Err(e) => return Err(e.into()),
    };
    find(db, id).await
}

// ── Notifying users ───────────────────────────────────────────────────────

/// A user `scope_for_notification` or `scope_for_interstitial` selects.
#[derive(Debug, Clone)]
pub struct Recipient {
    pub user_id: i64,
    pub email: String,
    pub locale: Option<String>,
}

fn cutoff(tos: &TermsOfService) -> NaiveDateTime {
    tos.published_at.unwrap_or_else(now) - Duration::days(NOTIFICATION_ACTIVITY_CUTOFF_DAYS)
}

/// `scope_for_notification`: confirmed users who signed up before the
/// version was published, whose account is not suspended, and who have signed
/// in within the year before it.
pub async fn scope_for_notification(
    db: &PgPool,
    tos: &TermsOfService,
) -> AppResult<Vec<Recipient>> {
    let rows = sqlx::query!(
        r#"SELECT u.id, u.email, u.locale FROM users u JOIN accounts a ON a.id = u.account_id
           WHERE u.confirmed_at IS NOT NULL AND u.created_at <= $1
             AND a.suspended_at IS NULL AND u.current_sign_in_at >= $2
           ORDER BY u.id"#,
        tos.published_at,
        cutoff(tos),
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Recipient {
            user_id: r.id,
            email: r.email,
            locale: r.locale,
        })
        .collect())
}

/// `scope_for_notification.count`.
pub async fn notification_count(db: &PgPool, tos: &TermsOfService) -> AppResult<i64> {
    Ok(scope_for_notification(db, tos).await?.len() as i64)
}

/// `scope_for_interstitial.update_all(require_tos_interstitial: true)`: the
/// rest of the users who signed up before it — suspended, or not seen for a
/// year — are shown the terms when they next come back instead of mailed.
pub async fn flag_for_interstitial(db: &PgPool, tos: &TermsOfService) -> AppResult<u64> {
    Ok(sqlx::query!(
        r#"UPDATE users u SET require_tos_interstitial = true
           FROM accounts a
           WHERE a.id = u.account_id AND u.confirmed_at IS NOT NULL AND u.created_at <= $1
             AND (a.suspended_at IS NOT NULL
                  OR u.current_sign_in_at IS NULL OR u.current_sign_in_at < $2)"#,
        tos.published_at,
        cutoff(tos),
    )
    .execute(db)
    .await?
    .rows_affected())
}

/// `UserMailer#terms_of_service_changed` to one user.
pub async fn send_changed_email(state: &AppState, to: &str, tos: &TermsOfService) {
    let domain = &state.instance.domain;
    let date = tos.usable_effective_date();
    let url = match tos.effective_date {
        Some(d) => format!("https://{domain}/terms-of-service/{d}"),
        None => format!("https://{domain}/terms-of-service"),
    };
    let changelog = crate::markdown::render(&tos.changelog);
    if let Err(error) = state
        .mailer()
        .send_terms_of_service_changed(
            to,
            domain,
            &date.format("%b %d, %Y").to_string(),
            &url,
            &changelog,
        )
        .await
    {
        tracing::warn!(%error, "could not send a terms of service email");
    }
}

/// `Admin::DistributeTermsOfServiceNotificationWorker.perform_async`.
pub async fn distribute(state: &AppState, tos: TermsOfService) -> AppResult<()> {
    crate::jobs::perform_async(
        state,
        DistributeNotificationWorker {
            terms_of_service_id: tos.id,
        },
    )
    .await
    .map_err(AppError::Internal)?;
    Ok(())
}

/// `Admin::DistributeTermsOfServiceNotificationWorker`: flag the users the
/// interstitial is for, then mail the rest.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct DistributeNotificationWorker {
    pub terms_of_service_id: i64,
}

impl crate::jobs::Job for DistributeNotificationWorker {
    const KIND: &'static str = "Admin::DistributeTermsOfServiceNotificationWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT;

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        let tos = match find(&state.db, self.terms_of_service_id).await {
            Ok(tos) => tos,
            Err(AppError::NotFound) => return Ok(()),
            Err(error) => return Err(anyhow::anyhow!("{error:?}")),
        };
        // `on_start`.
        flag_for_interstitial(&state.db, &tos)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        // `push_bulk_mailer(UserMailer, :terms_of_service_changed, ...)`.
        for recipient in scope_for_notification(&state.db, &tos)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
        {
            send_changed_email(state, &recipient.email, &tos).await;
        }
        Ok(())
    }
}
