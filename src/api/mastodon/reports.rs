use axum::{extract::Extension, Json};
use serde::Deserialize;

use super::{
    accounts::{batch_account_roles, fetch_account_emojis},
    convert::account_from_db,
    types::Report,
};
use crate::middleware::ResolvedInstance;
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    push::notify_admins,
    state::AppState,
};

fn de_i64_from_str_or_num<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StrOrNum {
        S(String),
        N(i64),
    }
    match StrOrNum::deserialize(d)? {
        StrOrNum::S(s) => s.parse().map_err(serde::de::Error::custom),
        StrOrNum::N(n) => Ok(n),
    }
}

// ── POST /api/v1/reports ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ReportForm {
    #[serde(deserialize_with = "de_i64_from_str_or_num")]
    pub account_id: i64,
    pub status_ids: Option<Vec<String>>,
    pub comment: Option<String>,
    pub forward: Option<bool>,
    pub category: Option<String>,
    pub rule_ids: Option<Vec<String>>,
}

pub async fn file_report(
    state: AppState,
    Extension(ResolvedInstance(_instance)): Extension<crate::middleware::ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<ReportForm>,
) -> AppResult<Json<Report>> {
    auth.require_scope("write:reports")?;
    let target_account = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = $1",
        form.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let status_ids: Vec<i64> = form
        .status_ids
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s.parse::<i64>().ok())
        .collect();

    // Verify all provided statuses belong to the target account.
    for &sid in &status_ids {
        let belongs = sqlx::query_scalar!(
            "SELECT 1 as e FROM statuses WHERE id = $1 AND account_id = $2 AND deleted_at IS NULL",
            sid,
            form.account_id,
        )
        .fetch_optional(&state.db)
        .await?;
        if belongs.is_none() {
            return Err(AppError::NotFound);
        }
    }

    let comment = form.comment.unwrap_or_default();
    // Mastodon Report::COMMENT_SIZE_LIMIT (local reports).
    if comment.chars().count() > 1_000 {
        return Err(AppError::Unprocessable(
            "Validation failed: Comment is too long (maximum is 1000 characters)".into(),
        ));
    }
    let forwarded = form.forward.unwrap_or(false);
    let rule_ids: Vec<i64> = form
        .rule_ids
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s.parse::<i64>().ok())
        .collect();
    // If rule_ids are provided, Mastodon forces category to 'violation'
    let category = if !rule_ids.is_empty() {
        "violation".to_string()
    } else {
        form.category.unwrap_or_else(|| "other".into())
    };
    let category_int = crate::db::models::report_category::parse(&category)
        .ok_or_else(|| AppError::Unprocessable(format!("'{category}' is not a valid category")))?;

    let report = sqlx::query!(
        r#"INSERT INTO reports (account_id, target_account_id, status_ids, comment, forwarded, category, rule_ids, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, now(), now())
           RETURNING id, created_at"#,
        auth.account_id,
        form.account_id,
        &status_ids,
        comment,
        forwarded,
        category_int,
        &rule_ids,
    )
    .fetch_one(&state.db)
    .await?;

    let status_id_strings: Vec<String> = status_ids.iter().map(|id| id.to_string()).collect();
    let rule_id_strings: Vec<String> = rule_ids.iter().map(|id| id.to_string()).collect();

    // Notify admins/moderators about the new report.
    {
        let state2 = state.clone();
        let reporter_id = auth.account_id;
        let rid = report.id;
        crate::tenants::spawn(async move {
            notify_admins(&state2, reporter_id, "admin.report", Some(rid)).await;
        });
    }

    let mut ta_api = account_from_db(&state.urls, &target_account);
    ta_api.emojis = fetch_account_emojis(&state, &target_account).await;
    ta_api.roles = {
        let m = batch_account_roles(&state, std::slice::from_ref(&target_account)).await;
        m.get(&target_account.id).cloned().unwrap_or_default()
    };
    super::accounts::apply_account_stats(&state, &mut ta_api, target_account.id).await;
    Ok(Json(Report {
        id: report.id.to_string(),
        action_taken: false,
        action_taken_at: None,
        category,
        comment,
        forwarded,
        created_at: super::convert::mastodon_date(report.created_at),
        status_ids: status_id_strings,
        rule_ids: rule_id_strings,
        collection_ids: vec![],
        target_account: ta_api,
    }))
}
