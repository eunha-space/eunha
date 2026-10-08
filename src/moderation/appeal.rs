//! `Appeal`: an account asking for a strike against it to be reconsidered,
//! with `AppealService` and `ApproveAppealService`.

use crate::error::{AppError, AppResult};
use crate::state::AppState;

use super::action_log::{self, Target};
use super::warning::{action, APPEAL_WINDOW};

/// `Appeal::TEXT_LENGTH_LIMIT`.
pub const TEXT_LENGTH_LIMIT: usize = 2_000;

struct Strike {
    id: i64,
    account_id: Option<i64>,
    target_account_id: Option<i64>,
    action: i32,
    status_ids: Option<Vec<String>>,
    created_at: chrono::NaiveDateTime,
}

async fn strike(state: &AppState, id: i64) -> AppResult<Strike> {
    sqlx::query_as!(
        Strike,
        r#"SELECT id, account_id, target_account_id, action, status_ids, created_at
           FROM account_warnings WHERE id = $1"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)
}

/// `AccountWarning#appeal_eligible?`.
pub fn appeal_eligible(created_at: chrono::NaiveDateTime) -> bool {
    created_at >= chrono::Utc::now().naive_utc() - APPEAL_WINDOW
}

/// `AppealService#call`: record the appeal and tell the staff who handle them.
/// The caller has already checked `AccountWarningPolicy#appeal?`.
pub async fn create(state: &AppState, strike_id: i64, text: Option<&str>) -> AppResult<i64> {
    let strike = strike(state, strike_id).await?;
    let target_id = strike.target_account_id.ok_or(AppError::NotFound)?;
    // `validates :text, presence: true, length: { maximum: TEXT_LENGTH_LIMIT }`,
    // `validates :account_warning_id, uniqueness: true` and
    // `validate :validate_time_frame, on: :create`, in that order.
    let text = text.unwrap_or_default();
    let mut errors = vec![];
    if text.trim().is_empty() {
        errors.push("Text can't be blank".to_owned());
    } else if text.chars().count() > TEXT_LENGTH_LIMIT {
        errors.push(format!(
            "Text is too long (maximum is {TEXT_LENGTH_LIMIT} characters)"
        ));
    }
    let taken = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM appeals WHERE account_warning_id = $1) AS "e!""#,
        strike_id
    )
    .fetch_one(&state.db)
    .await?;
    if taken {
        errors.push("Account warning has already been taken".to_owned());
    }
    if !appeal_eligible(strike.created_at) {
        errors.push("It is too late to appeal this strike".to_owned());
    }
    if !errors.is_empty() {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )));
    }
    let appeal_id = sqlx::query_scalar!(
        r#"INSERT INTO appeals (account_id, account_warning_id, text, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())
           RETURNING id"#,
        target_id,
        strike_id,
        text,
    )
    .fetch_one(&state.db)
    .await?;
    notify_staff(state, &strike, target_id, text).await;
    Ok(appeal_id)
}

/// `AppealService#notify_staff!`: `AdminMailer#new_appeal` to every user who
/// may handle appeals and has not turned appeal emails off.
async fn notify_staff(state: &AppState, strike: &Strike, target_id: i64, text: &str) {
    let state = state.clone();
    let text = text.to_owned();
    let strike_id = strike.id;
    let issuer_id = strike.account_id;
    let strike_created_at = strike.created_at;
    let kind = action::to_str(strike.action);
    async move {
        let staff =
            match crate::push::accounts_who_can(&state, &[super::role::flag::MANAGE_APPEALS]).await
            {
                Ok(staff) => staff,
                Err(error) => {
                    tracing::warn!(%error, "could not list staff for an appeal");
                    return;
                }
            };
        let username = |id: Option<i64>| {
            let state = state.clone();
            async move {
                match id {
                    Some(id) => {
                        sqlx::query_scalar!("SELECT username FROM accounts WHERE id = $1", id)
                            .fetch_optional(&state.db)
                            .await
                            .ok()
                            .flatten()
                            .unwrap_or_default()
                    }
                    None => String::new(),
                }
            }
        };
        let target = username(Some(target_id)).await;
        let issuer = username(issuer_id).await;
        for staff_id in staff {
            let Ok(Some(recipient)) = sqlx::query!(
                "SELECT email, settings FROM users WHERE account_id = $1",
                staff_id
            )
            .fetch_optional(&state.db)
            .await
            else {
                continue;
            };
            // `User#allows_appeal_emails?`.
            if !crate::accounts::user_setting_bool(
                recipient.settings.as_deref(),
                "notification_emails.appeal",
                true,
            ) {
                continue;
            }
            if let Err(error) = state
                .mailer()
                .send_new_appeal(
                    &recipient.email,
                    &state.instance.domain,
                    &target,
                    &issuer,
                    strike_created_at,
                    kind,
                    &text,
                    strike_id,
                )
                .await
            {
                tracing::warn!(%error, "could not send a new appeal email");
            }
        }
    }
    .await;
}

struct AppealRow {
    id: i64,
    account_warning_id: i64,
    approved_at: Option<chrono::NaiveDateTime>,
    rejected_at: Option<chrono::NaiveDateTime>,
    created_at: chrono::NaiveDateTime,
}

/// An appeal, and what its log entry is about.
async fn appeal(state: &AppState, id: i64) -> AppResult<(AppealRow, Target)> {
    let row = sqlx::query_as!(
        AppealRow,
        r#"SELECT id, account_warning_id, approved_at, rejected_at, created_at
           FROM appeals WHERE id = $1"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    let acct = sqlx::query!(
        r#"SELECT a.username, a.domain FROM appeals ap JOIN accounts a ON a.id = ap.account_id
           WHERE ap.id = $1"#,
        id
    )
    .fetch_optional(&state.db)
    .await?
    .map(|a| match a.domain {
        Some(domain) => format!("{}@{domain}", a.username),
        None => a.username,
    })
    .unwrap_or_default();
    let target = Target::appeal(row.id, acct, row.account_warning_id);
    Ok((row, target))
}

/// `AppealPolicy#approve?` and `#reject?` ask that the appeal be pending.
fn pending(row: &AppealRow) -> bool {
    row.approved_at.is_none() && row.rejected_at.is_none()
}

/// Whether the appeal exists and is still pending, for the policy check.
pub async fn is_pending(state: &AppState, id: i64) -> AppResult<bool> {
    Ok(pending(&appeal(state, id).await?.0))
}

/// `Admin::Disputes::AppealsController#approve`: log it, then
/// `ApproveAppealService` undoes what the strike did, marks the strike
/// overruled, and mails the account.
pub async fn approve(state: &AppState, id: i64, actor_id: i64) -> AppResult<()> {
    let (row, target) = appeal(state, id).await?;
    action_log::log(&state.db, actor_id, "approve", &target).await?;
    let strike = strike(state, row.account_warning_id).await?;
    let target_id = strike.target_account_id.ok_or(AppError::NotFound)?;

    let mut tx = state.db.begin().await?;
    // `undo_strike_action!`.
    match strike.action {
        // `target_account.user.enable!`
        action::DISABLE => {
            sqlx::query!(
                "UPDATE users SET disabled = false, updated_at = now() WHERE account_id = $1",
                target_id
            )
            .execute(&mut *tx)
            .await?;
        }
        // `unsensitize!` and `unsilence!`.
        action::SENSITIVE => {
            sqlx::query!(
                "UPDATE accounts SET sensitized_at = NULL, updated_at = now() WHERE id = $1",
                target_id
            )
            .execute(&mut *tx)
            .await?;
        }
        action::SILENCE => {
            sqlx::query!(
                "UPDATE accounts SET silenced_at = NULL, updated_at = now() WHERE id = $1",
                target_id
            )
            .execute(&mut *tx)
            .await?;
        }
        // `delete_statuses` cannot be undone; `suspend` and
        // `mark_statuses_as_sensitive` are undone below, outside the
        // transaction, as they reach beyond the database.
        _ => {}
    }
    // `mark_strike_as_appealed!`.
    sqlx::query!(
        r#"UPDATE appeals SET approved_at = now(), approved_by_account_id = $2, updated_at = now()
           WHERE id = $1"#,
        id,
        actor_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE account_warnings SET overruled_at = now(), updated_at = now() WHERE id = $1",
        strike.id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    match strike.action {
        action::SUSPEND => {
            // `target_account.unsuspend!`, then `Admin::UnsuspensionWorker`.
            crate::delete_account::unsuspend(state, target_id).await?;
            super::suspension::unsuspend_later(state, target_id).await;
        }
        action::MARK_STATUSES_AS_SENSITIVE => {
            unmark_statuses_as_sensitive(state, &strike).await?;
        }
        _ => {}
    }

    // `Account#trigger_update_webhooks` for what changed on the account; an
    // unsuspension's came from `delete_account::unsuspend`.
    if matches!(strike.action, action::SENSITIVE | action::SILENCE) {
        let local = sqlx::query_scalar!(
            r#"SELECT (domain IS NULL) AS "local!" FROM accounts WHERE id = $1"#,
            target_id
        )
        .fetch_optional(&state.db)
        .await?
        .unwrap_or(false);
        if local {
            super::webhooks::trigger(
                state,
                "account.updated",
                super::webhooks::Object::Account(target_id),
            )
            .await;
        }
    }

    mail_decision(state, target_id, true, row.created_at, strike.created_at).await;
    Ok(())
}

/// `undo_mark_statuses_as_sensitive!`: each cited post that is still there and
/// carries media is no longer sensitive, edited as the instance's
/// representative so other servers hear of it.
async fn unmark_statuses_as_sensitive(state: &AppState, strike: &Strike) -> AppResult<()> {
    let ids: Vec<i64> = strike
        .status_ids
        .iter()
        .flatten()
        .filter_map(|id| id.parse().ok())
        .collect();
    let statuses = sqlx::query_as!(
        crate::db::models::Status,
        r#"UPDATE statuses SET sensitive = false, updated_at = now()
           WHERE id = ANY($1) AND deleted_at IS NULL
             AND EXISTS (SELECT 1 FROM media_attachments m WHERE m.status_id = statuses.id)
           RETURNING *"#,
        &ids,
    )
    .fetch_all(&state.db)
    .await?;
    for status in statuses {
        // `UpdateStatusService` saved it: `status.updated` for a local post.
        super::webhooks::status_updated(state, status.id).await;
        let Some(account) = sqlx::query_as!(
            crate::db::models::Account,
            "SELECT * FROM accounts WHERE id = $1",
            status.account_id
        )
        .fetch_optional(&state.db)
        .await?
        else {
            continue;
        };
        if let Err(error) = crate::api::mastodon::statuses::federate_status_update(
            state, status.id, &account, &status,
        )
        .await
        {
            tracing::warn!(status_id = status.id, %error, "could not federate an unmarked post");
        }
    }
    Ok(())
}

/// `Admin::Disputes::AppealsController#reject`: log it, mark the appeal
/// rejected, and mail the account.
pub async fn reject(state: &AppState, id: i64, actor_id: i64) -> AppResult<()> {
    let (row, target) = appeal(state, id).await?;
    action_log::log(&state.db, actor_id, "reject", &target).await?;
    sqlx::query!(
        r#"UPDATE appeals SET rejected_at = now(), rejected_by_account_id = $2, updated_at = now()
           WHERE id = $1"#,
        id,
        actor_id
    )
    .execute(&state.db)
    .await?;
    let strike = strike(state, row.account_warning_id).await?;
    if let Some(target_id) = strike.target_account_id {
        mail_decision(state, target_id, false, row.created_at, strike.created_at).await;
    }
    Ok(())
}

/// `UserMailer.appeal_approved` and `UserMailer.appeal_rejected`, which say
/// nothing to an account without a user.
async fn mail_decision(
    state: &AppState,
    account_id: i64,
    approved: bool,
    appeal_created_at: chrono::NaiveDateTime,
    strike_created_at: chrono::NaiveDateTime,
) {
    let Ok(Some(user)) = sqlx::query!(
        "SELECT email, time_zone FROM users WHERE account_id = $1",
        account_id
    )
    .fetch_optional(&state.db)
    .await
    else {
        return;
    };
    let to = user.email;
    let time_zone = user.time_zone;
    let email = state.mailer();
    let domain = state.instance.domain.clone();
    {
        if let Err(error) = email
            .send_appeal_decided(
                &to,
                &domain,
                approved,
                appeal_created_at,
                strike_created_at,
                time_zone.as_deref(),
            )
            .await
        {
            tracing::warn!(%error, "could not send an appeal decision email");
        }
    }
}
