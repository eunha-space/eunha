//! `Admin::FollowRecommendationsController`: the server's follow
//! recommendations and the accounts kept out of them, for
//! `manage_taxonomies` (`FollowRecommendationPolicy`). Suppressing is not
//! logged, as upstream does not log it.

use axum::{
    extract::{Extension, Query},
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::{FlexIds, Params};
use crate::{
    error::AppResult, middleware::AuthenticatedUser, moderation::role::flag, state::AppState,
};

/// Kaminari's `default_per_page`.
const PER_PAGE: i64 = 40;

/// An account, its reasons, rank and language, and whether it is suppressed.
type RecommendationRow = (i64, Vec<String>, Option<f64>, Option<String>, bool);

#[derive(Debug, Serialize)]
pub struct AdminFollowRecommendation {
    pub account: super::super::types::Account,
    /// Why it is recommended: `most_followed`, `most_interactions`, or both;
    /// empty for a suppressed account.
    pub reason: Vec<String>,
    pub rank: Option<f64>,
    /// The language its posts are mostly in, from `account_summaries`.
    pub language: Option<String>,
    pub suppressed: bool,
}

#[derive(Debug, Deserialize, Default)]
pub struct FollowRecommendationFilter {
    pub language: Option<String>,
    pub status: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_int"
    )]
    pub page: Option<i64>,
}

async fn authorize(state: &AppState, auth: &AuthenticatedUser, write: bool) -> AppResult<()> {
    auth.require_scope(if write { "admin:write" } else { "admin:read" })?;
    super::require_permission(state, auth.account_id, flag::MANAGE_TAXONOMIES).await
}

/// `GET /api/v1/admin/follow_recommendations`: `FollowRecommendationFilter`.
/// The recommendations, those mostly in `language` first and then by rank;
/// with `status=suppressed`, the suppressed accounts, latest first.
pub async fn list_follow_recommendations(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(filter): Query<FollowRecommendationFilter>,
) -> AppResult<Json<Vec<AdminFollowRecommendation>>> {
    authorize(&state, &auth, false).await?;
    let page = filter.page.unwrap_or(1).max(1);
    let offset = (page - 1) * PER_PAGE;
    // `usable_language(params[:language] || I18n.locale)`, the moderator's.
    let language = match filter.language.filter(|l| !l.trim().is_empty()) {
        Some(language) => language,
        None => sqlx::query_scalar!(
            "SELECT locale FROM users WHERE account_id = $1",
            auth.account_id
        )
        .fetch_optional(&state.db)
        .await?
        .flatten()
        .filter(|l| !l.is_empty())
        .unwrap_or_else(|| state.instance.default_locale().to_owned()),
    };
    let language = language
        .split(['_', '-'])
        .next()
        .unwrap_or_default()
        .to_owned();

    let suppressed = filter.status.as_deref() == Some("suppressed");
    let rows: Vec<RecommendationRow> = if suppressed {
        sqlx::query_as(
            "SELECT x.account_id, '{}'::varchar[], NULL::float8, s.language, true
             FROM follow_recommendation_suppressions x
             LEFT JOIN account_summaries s ON s.account_id = x.account_id
             ORDER BY x.id DESC LIMIT $1 OFFSET $2",
        )
        .bind(PER_PAGE)
        .bind(offset)
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as(
            "SELECT g.account_id, g.reason, g.rank::float8, s.language, false
             FROM global_follow_recommendations g
             JOIN account_summaries s ON s.account_id = g.account_id
             WHERE NOT EXISTS (SELECT 1 FROM follow_recommendation_suppressions x
                               WHERE x.account_id = g.account_id)
             ORDER BY (s.language IS NOT DISTINCT FROM $3) DESC, g.rank DESC
             LIMIT $1 OFFSET $2",
        )
        .bind(PER_PAGE)
        .bind(offset)
        .bind(&language)
        .fetch_all(&state.db)
        .await?
    };
    let mut out = Vec::with_capacity(rows.len());
    for (account_id, reason, rank, language, suppressed) in rows {
        if let Some(account) = super::api_account(&state, account_id).await? {
            out.push(AdminFollowRecommendation {
                account,
                reason,
                rank,
                language,
                suppressed,
            });
        }
    }
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct AccountBatchForm {
    pub account_ids: Option<FlexIds>,
}

/// `POST /api/v1/admin/follow_recommendations/suppress`: `Form::AccountBatch`'s
/// `suppress_follow_recommendation`, keeping each account out of the
/// recommendations.
pub async fn suppress_follow_recommendations(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<AccountBatchForm>,
) -> AppResult<Json<serde_json::Value>> {
    authorize(&state, &auth, true).await?;
    let ids = form.account_ids.map(|i| i.0).unwrap_or_default();
    sqlx::query!(
        r#"INSERT INTO follow_recommendation_suppressions (account_id, created_at, updated_at)
           SELECT id, now(), now() FROM accounts WHERE id = ANY($1)
           ON CONFLICT (account_id) DO NOTHING"#,
        &ids,
    )
    .execute(&state.db)
    .await?;
    // The recommendations already computed lose them at once, as the
    // `unsupressed` scope leaves them out of every read.
    Ok(Json(serde_json::json!({})))
}

/// `POST /api/v1/admin/follow_recommendations/unsuppress`.
pub async fn unsuppress_follow_recommendations(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<AccountBatchForm>,
) -> AppResult<Json<serde_json::Value>> {
    authorize(&state, &auth, true).await?;
    let ids = form.account_ids.map(|i| i.0).unwrap_or_default();
    sqlx::query!(
        "DELETE FROM follow_recommendation_suppressions WHERE account_id = ANY($1)",
        &ids,
    )
    .execute(&state.db)
    .await?;
    Ok(Json(serde_json::json!({})))
}
