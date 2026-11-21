//! `POST /api/eunha/v1/link_card`: the preview card a post would get, for
//! the composer to show while it is written. Mastodon fetches a card only
//! once a post is made, and has no such action.
//!
//! Mastodon: none

use axum::{routing::post, Extension, Json, Router};
use serde::Deserialize;

use crate::{
    api::{
        eunha::two_factor::signed_in,
        mastodon::{extractors::Params, types::PreviewCard},
    },
    error::AppResult,
    middleware::AuthenticatedUser,
    rate_limit::{self, LINK_CARDS},
    state::AppState,
};

pub fn routes() -> Router {
    Router::new().route("/api/eunha/v1/link_card", post(create))
}

#[derive(Debug, Deserialize)]
pub struct Body {
    /// The post's text as it stands, from which the link is picked as it
    /// is for a local post.
    text: String,
}

/// The card, or `null` when the text gets none.
pub async fn create(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Params(body): Params<Body>,
) -> AppResult<Json<Option<PreviewCard>>> {
    let (account_id, _) = signed_in(auth, "write:statuses")?;
    // Only a draft with a link counts, since only it may fetch a page.
    let Some(url) = crate::preview_card::draft_link(&state, &body.text) else {
        return Ok(Json(None));
    };
    rate_limit::record(&state, account_id, LINK_CARDS).await?;
    let Some(card_id) = crate::preview_card::preview(&state, &url).await else {
        return Ok(Json(None));
    };
    let mut cards =
        crate::api::mastodon::preview_cards::by_ids(&state, &[card_id], Some(account_id)).await?;
    Ok(Json(cards.remove(&card_id)))
}
