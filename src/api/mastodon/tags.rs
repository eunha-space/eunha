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

/// `tag_url(tag)`: by the tag's `to_param`, its name.
pub(super) fn tag_url(domain: &str, name: &str) -> String {
    crate::formatter::text::tag_url(domain, name)
}

/// The tag `TagsController#set_or_create_tag` sets: the one
/// `Tag.find_normalized` finds, or an unsaved `Tag.new(name:, display_name:)`
/// of the name given, which has no id.
pub(super) struct TagRef {
    pub id: Option<i64>,
    /// The normalized name: the URL's.
    pub name: String,
    /// `Tag#display_name`, which `REST::TagSerializer` gives as the name.
    pub display_name: String,
}

/// `TagsController#set_or_create_tag`: a 404 for a name that is no
/// hashtag (`HASHTAG_NAME_RE`), else the tag of that name, saved or not.
pub(super) async fn set_or_create_tag(state: &AppState, param: &str) -> AppResult<TagRef> {
    if !crate::formatter::extractor::HASHTAG_NAME_RE.is_match(param) {
        return Err(AppError::NotFound);
    }
    Ok(
        match crate::search::tags::find_normalized(state, param).await? {
            Some(found) => TagRef {
                id: Some(found.id),
                name: found.name,
                display_name: found.display_name,
            },
            None => TagRef {
                id: None,
                name: crate::search::tags::normalize(param),
                display_name: crate::tags::display_name(param),
            },
        },
    )
}

/// `REST::TagSerializer`: `following` and `featuring` only for a user. An
/// unsaved tag has the empty id, no follows and a history of zeros.
pub(super) async fn render_tag(
    state: &AppState,
    domain: &str,
    tag: &TagRef,
    viewer_id: Option<i64>,
) -> AppResult<Tag> {
    let history = fetch_tag_history(state, tag.id.unwrap_or(0)).await;
    let (following, featuring) = match (viewer_id, tag.id) {
        (Some(viewer), Some(id)) => {
            let row = sqlx::query!(
                r#"SELECT
                     EXISTS(SELECT 1 FROM tag_follows WHERE account_id = $1 AND tag_id = $2) AS "following!",
                     EXISTS(SELECT 1 FROM featured_tags WHERE account_id = $1 AND tag_id = $2) AS "featuring!""#,
                viewer,
                id,
            )
            .fetch_one(&state.db)
            .await?;
            (Some(row.following), Some(row.featuring))
        }
        (Some(_), None) => (Some(false), Some(false)),
        (None, _) => (None, None),
    };
    Ok(Tag {
        id: tag.id.map(|id| id.to_string()).unwrap_or_default(),
        name: tag.display_name.clone(),
        url: tag_url(domain, &tag.name),
        history,
        following,
        featuring,
    })
}

// ── GET /api/v1/tags/:name ────────────────────────────────────────────────

/// `TagsController#show`: the tag, or for a hashtag nobody has used the
/// unsaved one, with the empty id.
pub async fn get_tag(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Path(name): Path<String>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Tag>> {
    let viewer_id = auth.as_ref().map(|Extension(a)| a.account_id);
    let tag = set_or_create_tag(&state, &name).await?;
    render_tag(&state, &instance.domain, &tag, viewer_id)
        .await
        .map(Json)
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
        r#"SELECT tf.id AS follow_id, t.id, t.name,
                  COALESCE(t.display_name, t.name) AS "display_name!"
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
    // `TagRelationshipsPresenter#featuring_map`.
    let featuring: Option<std::collections::HashSet<i64>> = Some(
        sqlx::query_scalar!(
            "SELECT tag_id FROM featured_tags WHERE account_id = $1 AND tag_id = ANY($2)",
            auth.account_id,
            &tag_ids,
        )
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .collect(),
    );

    let result: Vec<Tag> = rows
        .into_iter()
        .map(|r| Tag {
            id: r.id.to_string(),
            url: tag_url(domain, &r.name),
            history: histories.get(&r.id).cloned().unwrap_or_default(),
            name: r.display_name,
            following: Some(true),
            featuring: featuring.as_ref().map(|f| f.contains(&r.id)),
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

/// `TagsController#follow`: the follow, which saves an unsaved tag.
pub async fn follow_tag(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> AppResult<Json<Tag>> {
    auth.require_scope("write:follows")?;
    let mut tag = set_or_create_tag(&state, &name).await?;
    let tag_id = match tag.id {
        Some(id) => id,
        None => crate::tags::find_or_create(&state.db, &name)
            .await?
            .ok_or(AppError::NotFound)?,
    };
    tag.id = Some(tag_id);

    crate::rate_limit::record_tag_follow(&state, auth.account_id, tag_id).await?;
    sqlx::query!(
        "INSERT INTO tag_follows (account_id, tag_id, created_at, updated_at) VALUES ($1, $2, now(), now()) ON CONFLICT DO NOTHING",
        auth.account_id,
        tag_id,
    )
    .execute(&state.db)
    .await?;

    render_tag(&state, &instance.domain, &tag, Some(auth.account_id))
        .await
        .map(Json)
}

/// `TagsController#unfollow`.
pub async fn unfollow_tag(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> AppResult<Json<Tag>> {
    auth.require_scope("write:follows")?;
    let tag = set_or_create_tag(&state, &name).await?;

    if let Some(tag_id) = tag.id {
        sqlx::query!(
            "DELETE FROM tag_follows WHERE account_id = $1 AND tag_id = $2",
            auth.account_id,
            tag_id,
        )
        .execute(&state.db)
        .await?;
        // `TagUnmergeWorker.perform_async(@tag.id, current_account.id)`.
        let job = TagUnmergeWorker {
            from_tag_id: tag_id,
            into_account_id: auth.account_id,
        };
        if crate::feed::sync_fanout() {
            job.unmerge(&state).await?;
        } else {
            crate::jobs::push(&state, job).await;
        }
    }

    render_tag(&state, &instance.domain, &tag, Some(auth.account_id))
        .await
        .map(Json)
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
