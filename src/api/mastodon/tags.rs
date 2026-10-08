use super::types::{Tag, TagHistory};
use crate::{
    error::{AppError, AppResult},
    middleware::{AuthenticatedUser, ResolvedInstance},
    state::AppState,
};
use axum::{
    extract::{Path, Query},
    http::{HeaderMap, Uri},
    response::{IntoResponse, Json},
    Extension,
};
use std::collections::HashMap;

// ── Tag history helpers ────────────────────────────────────────────────────

/// Each tag's `Trends::History`, seven days of uses and distinct users
/// counted as they happened, newest day first.
pub(super) async fn fetch_tags_histories(
    state: &AppState,
    tag_ids: &[i64],
) -> HashMap<i64, Vec<TagHistory>> {
    let mut histories = HashMap::new();
    for &id in tag_ids {
        histories.insert(id, fetch_tag_history(state, id).await);
    }
    histories
}

pub(super) async fn fetch_tag_history(state: &AppState, tag_id: i64) -> Vec<TagHistory> {
    crate::moderation::history::days(state, "tags", tag_id)
        .await
        .into_iter()
        .map(|d| TagHistory {
            day: d.day.to_string(),
            uses: d.uses.to_string(),
            accounts: d.accounts.to_string(),
        })
        .collect()
}

fn tag_url(domain: &str, name: &str) -> String {
    format!("https://{domain}/tags/{name}")
}

// ── GET /api/v1/tags/:name ────────────────────────────────────────────────

pub async fn get_tag(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Path(name): Path<String>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Tag>> {
    let domain = &instance.domain;
    let name = name.to_lowercase();

    // Mastodon's TagsController#show uses Tag.find_normalized! which raises
    // RecordNotFound (→ 404) for hashtags that do not exist.
    let tag = sqlx::query!("SELECT id FROM tags WHERE name = $1", name,)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;

    let (following, featuring, history, id_str) = {
        let t = &tag;
        let history = fetch_tag_history(&state, t.id).await;
        let (following, featuring) = if let Some(Extension(ref auth)) = auth {
            let following = sqlx::query_scalar!(
                r#"SELECT EXISTS(
                   SELECT 1 FROM tag_follows tf
                   WHERE tf.account_id = $1 AND tf.tag_id = $2
                )"#,
                auth.account_id,
                t.id,
            )
            .fetch_one(&state.db)
            .await?
            .unwrap_or(false);

            let featuring = sqlx::query_scalar!(
                "SELECT EXISTS(SELECT 1 FROM featured_tags WHERE account_id = $1 AND tag_id = $2)",
                auth.account_id,
                t.id,
            )
            .fetch_one(&state.db)
            .await?
            .unwrap_or(false);

            (Some(following), Some(featuring))
        } else {
            (None, None)
        };
        (following, featuring, history, t.id.to_string())
    };

    Ok(Json(Tag {
        id: id_str,
        url: tag_url(domain, &name),
        name,
        history,
        following,
        featuring,
    }))
}

#[derive(Debug, serde::Deserialize)]
pub struct FollowedTagsParams {
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_int"
    )]
    limit: Option<i64>,
    max_id: Option<String>,
    since_id: Option<String>,
    min_id: Option<String>,
}

pub async fn list_followed_tags(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(params): Query<FollowedTagsParams>,
    uri: Uri,
    req_headers: HeaderMap,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("read:follows")?;
    let domain = &instance.domain;
    let limit = params.limit.unwrap_or(100).clamp(1, 200);
    let max_id = params.max_id.as_deref().and_then(|s| s.parse::<i64>().ok());
    let since_id = params
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let min_id = params.min_id.as_deref().and_then(|s| s.parse::<i64>().ok());

    let rows = sqlx::query!(
        r#"SELECT tf.id AS follow_id, t.id, t.name
           FROM tag_follows tf
           JOIN tags t ON t.id = tf.tag_id
           WHERE tf.account_id = $1
             AND ($2::bigint IS NULL OR tf.id < $2)
             AND ($3::bigint IS NULL OR tf.id > $3)
             AND ($4::bigint IS NULL OR tf.id > $4)
           ORDER BY CASE WHEN $4::bigint IS NULL THEN -tf.id ELSE tf.id END
           LIMIT $5"#,
        auth.account_id,
        max_id,
        since_id,
        min_id,
        limit,
    )
    .fetch_all(&state.db)
    .await?;
    let rows = super::timelines::newest_first(min_id, rows);

    // Use tag_follow id (bigint) as the pagination cursor, not the tag UUID.
    let first_follow_id = rows.first().map(|r| r.follow_id.to_string());
    let last_follow_id = rows.last().map(|r| r.follow_id.to_string());

    let tag_ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let histories = fetch_tags_histories(&state, &tag_ids).await;

    let result: Vec<Tag> = rows
        .into_iter()
        .map(|r| Tag {
            id: r.id.to_string(),
            url: tag_url(domain, &r.name),
            history: histories.get(&r.id).cloned().unwrap_or_default(),
            name: r.name,
            following: Some(true),
            featuring: None,
        })
        .collect();

    let bounds = first_follow_id.zip(last_follow_id);
    let resp_headers = super::link_headers(
        &req_headers,
        &uri,
        bounds.as_ref().map(|(n, o)| (n.as_str(), o.as_str())),
    );
    Ok((resp_headers, Json(result)))
}

pub async fn follow_tag(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> AppResult<Json<Tag>> {
    auth.require_scope("write:follows")?;
    let domain = &instance.domain;
    let written = name.clone();
    let name = name.to_lowercase();

    // `Tag.find_or_create_by_names(params[:id])`.
    let tag_id = crate::tags::find_or_create(&state.db, &written)
        .await?
        .ok_or(AppError::NotFound)?;

    crate::rate_limit::record_tag_follow(&state, auth.account_id, tag_id).await?;
    sqlx::query!(
        "INSERT INTO tag_follows (account_id, tag_id, created_at, updated_at) VALUES ($1, $2, now(), now()) ON CONFLICT DO NOTHING",
        auth.account_id,
        tag_id,
    )
    .execute(&state.db)
    .await?;

    let history = fetch_tag_history(&state, tag_id).await;

    let featuring = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM featured_tags WHERE account_id = $1 AND tag_id = $2)",
        auth.account_id,
        tag_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false);

    Ok(Json(Tag {
        id: tag_id.to_string(),
        url: tag_url(domain, &name),
        name,
        history,
        following: Some(true),
        featuring: Some(featuring),
    }))
}

pub async fn unfollow_tag(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> AppResult<Json<Tag>> {
    auth.require_scope("write:follows")?;
    let domain = &instance.domain;
    let name = name.to_lowercase();

    let tag = sqlx::query!("SELECT id FROM tags WHERE name = $1", name,)
        .fetch_optional(&state.db)
        .await?;

    // If tag doesn't exist there's nothing to unfollow; return empty-id tag (matches Mastodon).
    let Some(tag) = tag else {
        return Ok(Json(Tag {
            id: String::new(),
            url: tag_url(domain, &name),
            name,
            history: vec![],
            following: Some(false),
            featuring: Some(false),
        }));
    };

    sqlx::query!(
        "DELETE FROM tag_follows WHERE account_id = $1 AND tag_id = $2",
        auth.account_id,
        tag.id,
    )
    .execute(&state.db)
    .await?;
    // `TagUnmergeWorker.perform_async(@tag.id, current_account.id)`.
    let job = TagUnmergeWorker {
        from_tag_id: tag.id,
        into_account_id: auth.account_id,
    };
    if crate::feed::sync_fanout() {
        job.unmerge(&state).await?;
    } else {
        crate::jobs::push(&state, job).await;
    }

    let history = fetch_tag_history(&state, tag.id).await;

    let featuring = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM featured_tags WHERE account_id = $1 AND tag_id = $2)",
        auth.account_id,
        tag.id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false);

    Ok(Json(Tag {
        id: tag.id.to_string(),
        url: tag_url(domain, &name),
        name,
        history,
        following: Some(false),
        featuring: Some(featuring),
    }))
}

/// `TagUnmergeWorker`, in the `pull` queue: the posts of a hashtag just
/// unfollowed out of the home feed (`FeedManager#unmerge_tag_from_home`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TagUnmergeWorker {
    pub from_tag_id: i64,
    pub into_account_id: i64,
}

impl TagUnmergeWorker {
    async fn unmerge(&self, state: &AppState) -> anyhow::Result<()> {
        // `rescue ActiveRecord::RecordNotFound`: a tag or account gone since.
        let found = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM tags WHERE id = $1)
                  AND EXISTS (SELECT 1 FROM accounts WHERE id = $2) AS "found!""#,
            self.from_tag_id,
            self.into_account_id,
        )
        .fetch_one(&state.db)
        .await?;
        if found {
            let mut redis = state.redis.clone();
            crate::feed::unmerge_tag_from_home(
                &mut redis,
                &state.redis_keys,
                &state.db,
                self.from_tag_id,
                self.into_account_id,
            )
            .await;
        }
        Ok(())
    }
}

impl crate::jobs::Job for TagUnmergeWorker {
    const KIND: &'static str = "TagUnmergeWorker";
    const OPTIONS: crate::jobs::Options =
        crate::jobs::Options::DEFAULT.queue(crate::jobs::Queue::Pull);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        self.unmerge(state).await
    }
}
