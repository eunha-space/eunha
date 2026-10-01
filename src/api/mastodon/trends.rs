use axum::{
    extract::{Extension, Query},
    Json,
};
use serde::Deserialize;

use super::{
    accounts::{batch_account_emojis, batch_account_roles},
    convert::status_from_db,
    status_serialize::{
        batch_reblog_data, batch_status_cards, batch_status_emojis, batch_status_media,
        batch_status_mentions, batch_status_polls, batch_statuses_tags, hydrate_status_stats,
    },
    types::{Status, Tag},
};
use crate::{
    error::AppResult,
    middleware::{AuthenticatedUser, ResolvedInstance},
    state::AppState,
};

#[derive(Debug, Deserialize)]
pub struct TrendParams {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

// ── GET /api/v1/trends/tags  &  GET /api/v1/trends ────────────────────────

pub async fn trending_tags(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Query(params): Query<TrendParams>,
    auth: Option<Extension<AuthenticatedUser>>,
    req_headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> AppResult<(axum::http::HeaderMap, Json<Vec<Tag>>)> {
    let limit = params.limit.unwrap_or(10).clamp(1, 20);
    let offset = params.offset.unwrap_or(0).max(0);
    let viewer_id = auth.map(|Extension(a)| a.account_id);
    // `enabled?`: the `trends` setting.
    if !crate::settings::boolean(&state, "trends").await {
        return Ok((axum::http::HeaderMap::new(), Json(vec![])));
    }
    let languages = crate::trends::preferred_languages(&state, viewer_id, &req_headers).await;
    let tags: Vec<Tag> = tags_query(
        &state,
        &instance.domain,
        limit,
        offset,
        viewer_id,
        false,
        &languages,
    )
    .await?
    .into_iter()
    .map(|(tag, _)| tag)
    .collect();
    let headers = super::offset_link_headers(&req_headers, &uri, offset, limit, tags.len());
    Ok((headers, Json(tags)))
}

/// What a moderator reviewing a trending tag sees besides the tag:
/// `REST::Admin::TagSerializer`'s extra fields.
pub struct TagReview {
    pub trendable: bool,
    pub usable: bool,
    pub listable: bool,
    pub requires_review: bool,
}

/// `Trends.tags.query`: the tags in `tag_trends`, `allowed` ones only unless
/// `staff`, who see those still awaiting review too; those in `languages`
/// first, then by score.
pub(crate) async fn tags_query(
    state: &AppState,
    domain: &str,
    limit: i64,
    offset: i64,
    viewer_id: Option<i64>,
    staff: bool,
    languages: &[String],
) -> AppResult<Vec<(Tag, TagReview)>> {
    let trendable_by_default = crate::settings::boolean(state, "trendable_by_default").await;

    let rows = sqlx::query!(
        r#"SELECT t.id, t.name,
                  COALESCE(t.trendable, $3) AS "trendable!", COALESCE(t.usable, true) AS "usable!",
                  COALESCE(t.listable, true) AS "listable!", (t.reviewed_at IS NULL) AS "requires_review!"
           FROM tags t
           JOIN tag_trends tt ON tt.tag_id = t.id
           WHERE $4 OR tt.allowed
           ORDER BY CASE WHEN tt.language = ANY($5) THEN 1 ELSE 0 END DESC, tt.score DESC
           LIMIT $1 OFFSET $2"#,
        limit,
        offset,
        trendable_by_default,
        staff,
        languages,
    )
    .fetch_all(&state.db)
    .await?;

    let tag_ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let histories = super::tags::fetch_tags_histories(state, &tag_ids).await;

    let (following_set, featuring_set) =
        if let Some(vid) = viewer_id {
            let followed: std::collections::HashSet<i64> = sqlx::query_scalar!(
            "SELECT tag_id FROM tag_follows WHERE account_id = $1 AND tag_id = ANY($2::bigint[])",
            vid, &tag_ids,
        )
            .fetch_all(&state.db)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();

            let featured: std::collections::HashSet<i64> = sqlx::query_scalar!(
            "SELECT tag_id FROM featured_tags WHERE account_id = $1 AND tag_id = ANY($2::bigint[])",
            vid, &tag_ids,
        )
            .fetch_all(&state.db)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect();

            (Some(followed), Some(featured))
        } else {
            (None, None)
        };

    Ok(rows
        .into_iter()
        .map(|r| {
            let name_lower = r.name.to_lowercase();
            let following = following_set.as_ref().map(|s| s.contains(&r.id));
            let featuring = featuring_set.as_ref().map(|s| s.contains(&r.id));
            let review = TagReview {
                trendable: r.trendable,
                usable: r.usable,
                listable: r.listable,
                requires_review: r.requires_review,
            };
            let tag = Tag {
                id: r.id.to_string(),
                history: histories.get(&r.id).cloned().unwrap_or_default(),
                name: r.name,
                url: format!(
                    "https://{}/tags/{}",
                    domain,
                    urlencoding::encode(&name_lower)
                ),
                following,
                featuring,
            };
            (tag, review)
        })
        .collect())
}

// ── GET /api/v1/trends/statuses ───────────────────────────────────────────

pub async fn trending_statuses(
    state: AppState,
    Query(params): Query<TrendParams>,
    auth: Option<Extension<crate::middleware::AuthenticatedUser>>,
    req_headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> AppResult<(axum::http::HeaderMap, Json<Vec<Status>>)> {
    let limit = params.limit.unwrap_or(20).clamp(1, 40);
    let offset = params.offset.unwrap_or(0).max(0);
    let viewer_id = auth.map(|Extension(a)| a.account_id);
    if !crate::settings::boolean(&state, "trends").await {
        return Ok((axum::http::HeaderMap::new(), Json(vec![])));
    }
    let languages = crate::trends::preferred_languages(&state, viewer_id, &req_headers).await;
    let result: Vec<Status> = statuses_query(&state, limit, offset, viewer_id, false, &languages)
        .await?
        .into_iter()
        .map(|(status, _)| status)
        .collect();
    let headers = super::offset_link_headers(&req_headers, &uri, offset, limit, result.len());
    Ok((headers, Json(result)))
}

/// `Trends.statuses.query`: the posts in `status_trends`, those in
/// `languages` first, then by score. Unless `staff`, only allowed ones, each
/// account's best only, and none the viewer has blocked, muted, or blocked
/// the domain of (`filtered_for`); with each, whether it still awaits review
/// (`Status#requires_review?`).
pub(crate) async fn statuses_query(
    state: &AppState,
    limit: i64,
    offset: i64,
    viewer_id: Option<i64>,
    staff: bool,
    languages: &[String],
) -> AppResult<Vec<(Status, bool)>> {
    let state = state.clone();
    let viewer_id = viewer_id.filter(|_| !staff);

    let rows = sqlx::query_as!(
        crate::db::models::Status,
        r#"SELECT s.* FROM statuses s
           JOIN status_trends st ON st.status_id = s.id
           JOIN accounts a ON a.id = s.account_id
           WHERE s.deleted_at IS NULL
             -- `StatusTrend.allowed`: allowed, and its account's best.
             AND ($4 OR (st.allowed AND st.score = (
                   SELECT MAX(m.score) FROM status_trends m WHERE m.account_id = st.account_id)))
             -- `not_excluded_by_account`
             AND ($3::bigint IS NULL OR NOT EXISTS (
                 SELECT 1 FROM blocks b
                 WHERE (b.account_id = $3 AND b.target_account_id = s.account_id)
                    OR (b.account_id = s.account_id AND b.target_account_id = $3)
             ))
             AND ($3::bigint IS NULL OR NOT EXISTS (
                 SELECT 1 FROM mutes mu
                 WHERE mu.account_id = $3 AND mu.target_account_id = s.account_id
                   AND (mu.expires_at IS NULL OR mu.expires_at > now())
             ))
             -- `not_domain_blocked_by_account`
             AND ($3::bigint IS NULL OR a.domain IS NULL OR NOT EXISTS (
                 SELECT 1 FROM account_domain_blocks adb
                 WHERE adb.account_id = $3 AND adb.domain = a.domain
             ))
           ORDER BY CASE WHEN st.language = ANY($5) THEN 1 ELSE 0 END DESC, st.score DESC
           LIMIT $1 OFFSET $2"#,
        limit,
        offset,
        viewer_id,
        staff,
        languages,
    )
    .fetch_all(&state.db)
    .await?;

    if rows.is_empty() {
        return Ok(vec![]);
    }
    // `requires_review?`: unset on the post, and its account never reviewed.
    let pending: std::collections::HashSet<i64> = sqlx::query_scalar!(
        r#"SELECT s.id FROM statuses s JOIN accounts a ON a.id = s.account_id
           WHERE s.id = ANY($1) AND s.trendable IS NULL AND a.reviewed_at IS NULL"#,
        &rows.iter().map(|s| s.id).collect::<Vec<_>>(),
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    let all_ids: Vec<i64> = rows.iter().map(|s| s.id).collect();
    let media_map = batch_status_media(&state, &all_ids).await?;
    let reblog_map = batch_reblog_data(&state, &rows).await?;
    let reblog_ids: Vec<i64> = reblog_map.values().map(|(rs, _, _)| rs.id).collect();
    let mut enrich_ids = all_ids.clone();
    enrich_ids.extend_from_slice(&reblog_ids);
    let tags_map = batch_statuses_tags(&state, &enrich_ids).await?;
    let mentions_map = batch_status_mentions(&state, &enrich_ids).await?;
    let all_statuses_for_emoji: Vec<crate::db::models::Status> = rows
        .iter()
        .cloned()
        .chain(reblog_map.values().map(|(rs, _, _)| rs.clone()))
        .collect();
    let emojis_map = batch_status_emojis(&state, &all_statuses_for_emoji).await?;
    let polls_map = batch_status_polls(&state, &enrich_ids, viewer_id).await?;
    let cards_map = batch_status_cards(&state, &enrich_ids).await?;
    let ctxs = if let Some(vid) = viewer_id {
        super::statuses::batch_viewer_contexts(&state, vid, &all_ids).await?
    } else {
        std::collections::HashMap::new()
    };

    let account_ids: Vec<i64> = rows
        .iter()
        .map(|s| s.account_id)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let accounts: Vec<crate::db::models::Account> = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
        &account_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let account_map: std::collections::HashMap<i64, crate::db::models::Account> =
        accounts.iter().cloned().map(|a| (a.id, a)).collect();

    // Batch-fetch profile emojis for all accounts
    let all_accounts_for_emoji: Vec<crate::db::models::Account> = {
        let mut seen = std::collections::HashSet::new();
        account_map
            .values()
            .chain(reblog_map.values().map(|(_, ra, _)| ra))
            .filter(|a| seen.insert(a.id))
            .cloned()
            .collect()
    };
    let account_emojis_map = batch_account_emojis(&state, &all_accounts_for_emoji).await;
    let account_roles_map = batch_account_roles(&state, &all_accounts_for_emoji).await;

    let mut result = Vec::with_capacity(rows.len());
    for s in &rows {
        let Some(account) = account_map.get(&s.account_id) else {
            continue;
        };
        let media = media_map.get(&s.id).cloned().unwrap_or_default();
        let reblog = reblog_map.get(&s.id).cloned();
        let mentions = mentions_map.get(&s.id).cloned().unwrap_or_default();
        let rb_mentions = reblog
            .as_ref()
            .and_then(|(rs, _, _)| mentions_map.get(&rs.id))
            .cloned()
            .unwrap_or_default();
        let ctx = ctxs.get(&s.id).cloned();
        let mut api = status_from_db(
            &state.urls,
            s,
            account,
            media,
            reblog,
            ctx,
            &mentions,
            &rb_mentions,
        );
        api.account.emojis = account_emojis_map
            .get(&account.id)
            .cloned()
            .unwrap_or_default();
        api.account.roles = account_roles_map
            .get(&account.id)
            .cloned()
            .unwrap_or_default();
        api.tags = tags_map.get(&s.id).cloned().unwrap_or_default();
        api.mentions = mentions;
        api.emojis = emojis_map.get(&s.id).cloned().unwrap_or_default();
        api.poll = polls_map.get(&s.id).cloned();
        api.card = cards_map.get(&s.id).cloned();
        if let Some(ref mut rb) = api.reblog {
            let rid: i64 = rb.id.parse().unwrap_or(0);
            let rb_id: i64 = rb.account.id.parse().unwrap_or(0);
            rb.account.emojis = account_emojis_map.get(&rb_id).cloned().unwrap_or_default();
            rb.account.roles = account_roles_map.get(&rb_id).cloned().unwrap_or_default();
            rb.tags = tags_map.get(&rid).cloned().unwrap_or_default();
            rb.mentions = rb_mentions;
            rb.emojis = emojis_map.get(&rid).cloned().unwrap_or_default();
            rb.poll = polls_map.get(&rid).cloned();
            rb.card = cards_map.get(&rid).cloned();
        }
        result.push(api);
    }
    hydrate_status_stats(&state, result.iter_mut()).await;

    Ok(result
        .into_iter()
        .map(|status| {
            let pending = status
                .id
                .parse::<i64>()
                .is_ok_and(|id| pending.contains(&id));
            (status, pending)
        })
        .collect())
}

// ── GET /api/v1/trends/links ──────────────────────────────────────────────

pub async fn trending_links(
    state: AppState,
    Query(params): Query<TrendParams>,
    auth: Option<Extension<AuthenticatedUser>>,
    req_headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
) -> AppResult<(axum::http::HeaderMap, Json<Vec<super::types::PreviewCard>>)> {
    let limit = params.limit.unwrap_or(10).clamp(1, 40);
    let offset = params.offset.unwrap_or(0).max(0);
    if !crate::settings::boolean(&state, "trends").await {
        return Ok((axum::http::HeaderMap::new(), Json(vec![])));
    }
    let viewer_id = auth.map(|Extension(a)| a.account_id);
    let languages = crate::trends::preferred_languages(&state, viewer_id, &req_headers).await;
    let cards: Vec<super::types::PreviewCard> =
        links_query(&state, limit, offset, false, &languages)
            .await?
            .into_iter()
            .map(|(card, _, _)| card)
            .collect();
    let headers = super::offset_link_headers(&req_headers, &uri, offset, limit, cards.len());
    Ok((headers, Json(cards)))
}

/// `Trends.links.query`: the preview cards in `preview_card_trends`,
/// allowed ones only unless `staff`, those in `languages` first, then by
/// score; with each, its id and whether it awaits review.
pub(crate) async fn links_query(
    state: &AppState,
    limit: i64,
    offset: i64,
    staff: bool,
    languages: &[String],
) -> AppResult<Vec<(super::types::PreviewCard, i64, bool)>> {
    let rows = sqlx::query!(
        r#"SELECT pc.id, pc.url, pc.title, pc.description, pc.language,
                  CASE pc.type WHEN 1 THEN 'photo' WHEN 2 THEN 'video' WHEN 3 THEN 'rich' ELSE 'link' END as "card_type!",
                  pc.author_name, pc.author_url, pc.provider_name, pc.provider_url,
                  pc.html, pc.width, pc.height, NULL::text as image_url, pc.embed_url, pc.blurhash,
                  -- `PreviewCard#requires_review?`
                  (pc.trendable IS NULL AND NOT EXISTS (
                     SELECT 1 FROM preview_card_providers p
                     WHERE (lower(substring(pc.url from '^[a-zA-Z]+://([^/:?#]+)')) = p.domain
                            OR lower(substring(pc.url from '^[a-zA-Z]+://([^/:?#]+)')) LIKE '%.' || p.domain)
                       AND p.reviewed_at IS NOT NULL
                       AND char_length(p.domain) = (
                         SELECT max(char_length(p2.domain)) FROM preview_card_providers p2
                         WHERE lower(substring(pc.url from '^[a-zA-Z]+://([^/:?#]+)')) = p2.domain
                            OR lower(substring(pc.url from '^[a-zA-Z]+://([^/:?#]+)')) LIKE '%.' || p2.domain)
                  )) AS "requires_review!"
           FROM preview_cards pc
           JOIN preview_card_trends t ON t.preview_card_id = pc.id
           WHERE $3 OR t.allowed
           ORDER BY CASE WHEN t.language = ANY($4) THEN 1 ELSE 0 END DESC, t.score DESC
           LIMIT $1 OFFSET $2"#,
        limit, offset, staff, languages,
    )
    .fetch_all(&state.db)
    .await?;

    let mut cards = Vec::with_capacity(rows.len());
    for r in rows {
        // `REST::Trends::LinkSerializer#history`.
        let history = crate::moderation::history::days(state, "links", r.id)
            .await
            .into_iter()
            .map(|d| super::types::TagHistory {
                day: d.day.to_string(),
                uses: d.uses.to_string(),
                accounts: d.accounts.to_string(),
            })
            .collect();
        cards.push((
            super::types::PreviewCard {
                url: r.url,
                title: r.title,
                description: r.description,
                card_type: r.card_type,
                author_name: r.author_name,
                author_url: r.author_url,
                provider_name: r.provider_name,
                provider_url: r.provider_url,
                html: r.html,
                width: r.width,
                height: r.height,
                image: r.image_url,
                embed_url: r.embed_url,
                blurhash: r.blurhash,
                language: r.language,
                published_at: None,
                authors: vec![],
                image_description: String::new(),
                missing_attribution: None,
                history: Some(history),
            },
            r.id,
            r.requires_review,
        ));
    }
    Ok(cards)
}
