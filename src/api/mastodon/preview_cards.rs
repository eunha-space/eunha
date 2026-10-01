//! `REST::PreviewCardSerializer`.

use std::collections::HashMap;

use super::types::{PreviewCard, PreviewCardAuthor};
use crate::db::models::Account;
use crate::error::AppResult;
use crate::state::AppState;

struct Row {
    id: i64,
    url: String,
    title: String,
    description: String,
    language: Option<String>,
    card_type: String,
    author_name: String,
    author_url: String,
    provider_name: String,
    provider_url: String,
    html: String,
    width: i32,
    height: i32,
    image_file_name: Option<String>,
    image_storage_schema_version: Option<i32>,
    image_description: String,
    embed_url: String,
    blurhash: Option<String>,
    published_at: Option<chrono::NaiveDateTime>,
    author_account_id: Option<i64>,
    unverified_author_account_id: Option<i64>,
}

/// The cards with these ids, as the API shows them to `viewer_id`.
pub async fn by_ids(
    state: &AppState,
    card_ids: &[i64],
    viewer_id: Option<i64>,
) -> AppResult<HashMap<i64, PreviewCard>> {
    if card_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query_as!(
        Row,
        r#"SELECT id, url, title, description, language,
                  CASE type WHEN 1 THEN 'photo' WHEN 2 THEN 'video' WHEN 3 THEN 'rich' ELSE 'link' END AS "card_type!",
                  author_name, author_url, provider_name, provider_url, html, width, height,
                  image_file_name, image_storage_schema_version, image_description, embed_url,
                  blurhash, published_at, author_account_id, unverified_author_account_id
           FROM preview_cards WHERE id = ANY($1)"#,
        card_ids,
    )
    .fetch_all(&state.db)
    .await?;

    let author_ids: Vec<i64> = rows.iter().filter_map(|r| r.author_account_id).collect();
    let authors: HashMap<i64, super::types::Account> = if author_ids.is_empty() {
        HashMap::new()
    } else {
        let accounts = sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = ANY($1)",
            &author_ids
        )
        .fetch_all(&state.db)
        .await?;
        let api = super::accounts::batch_accounts_to_api(state, &accounts).await;
        accounts.iter().map(|a| a.id).zip(api).collect()
    };

    Ok(rows
        .into_iter()
        .map(|r| {
            let image = r
                .image_file_name
                .as_deref()
                .filter(|f| !f.is_empty())
                .map(|f| {
                    state.storage.public_url(&crate::preview_card::image_path(
                        r.id,
                        f,
                        r.image_storage_schema_version,
                    ))
                });
            // `PreviewCard#authors`: one author, when the card names any.
            let authors = if !r.author_name.is_empty()
                || !r.author_url.is_empty()
                || r.author_account_id.is_some()
            {
                vec![PreviewCardAuthor {
                    name: r.author_name.clone(),
                    url: r.author_url.clone(),
                    account: r.author_account_id.and_then(|id| authors.get(&id).cloned()),
                }]
            } else {
                vec![]
            };
            let card = PreviewCard {
                url: r.url,
                title: r.title,
                description: r.description,
                language: r.language,
                card_type: r.card_type,
                author_name: r.author_name,
                author_url: r.author_url,
                provider_name: r.provider_name,
                provider_url: r.provider_url,
                html: crate::preview_card::sanitize_oembed(&r.html),
                width: r.width,
                height: r.height,
                image,
                image_description: r.image_description,
                embed_url: r.embed_url,
                blurhash: r.blurhash,
                published_at: r.published_at.map(super::convert::mastodon_date),
                authors,
                // Only for a signed-in viewer.
                missing_attribution: viewer_id.map(|viewer| {
                    r.unverified_author_account_id
                        .is_some_and(|unverified| unverified == viewer)
                }),
                history: None,
            };
            (r.id, card)
        })
        .collect())
}

/// `Status#preview_card` for each of these statuses: the card, with its
/// `url` the one the post linked to (`original_url.presence || url`).
pub async fn for_statuses(
    state: &AppState,
    status_ids: &[i64],
    viewer_id: Option<i64>,
) -> AppResult<HashMap<i64, PreviewCard>> {
    if status_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let links = sqlx::query!(
        r#"SELECT status_id, preview_card_id, url FROM preview_cards_statuses
           WHERE status_id = ANY($1) ORDER BY status_id, preview_card_id"#,
        status_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let mut first: HashMap<i64, (i64, Option<String>)> = HashMap::new();
    for link in links {
        first
            .entry(link.status_id)
            .or_insert((link.preview_card_id, link.url));
    }
    let card_ids: Vec<i64> = first.values().map(|(id, _)| *id).collect();
    let cards = by_ids(state, &card_ids, viewer_id).await?;
    Ok(first
        .into_iter()
        .filter_map(|(status_id, (card_id, original_url))| {
            let mut card = cards.get(&card_id)?.clone();
            if let Some(original) = original_url.filter(|u| !u.trim().is_empty()) {
                card.url = original;
            }
            Some((status_id, card))
        })
        .collect())
}
