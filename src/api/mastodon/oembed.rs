//! `GET /api/oembed`: the oEmbed document for a post.
//!
//! Mastodon: app/controllers/api/oembed_controller.rb
//! Mastodon: app/serializers/oembed_serializer.rb

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

    let base_url = format!("https://{}", instance.web_domain());
    let author_url = format!("{base_url}/@{}", row.username);
    let status_url = format!("{base_url}/@{}/{}", row.username, row.id);
    let display_name = if row.display_name.is_empty() {
        row.username.clone()
    } else {
        row.display_name.clone()
    };

    let width = params.maxwidth.unwrap_or(400).min(400);
    let handle = format!("@{}@{}", row.username, instance.local_domain);
    let html = embed_html(&base_url, &status_url, &handle);

    Ok(Json(OEmbedResponse {
        oembed_type: "rich".to_string(),
        version: "1.0".to_string(),
        author_name: display_name,
        author_url,
        // `OEmbedSerializer#provider_name`: the local domain.
        provider_name: instance.local_domain.to_owned(),
        provider_url: base_url,
        cache_age: 86400,
        html,
        width: Some(width),
        height: None,
    }))
}

/// `OEmbedSerializer#html`: a link to the post, which `embed.js` replaces with
/// the embed page where scripts run. It says Eunha where Mastodon says Mastodon.
fn embed_html(base_url: &str, status_url: &str, handle: &str) -> String {
    format!(
        concat!(
            r#"<blockquote class="mastodon-embed" data-embed-url="{status_url}/embed" "#,
            r#"style="background: #FCF8FF; border-radius: 8px; border: 1px solid #C9C4DA; margin: 0; "#,
            r#"max-width: 540px; min-width: 270px; overflow: hidden; padding: 0;">"#,
            r#"<a href="{status_url}" target="_blank" style="align-items: center; color: #1C1A25; "#,
            r#"display: flex; flex-direction: column; font-family: system-ui, -apple-system, "#,
            r#"BlinkMacSystemFont, 'Segoe UI', Oxygen, Ubuntu, Cantarell, 'Helvetica Neue', Roboto, "#,
            r#"sans-serif; font-size: 14px; justify-content: center; letter-spacing: 0.25px; "#,
            r#"line-height: 20px; padding: 24px; text-decoration: none;">"#,
            r#"<div style="color: #787588; margin-top: 16px;">Post by {handle}</div>"#,
            r#"<div style="font-weight: bold;">View on Eunha</div></a></blockquote> "#,
            r#"<script data-allowed-prefixes="{base_url}/" async src="{base_url}/embed.js"></script>"#,
        ),
        status_url = status_url,
        handle = handle,
        base_url = base_url,
    )
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
