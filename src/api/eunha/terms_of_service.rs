//! The terms of service interstitial, for eunha's own web client.
//!
//! When an administrator notifies users of new terms, Mastodon mails the
//! users active in the past year and flags the rest
//! (`users.require_tos_interstitial`); its web app then shows a flagged user
//! the terms in place of any page (`WebAppControllerConcern#
//! redirect_to_tos_interstitial!`) until they open `/terms-of-service`, which
//! clears the flag. Eunha's web app is a client of the API, so it asks here
//! instead; see the `terms-of-service-interstitial-api` divergence.

use axum::{Extension, Json};
use serde::Serialize;

use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
    terms_of_service,
};

#[derive(Debug, Serialize)]
pub struct Interstitial {
    /// The terms to show, as `REST::TermsOfServiceSerializer` has them, or
    /// `null` when there is nothing to show.
    pub terms_of_service: Option<terms_of_service::Rest>,
}

fn user_id(auth: Option<Extension<AuthenticatedUser>>) -> AppResult<i64> {
    auth.and_then(|Extension(a)| a.user_id)
        .ok_or(AppError::Unauthorized)
}

/// GET /api/eunha/v1/terms_of_service/interstitial
///
/// For a flagged user, `TermsOfService.published.first`; a user flagged for
/// terms that have since been removed is unflagged, as Mastodon does.
pub async fn show(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Interstitial>> {
    let user_id = user_id(auth)?;
    let flagged = sqlx::query_scalar!(
        "SELECT require_tos_interstitial FROM users WHERE id = $1",
        user_id
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    if !flagged {
        return Ok(Json(Interstitial {
            terms_of_service: None,
        }));
    }
    let Some(tos) = terms_of_service::published_first(&state).await? else {
        clear_flag(&state, user_id).await?;
        return Ok(Json(Interstitial {
            terms_of_service: None,
        }));
    };
    Ok(Json(Interstitial {
        terms_of_service: Some(terms_of_service::serialize(&state, &tos).await?),
    }))
}

/// DELETE /api/eunha/v1/terms_of_service/interstitial
///
/// `TermsOfServiceController#clear_redirect_interstitial!`: the user has
/// opened the terms.
pub async fn dismiss(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<serde_json::Value>> {
    let user_id = user_id(auth)?;
    clear_flag(&state, user_id).await?;
    Ok(Json(serde_json::json!({})))
}

async fn clear_flag(state: &AppState, user_id: i64) -> AppResult<()> {
    sqlx::query!(
        "UPDATE users SET require_tos_interstitial = false WHERE id = $1",
        user_id
    )
    .execute(&state.db)
    .await?;
    Ok(())
}
