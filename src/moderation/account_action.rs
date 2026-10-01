//! `Admin::AccountAction`: a moderator acting on an account — warning it,
//! disabling its login, marking its media sensitive, limiting it or suspending
//! it — with the strike, audit log entries, report resolutions and
//! notifications that come with it.

use crate::db::models::Account;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

use super::action_log::{self, Target};
use super::role::{self, authorize, flag, Role};
use super::warning::action as warning_action;

/// `Admin::AccountAction::TYPES`.
pub const TYPES: &[&str] = &["none", "disable", "sensitive", "silence", "suspend"];

/// The form `Api::V1::Admin::AccountActionsController#resource_params` permits.
#[derive(Debug, Clone)]
pub struct AccountAction {
    pub kind: Option<String>,
    pub report_id: Option<i64>,
    pub warning_preset_id: Option<i64>,
    pub text: Option<String>,
    /// `attribute :send_email_notification, :boolean, default: true`.
    pub send_email_notification: bool,
    /// `attribute :include_statuses, :boolean, default: true`.
    pub include_statuses: bool,
}

impl Default for AccountAction {
    fn default() -> Self {
        Self {
            kind: None,
            report_id: None,
            warning_preset_id: None,
            text: None,
            send_email_notification: true,
            include_statuses: true,
        }
    }
}

/// The user behind a local account: what `UserPolicy` judges.
struct UserRow {
    id: i64,
    email: String,
}

/// `Admin::AccountAction#save!`, by `actor_id` on `target`.
pub async fn save(
    state: &AppState,
    actor_id: i64,
    target: &Account,
    action: AccountAction,
) -> AppResult<()> {
    // `validates :type, presence: true, inclusion: { in: TYPES }`.
    let kind = match action.kind.as_deref().filter(|k| !k.is_empty()) {
        None => {
            return Err(AppError::Unprocessable(
                "Validation failed: Type can't be blank".into(),
            ))
        }
        Some(k) if TYPES.contains(&k) => k.to_owned(),
        Some(_) => {
            return Err(AppError::Unprocessable(
                "Validation failed: Type is not included in the list".into(),
            ))
        }
    };

    // `Report.find(report_id)` and `AccountWarningPreset.find(...)`.
    let report = match action.report_id {
        Some(id) => Some(
            sqlx::query!(
                r#"SELECT id, status_ids, category, rule_ids FROM reports WHERE id = $1"#,
                id
            )
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::NotFound)?,
        ),
        None => None,
    };
    let preset_text = match action.warning_preset_id {
        Some(id) => Some(
            sqlx::query_scalar!("SELECT text FROM account_warning_presets WHERE id = $1", id)
                .fetch_optional(&state.db)
                .await?
                .ok_or(AppError::NotFound)?,
        ),
        None => None,
    };
    // `text_for_warning`.
    let text = [preset_text, action.text.clone()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n\n");

    let acting = role::acting(&state.db, actor_id).await?;
    let target_role = role::of_account(&state.db, target.id).await?;
    let user = sqlx::query_as!(
        UserRow,
        "SELECT id, email FROM users WHERE account_id = $1",
        target.id,
    )
    .fetch_optional(&state.db)
    .await?;
    let acct = target.acct();

    let mut tx = state.db.begin().await?;

    // `handle_type!`.
    match kind.as_str() {
        "disable" => {
            // `authorize(target_account.user, :disable?)`; a remote account has
            // no user to disable.
            let user = user.as_ref().ok_or(AppError::Forbidden)?;
            authorize(acting.can(&[flag::MANAGE_USERS]) && acting.overrides(target_role.as_ref()))?;
            action_log::log(
                &mut *tx,
                actor_id,
                "disable",
                &Target::user(user.id, target.id, &acct),
            )
            .await?;
            sqlx::query!(
                "UPDATE users SET disabled = true, updated_at = now() WHERE id = $1",
                user.id
            )
            .execute(&mut *tx)
            .await?;
        }
        "sensitive" => {
            authorize(warn_policy(&acting, target_role.as_ref()))?;
            action_log::log(
                &mut *tx,
                actor_id,
                "sensitive",
                &Target::account(target.id, &acct),
            )
            .await?;
            sqlx::query!(
                "UPDATE accounts SET sensitized_at = now(), updated_at = now() WHERE id = $1",
                target.id
            )
            .execute(&mut *tx)
            .await?;
        }
        "silence" => {
            authorize(warn_policy(&acting, target_role.as_ref()))?;
            action_log::log(
                &mut *tx,
                actor_id,
                "silence",
                &Target::account(target.id, &acct),
            )
            .await?;
            sqlx::query!(
                "UPDATE accounts SET silenced_at = now(), updated_at = now() WHERE id = $1",
                target.id
            )
            .execute(&mut *tx)
            .await?;
        }
        "suspend" => {
            authorize(
                warn_policy(&acting, target_role.as_ref())
                    && target.id != crate::federation::instance_actor::INSTANCE_ACTOR_ID,
            )?;
            action_log::log(
                &mut *tx,
                actor_id,
                "suspend",
                &Target::account(target.id, &acct),
            )
            .await?;
        }
        _ => {}
    }

    // `process_strike!`.
    let status_ids: Option<Vec<String>> = report
        .as_ref()
        .filter(|_| action.include_statuses)
        .map(|r| r.status_ids.iter().map(i64::to_string).collect());
    let strike_action = warning_action::parse(&kind).unwrap_or(warning_action::NONE);
    let warning_id = sqlx::query_scalar!(
        r#"INSERT INTO account_warnings
             (account_id, target_account_id, report_id, action, text, status_ids, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, now(), now())
           RETURNING id"#,
        actor_id,
        target.id,
        action.report_id,
        strike_action,
        text,
        status_ids.as_deref(),
    )
    .fetch_one(&mut *tx)
    .await?;

    // `create_log!`: only a warning with words of its own is worth a line.
    if !text.is_empty() && kind == "none" {
        action_log::log(
            &mut *tx,
            actor_id,
            "create",
            &Target::account_warning(warning_id, &acct),
        )
        .await?;
    }

    // `process_reports!`: a plain warning resolves the one report it came
    // from; anything stronger resolves every open report about the account.
    let reports: Vec<i64> = if kind == "none" {
        report.as_ref().map(|r| vec![r.id]).unwrap_or_default()
    } else {
        sqlx::query_scalar!(
            "SELECT id FROM reports WHERE target_account_id = $1 AND action_taken_at IS NULL ORDER BY id",
            target.id,
        )
        .fetch_all(&mut *tx)
        .await?
    };
    for report_id in reports {
        authorize(acting.can(&[flag::MANAGE_REPORTS]))?;
        action_log::log(&mut *tx, actor_id, "resolve", &Target::report(report_id)).await?;
        sqlx::query!(
            r#"UPDATE reports SET action_taken_at = now(), action_taken_by_account_id = $2, updated_at = now()
               WHERE id = $1"#,
            report_id,
            actor_id,
        )
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    // After the transaction, as Mastodon's `suspend!` and `disable!` run inside
    // it but publish to Redis as they go.
    match kind.as_str() {
        "disable" => state.streaming.publish(crate::streaming::Event::Kill {
            account_id: target.id,
        }),
        "suspend" => {
            crate::delete_account::suspend(
                state,
                target.id,
                crate::delete_account::suspension_origin::LOCAL,
                true,
            )
            .await?;
        }
        _ => {}
    }

    // `process_notification!`.
    if action.send_email_notification && target.is_local() {
        notify(
            state,
            target,
            user.as_ref(),
            warning_id,
            &kind,
            &text,
            report
                .as_ref()
                .map(|r| (r.category, r.rule_ids.clone().unwrap_or_default())),
        )
        .await;
    }

    // `process_queue!`: `Admin::SuspensionWorker`.
    if kind == "suspend" {
        let state = state.clone();
        let id = target.id;
        crate::tenants::spawn(async move {
            if let Err(error) = super::suspension::suspend(&state, id).await {
                tracing::warn!(account_id = id, %error, "SuspendAccountService failed");
            }
        });
    }
    Ok(())
}

/// `AccountPolicy#warn?`, which `sensitive?`, `silence?` and `suspend?` share:
/// `manage_users` or `manage_reports`, over a lower role.
pub fn warn_policy(acting: &Role, target: Option<&Role>) -> bool {
    acting.can(&[flag::MANAGE_USERS, flag::MANAGE_REPORTS]) && acting.overrides(target)
}

/// `UserMailer.warning` and the `moderation_warning` notification.
async fn notify(
    state: &AppState,
    target: &Account,
    user: Option<&UserRow>,
    warning_id: i64,
    kind: &str,
    text: &str,
    report: Option<(i32, Vec<i64>)>,
) {
    crate::push::notify_local(
        state,
        target.id,
        "moderation_warning",
        "AccountWarning",
        warning_id,
        target.id,
    )
    .await;

    let Some(user) = user else {
        return;
    };
    let category = report
        .as_ref()
        .map(|(c, _)| crate::db::models::report_category::to_str(*c))
        .filter(|c| *c != "other");
    let rules: Vec<String> = match &report {
        Some((c, rule_ids))
            if *c == crate::db::models::report_category::VIOLATION && !rule_ids.is_empty() =>
        {
            sqlx::query_scalar!(
                "SELECT text FROM rules WHERE id = ANY($1) ORDER BY priority, id",
                rule_ids,
            )
            .fetch_all(&state.db)
            .await
            .unwrap_or_default()
        }
        _ => vec![],
    };
    let cited: Vec<String> = sqlx::query_scalar!(
        r#"SELECT s.id::text AS "id!" FROM account_warnings w
           CROSS JOIN LATERAL unnest(COALESCE(w.status_ids, '{}')) AS sid
           JOIN statuses s ON s.id = sid::bigint
           WHERE w.id = $1"#,
        warning_id,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|id| {
        format!(
            "https://{}/@{}/{id}",
            state.instance.domain, target.username
        )
    })
    .collect();
    let acct = format!("@{}@{}", target.username, state.instance.domain);
    let text = crate::email::html_escape(text).replace('\n', "<br>");
    let email = state.email.clone();
    let to = user.email.clone();
    let domain = state.instance.domain.clone();
    let kind = kind.to_owned();
    let category = category.map(str::to_owned);
    crate::tenants::spawn(async move {
        let reason = category.as_deref().map(|c| (c, rules.as_slice()));
        if let Err(error) = email
            .send_warning(&to, &acct, &domain, &kind, &text, reason, &cited)
            .await
        {
            tracing::warn!(%error, "could not send a moderation warning email");
        }
    });
}
