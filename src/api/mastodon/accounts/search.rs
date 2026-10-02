//! `GET /api/v1/accounts/search`: `Api::V1::Accounts::SearchController`,
//! which hands the query to `AccountSearchService` (crate::search::accounts).

use super::*;
use crate::api::mastodon::extractors::FlexBool;

/// `DEFAULT_ACCOUNTS_LIMIT`.
const DEFAULT_ACCOUNTS_LIMIT: i64 = 40;

pub async fn search_accounts(
    state: AppState,
    Query(q): Query<AccountSearchQuery>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<ApiAccount>>> {
    auth.require_scope("read:accounts")?;
    // `limit_param(DEFAULT_ACCOUNTS_LIMIT)`.
    let limit = q.limit.as_deref().map_or(DEFAULT_ACCOUNTS_LIMIT, |l| {
        crate::search::ruby_to_i(l)
            .abs()
            .min(DEFAULT_ACCOUNTS_LIMIT * 2)
    });
    let offset = q
        .offset
        .as_deref()
        .map_or(0, crate::search::ruby_to_i)
        .max(0);
    let found = crate::search::accounts::search(
        &state,
        q.q.as_deref().unwrap_or(""),
        Some(auth.account_id),
        &crate::search::accounts::Options {
            limit,
            offset,
            resolve: q.resolve.is_some_and(|FlexBool(b)| b),
            following: q.following.is_some_and(|FlexBool(b)| b),
            use_searchable_text: false,
        },
    )
    .await?;
    Ok(Json(batch_accounts_to_api(&state, &found).await))
}
