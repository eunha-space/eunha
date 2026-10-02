use axum::{
    extract::{Extension, Query},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::{
    error::{AppError, AppResult},
    middleware::ResolvedInstance,
    state::AppState,
};

#[derive(Debug, Deserialize)]
pub struct OEmbedParams {
    pub url: String,
    pub maxwidth: Option<u32>,
    pub maxheight: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct OEmbedResponse {
    #[serde(rename = "type")]
    pub oembed_type: String,
    pub version: String,
    pub author_name: String,
    pub author_url: String,
    pub provider_name: String,
    pub provider_url: String,
    pub cache_age: u64,
    pub html: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

// ── GET /api/oembed ────────────────────────────────────────────────────────
// Parses a status URL of the form https://domain/@username/status_id and
// returns an oEmbed representation of that status.

pub async fn get_oembed(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Query(params): Query<OEmbedParams>,
) -> AppResult<Json<OEmbedResponse>> {
    let status_id = parse_status_id_from_url(&params.url).ok_or(AppError::NotFound)?;

    let row = sqlx::query!(
        r#"SELECT s.id, s.account_id,
                  a.username, a.display_name
           FROM statuses s
           JOIN accounts a ON a.id = s.account_id
           WHERE s.id = $1
             AND s.deleted_at IS NULL
             AND s.visibility IN (0, 1)"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let base_url = format!("https://{}", instance.domain);
    let author_url = format!("{base_url}/@{}", row.username);
    let status_url = format!("{base_url}/@{}/{}", row.username, row.id);
    let display_name = if row.display_name.is_empty() {
        row.username.clone()
    } else {
        row.display_name.clone()
    };

    let width = params.maxwidth.unwrap_or(400).min(400);
    let html = format!(
        r#"<iframe src="{status_url}/embed" class="mastodon-embed" style="max-width: 100%; border: 0" width="{width}" allowfullscreen="allowfullscreen"></iframe><script src="{base_url}/embed.js" async="async"></script>"#,
    );

    Ok(Json(OEmbedResponse {
        oembed_type: "rich".to_string(),
        version: "1.0".to_string(),
        author_name: display_name,
        author_url,
        // `OEmbedSerializer#provider_name`: the local domain.
        provider_name: instance.domain.clone(),
        provider_url: base_url,
        cache_age: 86400,
        html,
        width: Some(width),
        height: None,
    }))
}

/// Parse a status ID from a URL of the form `https://domain/@username/123456`.
fn parse_status_id_from_url(url: &str) -> Option<i64> {
    // Strip query string
    let url = url.split('?').next()?;
    // Find the /@username/ segment
    let at_pos = url.find("/@")?;
    let after_at = &url[at_pos + 2..];
    // The ID is after the username
    let slash_pos = after_at.find('/')?;
    let id_str = &after_at[slash_pos + 1..];
    // Strip any trailing path segments
    let id_str = id_str.split('/').next()?;
    id_str.parse::<i64>().ok()
}
