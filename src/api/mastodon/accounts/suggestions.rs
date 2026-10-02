//! Follow suggestions (`/api/v1` and `/api/v2`) and dismissing a suggestion,
//! from [`crate::suggestions`].

use super::*;

#[derive(Debug, serde::Deserialize)]
pub struct SuggestionsParams {
    pub limit: Option<String>,
    pub offset: Option<String>,
}

impl SuggestionsParams {
    /// `limit_param(DEFAULT_ACCOUNTS_LIMIT)`, at most `MAX_LIMIT`, and the
    /// v2 `offset`.
    fn window(&self) -> (usize, usize) {
        let limit = self
            .limit
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(40)
            .clamp(1, 80) as usize;
        let offset = self
            .offset
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0)
            .max(0) as usize;
        (limit, offset)
    }
}

/// The suggested accounts, in the order suggested, with their sources.
async fn suggested(
    state: &AppState,
    viewer: i64,
    limit: usize,
    offset: usize,
) -> AppResult<Vec<(Account, Vec<String>)>> {
    let found = crate::suggestions::get(state, viewer, limit, offset)
        .await
        .map_err(AppError::Internal)?;
    let ids: Vec<i64> = found.iter().map(|(id, _)| *id).collect();
    let mut accounts: std::collections::HashMap<i64, Account> =
        sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = ANY($1)", &ids)
            .fetch_all(&state.db)
            .await?
            .into_iter()
            .map(|a| (a.id, a))
            .collect();
    Ok(found
        .into_iter()
        .filter_map(|(id, sources)| accounts.remove(&id).map(|a| (a, sources)))
        .collect())
}

// ── GET /api/v1/suggestions ────────────────────────────────────────────────

pub async fn get_suggestions(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(params): Query<SuggestionsParams>,
) -> AppResult<Json<Vec<ApiAccount>>> {
    auth.require_scope("read:accounts")?;
    let (limit, _) = params.window();
    let accounts: Vec<Account> = suggested(&state, auth.account_id, limit, 0)
        .await?
        .into_iter()
        .map(|(a, _)| a)
        .collect();
    Ok(Json(batch_accounts_to_api(&state, &accounts).await))
}

// ── DELETE /api/v1/suggestions/:account_id ────────────────────────────────

pub async fn dismiss_suggestion(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(account_id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:accounts")?;
    sqlx::query!(
        r#"INSERT INTO follow_recommendation_mutes (account_id, target_account_id, created_at, updated_at)
           VALUES ($1, $2, now(), now()) ON CONFLICT DO NOTHING"#,
        auth.account_id, account_id,
    )
    .execute(&state.db)
    .await?;
    Ok(Json(serde_json::json!({})))
}

// ── GET /api/v2/suggestions ───────────────────────────────────────────────

pub async fn get_suggestions_v2(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    headers: axum::http::HeaderMap,
    Query(params): Query<SuggestionsParams>,
) -> AppResult<axum::response::Response> {
    use axum::response::IntoResponse as _;
    auth.require_scope("read:accounts")?;
    // `before_action :schedule_fasp_retrieval`.
    let refresh =
        crate::fasp::schedule_follow_recommendations(&state, auth.account_id, &headers).await;
    let (limit, offset) = params.window();
    let found = suggested(&state, auth.account_id, limit, offset).await?;
    let accounts: Vec<Account> = found.iter().map(|(a, _)| a.clone()).collect();
    let emojis_map = batch_account_emojis(&state, &accounts).await;
    let roles_map = batch_account_roles(&state, &accounts).await;
    let suggestions = found
        .into_iter()
        .map(|(a, sources)| {
            let mut api = account_from_db(&state.urls, &a);
            api.emojis = emojis_map.get(&a.id).cloned().unwrap_or_default();
            api.roles = roles_map.get(&a.id).cloned().unwrap_or_default();
            SuggestionV2 {
                source: crate::suggestions::legacy_source(&sources).map(str::to_owned),
                sources,
                account: api,
            }
        })
        .collect::<Vec<SuggestionV2>>();

    let mut response = Json(suggestions).into_response();
    if let Some(value) = refresh.and_then(|v| v.parse().ok()) {
        response
            .headers_mut()
            .insert(crate::async_refresh::HEADER, value);
    }
    Ok(response)
}
