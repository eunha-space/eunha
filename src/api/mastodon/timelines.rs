use axum::{
    extract::{Extension, Path, Query},
    http::{header, HeaderMap, Uri},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use super::{
    accounts::{batch_account_emojis, batch_account_roles},
    convert::status_from_db,
    status_serialize::{
        batch_quote_data, batch_reblog_data, batch_status_cards, batch_status_emojis,
        batch_status_media, batch_status_mentions, batch_status_polls, batch_statuses_tags,
        hydrate_status_stats,
    },
    types::{PaginationParams, Status},
};
use crate::{
    db::models::{Account, Status as DbStatus},
    error::{AppError, AppResult},
    feed,
    middleware::AuthenticatedUser,
    state::AppState,
};

#[derive(Debug, Deserialize)]
pub struct PublicTimelineQuery {
    #[serde(flatten)]
    pub pagination: PaginationParams,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    pub local: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    pub remote: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    pub only_media: Option<bool>,
}

/// Which feed's access settings apply: the live feeds (`PublicFeed`) or the
/// hashtag and link feeds (`TagFeed`, `LinkFeed`).
#[derive(Clone, Copy)]
pub(crate) enum FeedKind {
    Live,
    Topic,
}

/// What a public, hashtag or link feed may show the viewer.
pub(crate) struct FeedReach {
    /// `incompatible_feed_settings?`: nothing at all.
    pub nothing: bool,
    pub local_only: bool,
    pub remote_only: bool,
}

/// The `require_user!, if: :require_auth?` of the timeline controllers, then
/// `PublicFeed`'s reading of the `*_feed_access` settings: `public` for
/// anyone, `authenticated` for functional users, `disabled` for those whose
/// role may `view_feeds`.
pub(crate) async fn feed_reach(
    state: &AppState,
    auth: Option<&AuthenticatedUser>,
    kind: FeedKind,
    local: bool,
    remote: bool,
) -> AppResult<FeedReach> {
    let (local_var, remote_var) = match kind {
        FeedKind::Live => ("local_live_feed_access", "remote_live_feed_access"),
        FeedKind::Topic => ("local_topic_feed_access", "remote_topic_feed_access"),
    };
    let local_setting = crate::settings::string(state, local_var).await;
    let remote_setting = crate::settings::string(state, remote_var).await;
    let require_auth = if local {
        local_setting != "public"
    } else if remote {
        remote_setting != "public"
    } else {
        local_setting != "public" || remote_setting != "public"
    };
    if require_auth {
        crate::middleware::require_user(auth)?;
    }
    let local_access = feed_access(state, auth, &local_setting).await;
    let remote_access = feed_access(state, auth, &remote_setting).await;
    let local_only = (local && !remote) || !remote_access;
    let remote_only = (remote && !local) || !local_access;
    Ok(FeedReach {
        nothing: (local_only && !local_access) || (remote_only && !remote_access),
        local_only,
        remote_only,
    })
}

/// `PublicFeed#user_has_access_to_feed?`.
async fn feed_access(state: &AppState, auth: Option<&AuthenticatedUser>, setting: &str) -> bool {
    let user = auth.filter(|a| a.user_id.is_some());
    match setting {
        "public" => true,
        "authenticated" => {
            user.is_some_and(|a| a.standing == crate::middleware::Standing::Functional)
        }
        "disabled" => match user {
            Some(a) => crate::moderation::role::acting(&state.db, a.account_id)
                .await
                .is_ok_and(|role| role.can(&[crate::moderation::role::flag::VIEW_FEEDS])),
            None => false,
        },
        _ => false,
    }
}

/// `to_a_paginated_by_id`: a `min_id` page is fetched oldest first, so it
/// starts just past `min_id`, and handed over newest first like any other.
pub(crate) fn newest_first<T>(min_id: Option<i64>, mut page: Vec<T>) -> Vec<T> {
    if min_id.is_some() {
        page.reverse();
    }
    page
}

// ── GET /api/v1/timelines/public ──────────────────────────────────────────

pub async fn public_timeline(
    state: AppState,
    uri: Uri,
    req_headers: HeaderMap,
    Query(q): Query<PublicTimelineQuery>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<impl IntoResponse> {
    let limit = q.pagination.limit_clamped(20, 40);
    let max_id = q
        .pagination
        .max_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let since_id = q
        .pagination
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let min_id = q
        .pagination
        .min_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let reach = feed_reach(
        &state,
        auth.as_ref().map(|Extension(a)| a),
        FeedKind::Live,
        q.local.unwrap_or(false),
        q.remote.unwrap_or(false),
    )
    .await?;
    if reach.nothing {
        return Ok(with_pagination_link(&req_headers, &uri, vec![]));
    }
    let (local_only, remote_only) = (reach.local_only, reach.remote_only);
    let only_media = q.only_media.unwrap_or(false);
    let viewer_id: Option<i64> = auth.as_ref().map(|Extension(a)| a.account_id);

    // min_id: return oldest items just after min_id (ASC); else DESC
    let statuses = if min_id.is_some() {
        sqlx::query_as!(
            DbStatus,
            r#"SELECT s.*
               FROM statuses s
               JOIN accounts a ON a.id = s.account_id
               WHERE s.visibility = 0
                 AND s.deleted_at IS NULL
                 AND s.reblog_of_id IS NULL
                 AND (NOT s.reply OR s.in_reply_to_account_id = s.account_id)
                 AND (NOT $1::bool OR a.domain IS NULL)
                 AND (NOT $4::bool OR a.domain IS NOT NULL)
                 AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                 AND a.silenced_at IS NULL
                 AND ($6::bigint IS NULL OR a.domain IS NULL OR NOT EXISTS (
                     SELECT 1 FROM account_domain_blocks udb WHERE udb.account_id = $6 AND udb.domain = a.domain
                 ))
                 -- `language_scope`: the viewer's chosen languages, if any.
                 AND ($6::bigint IS NULL OR NOT EXISTS (
                     SELECT 1 FROM users vu WHERE vu.account_id = $6
                       AND cardinality(vu.chosen_languages) > 0
                       AND (s.language IS NULL OR NOT (s.language = ANY(vu.chosen_languages)))
                 ))
                 AND ($2::bigint IS NULL OR s.id > $2)
                 AND (NOT $5::bool OR EXISTS (SELECT 1 FROM media_attachments WHERE status_id = s.id))
                 AND (s.text != ''
                      OR s.poll_id IS NOT NULL
                      OR EXISTS (SELECT 1 FROM media_attachments WHERE status_id = s.id))
                 AND ($6::bigint IS NULL OR NOT EXISTS (
                     SELECT 1 FROM blocks b
                     WHERE (b.account_id = $6 AND b.target_account_id = s.account_id)
                        OR (b.account_id = s.account_id AND b.target_account_id = $6)
                 ))
                 AND ($6::bigint IS NULL OR NOT EXISTS (
                     SELECT 1 FROM mutes mu
                     WHERE mu.account_id = $6 AND mu.target_account_id = s.account_id
                 ))
               ORDER BY s.id ASC
               LIMIT $3"#,
            local_only,
            min_id,
            limit,
            remote_only,
            only_media,
            viewer_id,
        )
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as!(
            DbStatus,
            r#"SELECT s.*
               FROM statuses s
               JOIN accounts a ON a.id = s.account_id
               WHERE s.visibility = 0
                 AND s.deleted_at IS NULL
                 AND s.reblog_of_id IS NULL
                 AND (NOT s.reply OR s.in_reply_to_account_id = s.account_id)
                 AND (NOT $1::bool OR a.domain IS NULL)
                 AND (NOT $5::bool OR a.domain IS NOT NULL)
                 AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                 AND a.silenced_at IS NULL
                 AND ($7::bigint IS NULL OR a.domain IS NULL OR NOT EXISTS (
                     SELECT 1 FROM account_domain_blocks udb WHERE udb.account_id = $7 AND udb.domain = a.domain
                 ))
                 -- `language_scope`: the viewer's chosen languages, if any.
                 AND ($7::bigint IS NULL OR NOT EXISTS (
                     SELECT 1 FROM users vu WHERE vu.account_id = $7
                       AND cardinality(vu.chosen_languages) > 0
                       AND (s.language IS NULL OR NOT (s.language = ANY(vu.chosen_languages)))
                 ))
                 AND ($2::bigint IS NULL OR s.id < $2)
                 AND ($4::bigint IS NULL OR s.id > $4)
                 AND (NOT $6::bool OR EXISTS (SELECT 1 FROM media_attachments WHERE status_id = s.id))
                 AND (s.text != ''
                      OR s.poll_id IS NOT NULL
                      OR EXISTS (SELECT 1 FROM media_attachments WHERE status_id = s.id))
                 AND ($7::bigint IS NULL OR NOT EXISTS (
                     SELECT 1 FROM blocks b
                     WHERE (b.account_id = $7 AND b.target_account_id = s.account_id)
                        OR (b.account_id = s.account_id AND b.target_account_id = $7)
                 ))
                 AND ($7::bigint IS NULL OR NOT EXISTS (
                     SELECT 1 FROM mutes mu
                     WHERE mu.account_id = $7 AND mu.target_account_id = s.account_id
                 ))
               ORDER BY s.id DESC
               LIMIT $3"#,
            local_only,
            max_id,
            limit,
            since_id,
            remote_only,
            only_media,
            viewer_id,
        )
        .fetch_all(&state.db)
        .await?
    };

    let statuses = newest_first(min_id, statuses);
    let result = build_status_list_with_filters(&state, statuses, viewer_id).await?;
    let resp = with_pagination_link(&req_headers, &uri, result);
    Ok(resp)
}

// ── GET /api/v1/timelines/home ────────────────────────────────────────────

pub async fn home_timeline(
    state: AppState,
    uri: Uri,
    req_headers: HeaderMap,
    Query(q): Query<PaginationParams>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:statuses")?;
    let limit = q.limit_clamped(20, 40);
    let max_id = q.max_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let since_id = q.since_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let min_id = q.min_id.as_deref().and_then(|s| s.parse::<i64>().ok());

    // `HomeFeed#get`: what the feed holds, which reading never changes.
    let mut redis = state.redis.clone();
    let ids = feed::feed_get(
        &mut redis,
        &state.redis_keys,
        auth.account_id,
        max_id,
        since_id,
        min_id,
        limit as isize,
    )
    .await;
    let statuses = feed_statuses(&state, &ids).await?;

    let result = build_status_list_with_filters(&state, statuses, Some(auth.account_id)).await?;
    let mut resp = with_pagination_link(&req_headers, &uri, result).into_response();
    // `add_async_refresh_header(account_home_feed.async_refresh, retry_seconds:
    // 5)`, and `206` while `regenerating?`.
    let refresh = crate::home_feed::async_refresh(&state, auth.account_id).await;
    if let Some(value) = refresh.header_value(&state, crate::home_feed::RETRY_SECONDS) {
        if let Ok(value) = axum::http::HeaderValue::from_str(&value) {
            resp.headers_mut()
                .insert(crate::async_refresh::HEADER, value);
        }
    }
    if refresh.is_running() {
        *resp.status_mut() = axum::http::StatusCode::PARTIAL_CONTENT;
    }
    Ok(resp)
}

/// `Status.where(id: unhydrated)`, as `Feed#from_redis` hands a feed's ids
/// over, and as the home and list timelines then render them: what is kept
/// (`Status.kept`), newest first (`recent`), with no filter of its own — what
/// a feed may hold was decided as it was written.
async fn feed_statuses(state: &AppState, ids: &[i64]) -> AppResult<Vec<DbStatus>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }
    Ok(sqlx::query_as!(
        DbStatus,
        "SELECT * FROM statuses WHERE id = ANY($1) AND deleted_at IS NULL ORDER BY id DESC",
        ids,
    )
    .fetch_all(&state.db)
    .await?)
}

// ── GET /api/v1/timelines/list/:id ───────────────────────────────────────

pub async fn list_timeline(
    state: AppState,
    Path(list_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Query(q): Query<PaginationParams>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:statuses")?;
    sqlx::query_scalar!(
        "SELECT id FROM lists WHERE id = $1 AND account_id = $2",
        list_id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    let limit = q.limit_clamped(20, 40);
    let max_id = q.max_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let since_id = q.since_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let min_id = q.min_id.as_deref().and_then(|s| s.parse::<i64>().ok());

    // `ListFeed#get`: what the feed holds, which reading never changes.
    let mut redis = state.redis.clone();
    let ids = feed::list_feed_get(
        &mut redis,
        &state.redis_keys,
        list_id,
        max_id,
        since_id,
        min_id,
        limit as isize,
    )
    .await;
    let statuses = feed_statuses(&state, &ids).await?;

    let result = build_status_list_with_filters(&state, statuses, Some(auth.account_id)).await?;
    let resp = with_pagination_link(&req_headers, &uri, result);
    Ok(resp)
}

// ── GET /api/v1/timelines/tag/:hashtag ───────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TagTimelineQuery {
    #[serde(flatten)]
    pub pagination: PaginationParams,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    pub local: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    pub remote: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    pub only_media: Option<bool>,
}

pub async fn tag_timeline(
    state: AppState,
    Path(hashtag): Path<String>,
    uri: Uri,
    req_headers: HeaderMap,
    Query(q): Query<TagTimelineQuery>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<impl IntoResponse> {
    // `api_v1_timelines_tag_url params[:id], …`: the links' path written
    // from the tag again, escaped as Rails escapes a path segment, rather
    // than as the request spelled it.
    let path = format!(
        "/api/v1/timelines/tag/{}",
        crate::formatter::text::escape_segment(&hashtag)
    );
    let uri = match uri.query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    }
    .parse::<Uri>()
    .unwrap_or(uri);
    let limit = q.pagination.limit_clamped(20, 40);
    let max_id = q
        .pagination
        .max_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let since_id = q
        .pagination
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let min_id = q
        .pagination
        .min_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let reach = feed_reach(
        &state,
        auth.as_ref().map(|Extension(a)| a),
        FeedKind::Topic,
        q.local.unwrap_or(false),
        q.remote.unwrap_or(false),
    )
    .await?;
    if reach.nothing {
        return Ok(with_pagination_link(&req_headers, &uri, vec![]));
    }
    let (local_only, remote_only) = (reach.local_only, reach.remote_only);
    let only_media = q.only_media.unwrap_or(false);
    let tag_name = hashtag.to_lowercase();

    let collect_tag_filter = |key_plain: &str, key_bracket: &str| -> Option<Vec<String>> {
        let v: Vec<String> = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
            .filter(|(k, _)| k == key_plain || k == key_bracket)
            .map(|(_, v)| v.to_lowercase())
            .collect();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    };
    let any_tags = collect_tag_filter("any", "any[]");
    let all_tags = collect_tag_filter("all", "all[]");
    let none_tags = collect_tag_filter("none", "none[]");

    // $1=tag_name $2=max_id $3=since_id $4=any $5=all $6=none $7=local_only
    // $8=only_media $9=viewer_id $10=limit $11=remote_only $12=min_id
    let base_conditions = r#"
               JOIN accounts a ON a.id = s.account_id
               WHERE lower(t.name) = $1
                 AND s.visibility = 0
                 AND s.deleted_at IS NULL
                 AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                 AND a.silenced_at IS NULL
                 AND (NOT $7::bool OR a.domain IS NULL)
                 AND (NOT $11::bool OR a.domain IS NOT NULL)
                 AND (NOT $8::bool OR EXISTS (
                     SELECT 1 FROM media_attachments WHERE status_id = s.id
                 ))
                 AND ($4::text[] IS NULL OR EXISTS (
                     SELECT 1 FROM statuses_tags st2
                     JOIN tags t2 ON t2.id = st2.tag_id
                     WHERE st2.status_id = s.id AND lower(t2.name) = ANY($4)
                 ))
                 AND ($5::text[] IS NULL OR (
                     SELECT COUNT(DISTINCT lower(t2.name))
                     FROM statuses_tags st2 JOIN tags t2 ON t2.id = st2.tag_id
                     WHERE st2.status_id = s.id AND lower(t2.name) = ANY($5)
                 ) = array_length($5, 1))
                 AND ($6::text[] IS NULL OR NOT EXISTS (
                     SELECT 1 FROM statuses_tags st2
                     JOIN tags t2 ON t2.id = st2.tag_id
                     WHERE st2.status_id = s.id AND lower(t2.name) = ANY($6)
                 ))
                 AND ($9::bigint IS NULL OR a.domain IS NULL OR NOT EXISTS (
                     SELECT 1 FROM account_domain_blocks udb
                     WHERE udb.account_id = $9 AND udb.domain = a.domain
                 ))
                 AND ($9::bigint IS NULL OR NOT EXISTS (
                     SELECT 1 FROM blocks b
                     WHERE (b.account_id = $9 AND b.target_account_id = s.account_id)
                        OR (b.account_id = s.account_id AND b.target_account_id = $9)
                 ))
                 AND ($9::bigint IS NULL OR NOT EXISTS (
                     SELECT 1 FROM mutes mu
                     WHERE mu.account_id = $9 AND mu.target_account_id = s.account_id
                 ))"#;

    let viewer_id: Option<i64> = auth.as_ref().map(|Extension(a)| a.account_id);

    // With `min_id`, the page just after it, oldest first, as
    // `paginate_by_min_id` takes it.
    let order = if min_id.is_some() { "ASC" } else { "DESC" };
    let sql = format!(
        r#"SELECT s.* FROM statuses s
           JOIN statuses_tags st ON st.status_id = s.id
           JOIN tags t ON t.id = st.tag_id
           {base_conditions}
             AND ($2::bigint IS NULL OR s.id < $2)
             AND ($3::bigint IS NULL OR s.id > $3)
             AND ($12::bigint IS NULL OR s.id > $12)
           ORDER BY s.id {order}
           LIMIT $10"#
    );
    let statuses: Vec<DbStatus> = sqlx::query_as(&sql)
        .bind(&tag_name)
        .bind(max_id)
        .bind(since_id)
        .bind(&any_tags)
        .bind(&all_tags)
        .bind(&none_tags)
        .bind(local_only)
        .bind(only_media)
        .bind(viewer_id)
        .bind(limit)
        .bind(remote_only)
        .bind(min_id)
        .fetch_all(&state.db)
        .await?;
    let statuses = newest_first(min_id, statuses);
    let result = build_status_list_with_filters(&state, statuses, viewer_id).await?;
    let resp = with_pagination_link(&req_headers, &uri, result);
    Ok(resp)
}

// ── Helpers ────────────────────────────────────────────────────────────────

/// Apply active custom filters for the viewer against a list of statuses.
/// `CustomFilter.apply_cached_filters` over `StatusRelationshipsPresenter`'s
/// statuses: for each post, the viewer's unexpired filters that match its
/// `proper` (keywords against `searchable_text`, or the post or what it
/// boosts named by the filter), as `FilterResult`s, keyed by the post's id and,
/// for a boost, by the boosted post's id too.
///
/// Every filter applies whatever its context, and none removes anything: the
/// client reads `filter.context` and `filter_action` and decides, as
/// Mastodon leaves it to.
pub(crate) async fn compute_filter_results(
    db: &sqlx::PgPool,
    viewer_id: i64,
    statuses: &[DbStatus],
) -> std::collections::HashMap<i64, serde_json::Value> {
    let mut result = std::collections::HashMap::new();
    let Ok(filters) = sqlx::query!(
        r#"SELECT cf.id, cf.phrase AS title, cf.context, cf.expires_at,
                  CASE cf.action WHEN 1 THEN 'hide' WHEN 2 THEN 'blur' ELSE 'warn' END AS "filter_action!"
           FROM custom_filters cf
           WHERE cf.account_id = $1 AND (cf.expires_at IS NULL OR cf.expires_at > now())"#,
        viewer_id,
    )
    .fetch_all(db)
    .await
    else {
        return result;
    };
    if filters.is_empty() {
        return result;
    }
    let filter_ids: Vec<i64> = filters.iter().map(|f| f.id).collect();

    // `CustomFilterKeyword#to_regex`, unioned per filter.
    let mut regexes: std::collections::HashMap<i64, regex::Regex> = Default::default();
    {
        let keywords = sqlx::query!(
            "SELECT custom_filter_id, keyword, whole_word FROM custom_filter_keywords WHERE custom_filter_id = ANY($1)",
            &filter_ids,
        )
        .fetch_all(db)
        .await
        .unwrap_or_default();
        let mut parts: std::collections::HashMap<i64, Vec<String>> = Default::default();
        for kw in keywords {
            let escaped = regex::escape(&kw.keyword);
            let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
            let expr = if kw.whole_word {
                format!(
                    "{}{}{}",
                    if word(kw.keyword.chars().next()) {
                        r"\b"
                    } else {
                        ""
                    },
                    escaped,
                    if word(kw.keyword.chars().last()) {
                        r"\b"
                    } else {
                        ""
                    },
                )
            } else {
                escaped
            };
            parts.entry(kw.custom_filter_id).or_default().push(expr);
        }
        for (id, exprs) in parts {
            if let Ok(re) = regex::Regex::new(&format!("(?i){}", exprs.join("|"))) {
                regexes.insert(id, re);
            }
        }
    }

    let proper_ids: Vec<i64> = statuses
        .iter()
        .map(|s| s.reblog_of_id.unwrap_or(s.id))
        .collect();
    let mut all_ids: Vec<i64> = statuses.iter().map(|s| s.id).collect();
    all_ids.extend(&proper_ids);
    let filtered_statuses: Vec<(i64, i64)> = sqlx::query!(
        "SELECT custom_filter_id, status_id FROM custom_filter_statuses WHERE custom_filter_id = ANY($1) AND status_id = ANY($2)",
        &filter_ids,
        &all_ids,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| (r.custom_filter_id, r.status_id))
    .collect();

    // `Status#searchable_text`: content warning, plain text, poll options and
    // media descriptions of the proper post.
    let texts: std::collections::HashMap<i64, String> = sqlx::query!(
        r#"SELECT s.id, s.spoiler_text, s.text, (s.local OR a.domain IS NULL) AS "local!",
                  COALESCE((SELECT string_agg(o, E'\n\n') FROM unnest(p.options) o), '') AS "poll!",
                  COALESCE((SELECT string_agg(m.description, E'\n\n'
                                              ORDER BY array_position(s.ordered_media_attachment_ids, m.id), m.id)
                            FROM media_attachments m
                            WHERE m.status_id = s.id AND m.description IS NOT NULL
                              AND (s.ordered_media_attachment_ids IS NULL
                                   OR m.id = ANY(s.ordered_media_attachment_ids))), '') AS "media!"
           FROM statuses s
           JOIN accounts a ON a.id = s.account_id
           LEFT JOIN polls p ON p.id = s.poll_id
           WHERE s.id = ANY($1)"#,
        &proper_ids,
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        let plain = if r.local {
            r.text.clone()
        } else {
            crate::api::mastodon::formatting::html_to_plain_text(&r.text)
        };
        let text = [r.spoiler_text, plain, r.poll, r.media]
            .into_iter()
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        (r.id, text)
    })
    .collect();

    for s in statuses {
        let proper = s.reblog_of_id.unwrap_or(s.id);
        let text = texts.get(&proper).map(String::as_str).unwrap_or("");
        let mut matches = vec![];
        for f in &filters {
            let keyword_matches = regexes
                .get(&f.id)
                .and_then(|re| re.find(text))
                .map(|m| vec![m.as_str().to_owned()]);
            let status_matches: Vec<String> = [Some(s.id), s.reblog_of_id]
                .into_iter()
                .flatten()
                .filter(|id| filtered_statuses.contains(&(f.id, *id)))
                .map(|id| id.to_string())
                .collect();
            if keyword_matches.is_none() && status_matches.is_empty() {
                continue;
            }
            matches.push(serde_json::json!({
                "filter": {
                    "id": f.id.to_string(),
                    "title": f.title,
                    "context": f.context,
                    "expires_at": f.expires_at.map(|t| t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()),
                    "filter_action": f.filter_action,
                },
                "keyword_matches": keyword_matches,
                "status_matches": if status_matches.is_empty() { serde_json::Value::Null } else { serde_json::json!(status_matches) },
            }));
        }
        if !matches.is_empty() {
            let value = serde_json::Value::Array(matches);
            if let Some(boosted) = s.reblog_of_id {
                result.insert(boosted, value.clone());
            }
            result.insert(s.id, value);
        }
    }
    result
}

/// The statuses as the API renders them for `viewer_id`, each with the
/// viewer's matching filters in `filtered`.
pub async fn build_status_list_with_filters(
    state: &AppState,
    statuses: Vec<DbStatus>,
    viewer_id: Option<i64>,
) -> AppResult<Vec<Status>> {
    let filter_results = if let Some(vid) = viewer_id {
        compute_filter_results(&state.db, vid, &statuses).await
    } else {
        std::collections::HashMap::new()
    };

    let mut result = build_status_list(state, statuses, viewer_id).await?;

    for s in &mut result {
        let id: i64 = s.id.parse().unwrap_or(0);
        if let Some(serde_json::Value::Array(arr)) = filter_results.get(&id) {
            s.filtered = Some(arr.clone());
        }
        if let Some(ref mut rb) = s.reblog {
            let rid: i64 = rb.id.parse().unwrap_or(0);
            if let Some(serde_json::Value::Array(arr)) = filter_results.get(&rid) {
                rb.filtered = Some(arr.clone());
            }
        }
    }

    Ok(result)
}

async fn build_status_list(
    state: &AppState,
    statuses: Vec<DbStatus>,
    viewer_id: Option<i64>,
) -> AppResult<Vec<Status>> {
    // For reblogs, check viewer context against the original status.
    let effective_ids: Vec<i64> = statuses
        .iter()
        .map(|s| s.reblog_of_id.unwrap_or(s.id))
        .collect();

    let ctxs = if let Some(vid) = viewer_id {
        super::statuses::batch_viewer_contexts(state, vid, &effective_ids).await?
    } else {
        std::collections::HashMap::new()
    };

    if statuses.is_empty() {
        return Ok(vec![]);
    }

    let account_ids: Vec<i64> = statuses
        .iter()
        .map(|s| s.account_id)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let accounts = sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE id = ANY($1::bigint[])",
        &account_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let account_map: std::collections::HashMap<i64, Account> =
        accounts.into_iter().map(|a| (a.id, a)).collect();

    let all_status_ids: Vec<i64> = statuses.iter().map(|s| s.id).collect();
    let media_map = batch_status_media(state, &all_status_ids).await?;
    let reblog_map = batch_reblog_data(state, &statuses).await?;
    let quote_map = batch_quote_data(state, &statuses, viewer_id).await?;
    let reblog_ids: Vec<i64> = reblog_map.values().map(|(rs, _, _)| rs.id).collect();
    let mut enrich_ids = all_status_ids.clone();
    enrich_ids.extend_from_slice(&reblog_ids);
    let tags_map = batch_statuses_tags(state, &enrich_ids).await?;
    let mentions_map = batch_status_mentions(state, &enrich_ids).await?;
    let applications_map =
        super::status_serialize::fetch_status_applications(state, &enrich_ids, viewer_id).await;
    let all_statuses_for_emoji: Vec<DbStatus> = statuses
        .iter()
        .cloned()
        .chain(reblog_map.values().map(|(rs, _, _)| rs.clone()))
        .collect();
    let emojis_map = batch_status_emojis(state, &all_statuses_for_emoji).await?;
    let polls_map = batch_status_polls(state, &enrich_ids, viewer_id).await?;
    let cards_map = batch_status_cards(state, &enrich_ids, viewer_id).await?;

    // Collect all unique accounts (main + reblog) for emoji and role batch-fetch
    let all_accounts_for_emoji: Vec<Account> = {
        let mut seen = std::collections::HashSet::new();
        account_map
            .values()
            .chain(reblog_map.values().map(|(_, ra, _)| ra))
            .filter(|a| seen.insert(a.id))
            .cloned()
            .collect()
    };
    let account_emojis_map = batch_account_emojis(state, &all_accounts_for_emoji).await;
    let account_roles_map = batch_account_roles(state, &all_accounts_for_emoji).await;

    let mut result = Vec::with_capacity(statuses.len());
    for s in &statuses {
        let account = account_map.get(&s.account_id).ok_or(AppError::NotFound)?;
        let media = media_map.get(&s.id).cloned().unwrap_or_default();
        let reblog = reblog_map.get(&s.id).cloned();
        let effective_id = s.reblog_of_id.unwrap_or(s.id);
        let ctx = ctxs.get(&effective_id).cloned();
        let mentions = mentions_map.get(&s.id).cloned().unwrap_or_default();
        let rb_mentions = reblog
            .as_ref()
            .and_then(|(rs, _, _)| mentions_map.get(&rs.id))
            .cloned()
            .unwrap_or_default();
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
        // A status keeps the attribution it was posted with when read back.
        api.application = applications_map.get(&s.id).cloned();
        api.account.emojis = account_emojis_map
            .get(&account.id)
            .cloned()
            .unwrap_or_default();
        api.account.set_roles(
            account_roles_map
                .get(&account.id)
                .cloned()
                .unwrap_or_default(),
        );
        api.tags = tags_map.get(&s.id).cloned().unwrap_or_default();
        api.mentions = mentions;
        api.emojis = emojis_map.get(&s.id).cloned().unwrap_or_default();
        api.poll = polls_map.get(&s.id).cloned();
        api.card = cards_map.get(&s.id).cloned();
        api.quote = quote_map.get(&s.id).cloned();
        if let Some(ref mut rb) = api.reblog {
            let rid: i64 = rb.id.parse().unwrap_or(0);
            let rb_id: i64 = rb.account.id.parse().unwrap_or(0);
            rb.account.emojis = account_emojis_map.get(&rb_id).cloned().unwrap_or_default();
            rb.account
                .set_roles(account_roles_map.get(&rb_id).cloned().unwrap_or_default());
            rb.tags = tags_map.get(&rid).cloned().unwrap_or_default();
            rb.mentions = rb_mentions;
            rb.emojis = emojis_map.get(&rid).cloned().unwrap_or_default();
            rb.poll = polls_map.get(&rid).cloned();
            rb.card = cards_map.get(&rid).cloned();
        }
        result.push(api);
    }
    hydrate_status_stats(state, result.iter_mut(), viewer_id).await;
    Ok(result)
}

fn with_pagination_link(
    req_headers: &HeaderMap,
    uri: &Uri,
    statuses: Vec<Status>,
) -> impl IntoResponse {
    let link = statuses
        .first()
        .zip(statuses.last())
        .map(|(newest, oldest)| {
            let extra = super::non_pagination_query(uri.query());
            super::link_header(req_headers, uri.path(), &extra, &newest.id, &oldest.id)
        });
    let mut headers = HeaderMap::new();
    if let Some(v) = link {
        if let Ok(val) = v.parse() {
            headers.insert(header::LINK, val);
        }
    }
    (headers, Json(statuses))
}

// ── GET /api/v1/timelines/link ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LinkTimelineQuery {
    pub url: Option<String>,
    #[serde(flatten)]
    pub pagination: PaginationParams,
}

/// `Api::V1::Timelines::LinkController` and `LinkFeed`: public posts by
/// discoverable accounts that carry a trending, allowed link.
pub async fn link_timeline(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    headers: HeaderMap,
    uri: Uri,
    Query(params): Query<LinkTimelineQuery>,
) -> AppResult<impl IntoResponse> {
    let reach = feed_reach(
        &state,
        auth.as_ref().map(|Extension(a)| a),
        FeedKind::Topic,
        false,
        false,
    )
    .await?;

    // `PreviewCard.joins(:trend).merge(PreviewCardTrend.allowed).find_by!(url:)`
    let card_id: i64 = sqlx::query_scalar!(
        r#"SELECT pc.id FROM preview_cards pc
           JOIN preview_card_trends t ON t.preview_card_id = pc.id
           WHERE pc.url = $1 AND t.allowed"#,
        params.url.unwrap_or_default(),
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    if reach.nothing {
        return Ok(with_pagination_link(&headers, &uri, vec![]));
    }

    let viewer_id = auth.map(|Extension(u)| u.account_id);
    // `limit_param(DEFAULT_STATUSES_LIMIT)`
    let limit = params.pagination.limit_clamped(20, 40);
    let max_id: Option<i64> = params
        .pagination
        .max_id
        .as_deref()
        .and_then(|s| s.parse().ok());
    let since_id: Option<i64> = params
        .pagination
        .since_id
        .as_deref()
        .and_then(|s| s.parse().ok());
    let min_id: Option<i64> = params
        .pagination
        .min_id
        .as_deref()
        .and_then(|s| s.parse().ok());

    let mut statuses = sqlx::query_as!(
        crate::db::models::Status,
        r#"SELECT s.* FROM statuses s
           JOIN preview_cards_statuses spc ON spc.status_id = s.id
           JOIN accounts a ON a.id = s.account_id
           LEFT JOIN users au ON au.account_id = a.id
           WHERE spc.preview_card_id = $1
             AND s.visibility = 0
             AND s.deleted_at IS NULL
             -- `public_scope` and `Account.discoverable`.
             AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
             AND a.silenced_at IS NULL AND a.moved_to_account_id IS NULL
             AND a.discoverable IS TRUE
             AND (a.domain IS NOT NULL OR (au.approved AND au.confirmed_at IS NOT NULL))
             AND EXISTS (SELECT 1 FROM account_stats ast WHERE ast.account_id = a.id)
             AND (NOT $6::bool OR a.domain IS NULL)
             AND (NOT $7::bool OR a.domain IS NOT NULL)
             -- `account_filters_scope`
             AND ($8::bigint IS NULL OR NOT EXISTS (
                 SELECT 1 FROM blocks b
                 WHERE (b.account_id = $8 AND b.target_account_id = s.account_id)
                    OR (b.account_id = s.account_id AND b.target_account_id = $8)
             ))
             AND ($8::bigint IS NULL OR NOT EXISTS (
                 SELECT 1 FROM mutes mu
                 WHERE mu.account_id = $8 AND mu.target_account_id = s.account_id
             ))
             AND ($8::bigint IS NULL OR $6 OR a.domain IS NULL OR NOT EXISTS (
                 SELECT 1 FROM account_domain_blocks udb
                 WHERE udb.account_id = $8 AND udb.domain = a.domain
             ))
             -- `language_scope`
             AND ($8::bigint IS NULL OR NOT EXISTS (
                 SELECT 1 FROM users vu WHERE vu.account_id = $8
                   AND cardinality(vu.chosen_languages) > 0
                   AND (s.language IS NULL OR NOT (s.language = ANY(vu.chosen_languages)))
             ))
             AND ($2::bigint IS NULL OR s.id < $2)
             AND ($3::bigint IS NULL OR s.id > $3)
             AND ($4::bigint IS NULL OR s.id > $4)
           ORDER BY CASE WHEN $4::bigint IS NULL THEN -s.id ELSE s.id END
           LIMIT $5"#,
        card_id,
        max_id,
        since_id,
        min_id,
        limit,
        reach.local_only,
        reach.remote_only,
        viewer_id,
    )
    .fetch_all(&state.db)
    .await?;
    // `to_a_paginated_by_id`: a `min_id` page is taken oldest first, then
    // turned around.
    if min_id.is_some() {
        statuses.reverse();
    }

    let result = build_status_list_with_filters(&state, statuses, viewer_id).await?;
    Ok(with_pagination_link(&headers, &uri, result))
}
