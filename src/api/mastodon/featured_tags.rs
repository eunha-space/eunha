use axum::{
    extract::{Extension, Path},
    Json,
};
use serde::Deserialize;

use super::types::{FeaturedTag, Tag};
use crate::{
    error::{AppError, AppResult},
    middleware::{AuthenticatedUser, ResolvedInstance},
    state::AppState,
};

fn featured_tag_url(domain: &str, username: &str, name: &str) -> String {
    format!("https://{domain}/@{username}/tagged/{name}")
}

fn tag_url(domain: &str, name: &str) -> String {
    format!("https://{domain}/tags/{name}")
}

// ── CreateFeaturedTagService / RemoveFeaturedTagService ──────────────────

/// What a featured tag is featured by: the name as written
/// (`POST /api/v1/featured_tags`), or a tag (`POST /api/v1/tags/:name/feature`).
pub(crate) enum Featuring<'a> {
    Name(&'a str),
    Tag(i64),
}

/// A featured tag, as the REST API shows it.
pub(crate) struct Featured {
    pub id: i64,
    pub tag_name: String,
    pub display_name: String,
    pub statuses_count: i64,
    pub last_status_at: Option<chrono::NaiveDateTime>,
}

async fn featured_by_id(state: &AppState, id: i64) -> AppResult<Option<Featured>> {
    Ok(sqlx::query!(
        r#"SELECT ft.id, ft.name AS featured, ft.statuses_count, ft.last_status_at,
                  t.name, t.display_name
           FROM featured_tags ft JOIN tags t ON t.id = ft.tag_id WHERE ft.id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .map(|r| Featured {
        id: r.id,
        display_name: crate::api::ap::featured_tags::display_name(
            r.featured.as_deref(),
            r.display_name.as_deref(),
            &r.name,
        ),
        tag_name: r.name,
        statuses_count: r.statuses_count,
        last_status_at: r.last_status_at,
    }))
}

/// `CreateFeaturedTagService`: feature a tag for the local account
/// `account_id`, or find the one already featured under that name (or
/// that tag). A new one is counted (`FeaturedTag#reset_data`) and its `Add`
/// sent to everyone the account reaches.
pub(crate) async fn create_featured_tag(
    state: &AppState,
    account_id: i64,
    featuring: Featuring<'_>,
) -> AppResult<Featured> {
    let unprocessable =
        |message: &str| AppError::Unprocessable(format!("Validation failed: {message}"));
    // `normalizes :name`: stripped, and without its `#`.
    let (name, tag_id) = match featuring {
        Featuring::Name(written) => {
            let name = written.trim();
            let name = name.strip_prefix('#').unwrap_or(name).to_owned();
            // `find_or_initialize_by(name:)`.
            let existing = sqlx::query_scalar!(
                "SELECT id FROM featured_tags WHERE account_id = $1 AND name = $2",
                account_id,
                name,
            )
            .fetch_optional(&state.db)
            .await?;
            if let Some(id) = existing {
                return featured_by_id(state, id).await?.ok_or(AppError::NotFound);
            }
            if name.is_empty() {
                return Err(unprocessable("Name can't be blank"));
            }
            if name.chars().any(|c| {
                !(c.is_alphanumeric() || c == '_' || c == '·' || c == '\u{30FB}' || c == '\u{200C}')
            }) {
                return Err(unprocessable("Name is invalid"));
            }
            let tag_id = crate::tags::find_or_create(&state.db, &name)
                .await?
                .ok_or_else(|| unprocessable("Tag can't be blank"))?;
            (Some(name), tag_id)
        }
        Featuring::Tag(tag_id) => {
            // `find_or_initialize_by(tag:)`.
            let existing = sqlx::query_scalar!(
                "SELECT id FROM featured_tags WHERE account_id = $1 AND tag_id = $2",
                account_id,
                tag_id,
            )
            .fetch_optional(&state.db)
            .await?;
            if let Some(id) = existing {
                return featured_by_id(state, id).await?.ok_or(AppError::NotFound);
            }
            (None, tag_id)
        }
    };

    // `validates :tag_id, uniqueness: { scope: :account_id }`.
    let taken = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM featured_tags WHERE account_id = $1 AND tag_id = $2) AS "e!""#,
        account_id,
        tag_id,
    )
    .fetch_one(&state.db)
    .await?;
    if taken {
        return Err(unprocessable("Tag has already been taken"));
    }
    // `validate_featured_tags_limit`: `FeaturedTag::LIMIT`.
    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "c!" FROM featured_tags WHERE account_id = $1"#,
        account_id,
    )
    .fetch_one(&state.db)
    .await?;
    if count >= 10 {
        return Err(unprocessable(
            "You have already reached the limit of 10 featured hashtags",
        ));
    }

    // `reset_data`: the account's public and unlisted posts with the tag,
    // and when the latest was posted.
    let id = sqlx::query_scalar!(
        r#"WITH visible AS (
             SELECT s.id, s.created_at FROM statuses s
             JOIN statuses_tags st ON st.status_id = s.id AND st.tag_id = $2
             WHERE s.account_id = $1 AND s.deleted_at IS NULL AND s.visibility IN (0, 1)
           )
           INSERT INTO featured_tags
             (account_id, tag_id, name, statuses_count, last_status_at, created_at, updated_at)
           VALUES ($1, $2, $3,
                   (SELECT COUNT(*) FROM visible),
                   (SELECT created_at FROM visible ORDER BY id DESC LIMIT 1),
                   now(), now())
           ON CONFLICT (account_id, tag_id) DO NOTHING
           RETURNING id"#,
        account_id,
        tag_id,
        name,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| unprocessable("Tag has already been taken"))?;
    let featured = featured_by_id(state, id).await?.ok_or(AppError::NotFound)?;
    distribute_featured_tag(state, account_id, &featured, true).await;
    Ok(featured)
}

/// `RemoveFeaturedTagService`: stop featuring `featured_tag_id` and send its
/// `Remove` to everyone the account reaches. Nothing, if it is not the
/// account's.
pub(crate) async fn remove_featured_tag(
    state: &AppState,
    account_id: i64,
    featured_tag_id: i64,
) -> AppResult<bool> {
    let Some(featured) = featured_by_id(state, featured_tag_id).await? else {
        return Ok(false);
    };
    let deleted = sqlx::query!(
        "DELETE FROM featured_tags WHERE id = $1 AND account_id = $2",
        featured_tag_id,
        account_id,
    )
    .execute(&state.db)
    .await?
    .rows_affected();
    if deleted == 0 {
        return Ok(false);
    }
    distribute_featured_tag(state, account_id, &featured, false).await;
    Ok(true)
}

/// `ActivityPub::AccountRawDistributionWorker` with an `Add` or `Remove` of
/// a hashtag: to `AccountReachFinder`'s inboxes, with the Linked Data
/// signature `FeaturedTag#sign?` asks for outside authorized fetch mode.
async fn distribute_featured_tag(
    state: &AppState,
    account_id: i64,
    featured: &Featured,
    add: bool,
) {
    let result: anyhow::Result<()> = async {
        let Some(account) = sqlx::query_as!(
            crate::db::models::Account,
            "SELECT * FROM accounts WHERE id = $1 AND domain IS NULL",
            account_id,
        )
        .fetch_optional(&state.db)
        .await?
        else {
            return Ok(());
        };
        if !crate::federation::keypair::has_signing_key(state, account.id)
            .await
            .unwrap_or(false)
        {
            return Ok(());
        }
        let domain = &state.instance.domain;
        let own = crate::api::ap::serving::AccountUris::of(&state.uris, &account);
        let actor = own.actor()?;
        let hashtag = crate::api::ap::featured_tags::hashtag(
            domain,
            &account.username,
            &featured.tag_name,
            &featured.display_name,
        );
        let activity = crate::api::ap::featured_tags::activity(
            add,
            actor.as_str(),
            own.uri(crate::api::ap::serving::Own::Featured)?.as_str(),
            hashtag,
        );
        let inboxes = crate::federation::delivery::account_reach_inboxes(state, account.id).await?;
        crate::federation::delivery::deliver_to_inboxes_signed(
            state,
            activity,
            inboxes,
            own.key_id()?.into(),
            crate::federation::delivery::LinkedData::UnlessAuthorizedFetch,
        )
        .await?;
        Ok(())
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, "could not distribute a featured tag");
    }
}

fn featured_tag_entity(domain: &str, username: &str, featured: &Featured) -> FeaturedTag {
    FeaturedTag {
        id: featured.id.to_string(),
        name: featured.display_name.clone(),
        url: featured_tag_url(domain, username, &featured.tag_name),
        statuses_count: featured.statuses_count.to_string(),
        last_status_at: featured
            .last_status_at
            .map(|t| t.format("%Y-%m-%d").to_string()),
    }
}

// ── GET /api/v1/featured_tags ─────────────────────────────────────────────

pub async fn list_featured_tags(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<FeaturedTag>>> {
    auth.require_scope("read:accounts")?;
    let domain = &instance.domain;

    let username = sqlx::query_scalar!(
        "SELECT username FROM accounts WHERE id = $1",
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?;

    let ids = sqlx::query_scalar!(
        "SELECT id FROM featured_tags WHERE account_id = $1 ORDER BY statuses_count DESC",
        auth.account_id,
    )
    .fetch_all(&state.db)
    .await?;

    let mut tags = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(featured) = featured_by_id(&state, id).await? {
            tags.push(featured_tag_entity(domain, &username, &featured));
        }
    }
    Ok(Json(tags))
}

// ── POST /api/v1/featured_tags ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct FeaturedTagForm {
    pub name: String,
}

pub async fn feature_tag(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<FeaturedTagForm>,
) -> AppResult<Json<FeaturedTag>> {
    auth.require_scope("write:accounts")?;
    let username = sqlx::query_scalar!(
        "SELECT username FROM accounts WHERE id = $1",
        auth.account_id,
    )
    .fetch_one(&state.db)
    .await?;
    let featured =
        create_featured_tag(&state, auth.account_id, Featuring::Name(&form.name)).await?;
    Ok(Json(featured_tag_entity(
        &instance.domain,
        &username,
        &featured,
    )))
}

// ── DELETE /api/v1/featured_tags/:id ─────────────────────────────────────

pub async fn unfeature_tag(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("write:accounts")?;
    if !remove_featured_tag(&state, auth.account_id, id).await? {
        return Err(AppError::NotFound);
    }
    Ok(Json(serde_json::json!({})))
}

// ── POST /api/v1/tags/:name/feature ──────────────────────────────────────

pub async fn feature_tag_by_name(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> AppResult<Json<Tag>> {
    auth.require_scope("write:accounts")?;
    let domain = &instance.domain;
    let written = name.clone();
    let name = name.to_lowercase();
    let name = name.trim_start_matches('#');

    let tag_id = crate::tags::find_or_create(&state.db, &written)
        .await?
        .ok_or_else(|| AppError::Unprocessable("Validation failed: Tag is invalid".into()))?;

    create_featured_tag(&state, auth.account_id, Featuring::Tag(tag_id)).await?;

    let following = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM tag_follows WHERE account_id = $1 AND tag_id = $2)",
        auth.account_id,
        tag_id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false);

    let history = super::tags::fetch_tag_history(&state, tag_id).await;

    Ok(Json(Tag {
        id: tag_id.to_string(),
        url: tag_url(domain, name),
        name: name.to_string(),
        history,
        following: Some(following),
        featuring: Some(true),
    }))
}

// ── POST /api/v1/tags/:name/unfeature ────────────────────────────────────

pub async fn unfeature_tag_by_name(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(name): Path<String>,
) -> AppResult<Json<Tag>> {
    auth.require_scope("write:accounts")?;
    let domain = &instance.domain;
    let name = name.to_lowercase();

    let tag = sqlx::query!("SELECT id FROM tags WHERE name = $1", name,)
        .fetch_optional(&state.db)
        .await?;

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

    // `RemoveFeaturedTagService` on the tag: the account's featured tag for
    // it, if there is one.
    let featured = sqlx::query_scalar!(
        "SELECT id FROM featured_tags WHERE account_id = $1 AND tag_id = $2",
        auth.account_id,
        tag.id,
    )
    .fetch_optional(&state.db)
    .await?;
    if let Some(featured) = featured {
        remove_featured_tag(&state, auth.account_id, featured).await?;
    }

    let following = sqlx::query_scalar!(
        "SELECT EXISTS(SELECT 1 FROM tag_follows WHERE account_id = $1 AND tag_id = $2)",
        auth.account_id,
        tag.id,
    )
    .fetch_one(&state.db)
    .await?
    .unwrap_or(false);

    let history = super::tags::fetch_tag_history(&state, tag.id).await;

    Ok(Json(Tag {
        id: tag.id.to_string(),
        url: tag_url(domain, &name),
        name,
        history,
        following: Some(following),
        featuring: Some(false),
    }))
}

// ── GET /api/v1/featured_tags/suggestions ────────────────────────────────

pub async fn featured_tag_suggestions(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<super::types::Tag>>> {
    auth.require_scope("read:accounts")?;
    let domain = &instance.domain;

    let rows = sqlx::query!(
        r#"SELECT t.id, t.name
           FROM tags t
           JOIN statuses_tags st ON st.tag_id = t.id
           JOIN statuses s ON s.id = st.status_id
           WHERE s.account_id = $1 AND s.deleted_at IS NULL
           GROUP BY t.id, t.name
           ORDER BY COUNT(*) DESC
           LIMIT 10"#,
        auth.account_id,
    )
    .fetch_all(&state.db)
    .await?;

    let tags = rows
        .into_iter()
        .map(|r| super::types::Tag {
            id: r.id.to_string(),
            url: format!("https://{}/tags/{}", domain, r.name),
            name: r.name,
            history: vec![],
            following: None,
            featuring: None,
        })
        .collect();

    Ok(Json(tags))
}
