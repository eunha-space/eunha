//! Inbound `Add` / `Remove` (`ActivityPub::Activity::Add`, `::Remove`): the
//! sender's pinned posts and featured hashtags (its `featured` collection),
//! its collections (its `featuredCollections`), and the items of one of its
//! collections. Everything is looked up among the sender's own: no actor can
//! pin another's post, nor touch another's collection.

use serde_json::Value;

use crate::{error::AppResult, state::AppState};

use crate::federation::featured_collections as featured;
use ojak_vocab::json_ld_helper::{first_of_value, value_or_id};

/// The sender, with the collections an `Add` or `Remove` may target.
struct Sender {
    id: i64,
    uri: String,
    featured_collection_url: Option<String>,
    collections_url: Option<String>,
}

async fn sender(state: &AppState, actor_uri: &str) -> AppResult<Option<Sender>> {
    if actor_uri.is_empty() {
        return Ok(None);
    }
    let Ok(id) = super::resolve_or_fetch_remote_account(state, actor_uri).await else {
        return Ok(None);
    };
    Ok(sqlx::query!(
        r#"SELECT id, uri AS "uri!", featured_collection_url, collections_url
           FROM accounts WHERE id = $1 AND domain IS NOT NULL AND uri IS NOT NULL"#,
        id,
    )
    .fetch_optional(&state.db)
    .await?
    .map(|row| Sender {
        id: row.id,
        uri: row.uri,
        featured_collection_url: row.featured_collection_url.filter(|url| !url.is_empty()),
        collections_url: row.collections_url.filter(|url| !url.is_empty()),
    }))
}

/// Whether `object` is an embedded object of `type_name`.
fn is_type(object: Option<&Value>, type_name: &str) -> bool {
    object
        .and_then(|object| object.get("type"))
        .and_then(Value::as_str)
        == Some(type_name)
}

/// A `Hashtag`'s name without its `#`, if it has one.
fn hashtag_name(object: Option<&Value>) -> Option<String> {
    object
        .and_then(|object| object.get("name"))
        .and_then(Value::as_str)
        .map(|name| name.trim().trim_start_matches('#').to_owned())
        .filter(|name| !name.is_empty())
}

pub(super) async fn handle_add(state: &AppState, activity: &Value) -> AppResult<()> {
    let Some(target) = activity
        .get("target")
        .and_then(value_or_id)
        .filter(|t| !t.trim().is_empty())
    else {
        return Ok(());
    };
    let actor_uri = activity.get("actor").and_then(value_or_id).unwrap_or("");
    let Some(sender) = sender(state, actor_uri).await? else {
        return Ok(());
    };
    let object = activity.get("object");

    if sender.featured_collection_url.as_deref() == Some(target) {
        if is_type(object, "Hashtag") {
            return add_featured_tag(state, sender.id, object).await;
        }
        return add_featured(state, activity, &sender).await;
    }
    if sender.collections_url.as_deref() == Some(target) {
        // `ProcessFeaturedCollectionService.new.call(@account, @object)`.
        if let Some(object) = object.filter(|o| o.is_object()) {
            featured::process_featured_collection(state, sender.id, &sender.uri, object)
                .await
                .map_err(crate::error::AppError::Internal)?;
        }
        return Ok(());
    }
    // `@account.collections.find_by(uri: target)`, and the item processed
    // into it.
    let Some(collection_id) = sqlx::query_scalar!(
        "SELECT id FROM collections WHERE account_id = $1 AND uri = $2",
        sender.id,
        target,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    if let Some(object) = object {
        Box::pin(featured::process_featured_item(
            state,
            collection_id,
            object,
            None,
        ))
        .await
        .map_err(crate::error::AppError::Internal)?;
    }
    Ok(())
}

/// `add_featured`: the sender pins one of its own posts, found or fetched
/// (`status_from_object`), that it has not pinned yet and that may be
/// pinned (`StatusPinValidator`: not a boost, not a direct message).
async fn add_featured(state: &AppState, activity: &Value, sender: &Sender) -> AppResult<()> {
    let Some(status_id) = status_from_object(state, activity, sender).await? else {
        return Ok(());
    };
    let pinnable = sqlx::query_scalar!(
        r#"SELECT (account_id = $2 AND reblog_of_id IS NULL AND visibility <> 3) AS "ok!"
           FROM statuses WHERE id = $1"#,
        status_id,
        sender.id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(false);
    if !pinnable {
        return Ok(());
    }
    sqlx::query!(
        r#"INSERT INTO status_pins (id, account_id, status_id, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now()) ON CONFLICT DO NOTHING"#,
        crate::snowflake::next_id(),
        sender.id,
        status_id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `ActivityPub::Activity#status_from_object`: the status the object names,
/// if known; an embedded post of the sender's own, taken as the `Create` it
/// is; or else fetched, on behalf of a local follower of the sender.
async fn status_from_object(
    state: &AppState,
    activity: &Value,
    sender: &Sender,
) -> AppResult<Option<i64>> {
    let object = activity.get("object");
    let Some(object_uri) = object.and_then(value_or_id) else {
        return Ok(None);
    };
    if let Some(id) = super::kept_status(state, object_uri).await? {
        return Ok(Some(id));
    }
    if let Some(embedded) = object.filter(|o| o.is_object()) {
        if super::status_parser::is_status_type(embedded)
            && embedded
                .get("attributedTo")
                .and_then(first_of_value)
                .and_then(value_or_id)
                == Some(sender.uri.as_str())
        {
            let virtual_create = serde_json::json!({
                "type": "Create",
                "actor": sender.uri,
                "object": embedded,
            });
            Box::pin(super::create::create(
                state,
                &virtual_create,
                &super::create::CreateOptions {
                    fetched: true,
                    ..Default::default()
                },
            ))
            .await?;
            return super::kept_status(state, object_uri).await;
        }
    }
    // `fetch_remote_original_status`.
    if object_uri.starts_with("http") {
        if crate::federation::local_uri::is_local(state, object_uri) {
            return Ok(None);
        }
        return Ok(Box::pin(super::fetch_remote_status_with(
            state,
            object_uri,
            super::FetchOptions {
                on_behalf_of: crate::federation::json_ld::local_follower(state, sender.id).await,
                ..Default::default()
            },
        ))
        .await?
        .map(|(id, _)| id));
    }
    if let Some(url) = object
        .and_then(|o| o.get("url"))
        .and_then(Value::as_str)
        .filter(|url| !url.trim().is_empty())
    {
        return Box::pin(super::fetch_remote_status(state, url)).await;
    }
    Ok(None)
}

/// `add_featured_tags`: `FeaturedTag.create!(account:, name:)`, counted as
/// it is created (`reset_data`); one already featured stays as it is.
async fn add_featured_tag(
    state: &AppState,
    account_id: i64,
    object: Option<&Value>,
) -> AppResult<()> {
    let Some(name) = hashtag_name(object) else {
        return Ok(());
    };
    let Some(tag_id) = crate::tags::find_or_create(&state.db, &name).await? else {
        return Ok(());
    };
    sqlx::query!(
        r#"INSERT INTO featured_tags
             (account_id, tag_id, name, statuses_count, last_status_at, created_at, updated_at)
           SELECT $1, $2, $3, count(s.id), max(s.created_at), now(), now()
           FROM statuses s JOIN statuses_tags st ON st.status_id = s.id AND st.tag_id = $2
           WHERE s.account_id = $1 AND s.deleted_at IS NULL AND s.visibility IN (0, 1)
           ON CONFLICT (account_id, tag_id) DO NOTHING"#,
        account_id,
        tag_id,
        name,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

pub(super) async fn handle_remove(state: &AppState, activity: &Value) -> AppResult<()> {
    let Some(target) = activity
        .get("target")
        .and_then(value_or_id)
        .filter(|t| !t.trim().is_empty())
    else {
        return Ok(());
    };
    let actor_uri = activity.get("actor").and_then(value_or_id).unwrap_or("");
    let Some(sender) = sender(state, actor_uri).await? else {
        return Ok(());
    };
    let object = activity.get("object");
    let object_uri = object.and_then(value_or_id);

    if sender.featured_collection_url.as_deref() == Some(target) {
        if is_type(object, "Hashtag") {
            // `remove_featured_tags`: `FeaturedTag.by_name(name)
            // .find_by(account:)&.destroy!`, the name normalized as `Tag`
            // normalizes it.
            if let Some(name) = hashtag_name(object) {
                let name = crate::search::tags::normalize(&name);
                sqlx::query!(
                    r#"DELETE FROM featured_tags WHERE id = (
                         SELECT ft.id FROM featured_tags ft JOIN tags t ON t.id = ft.tag_id
                         WHERE ft.account_id = $1 AND t.name = $2 ORDER BY ft.id LIMIT 1)"#,
                    sender.id,
                    name,
                )
                .execute(&state.db)
                .await?;
            }
            return Ok(());
        }
        // `remove_featured`: the sender's own post, unpinned.
        let Some(object_uri) = object_uri else {
            return Ok(());
        };
        if let Some(status_id) = super::kept_status(state, object_uri).await? {
            sqlx::query!(
                r#"DELETE FROM status_pins WHERE account_id = $1 AND status_id = $2
                     AND EXISTS (SELECT 1 FROM statuses WHERE id = $2 AND account_id = $1)"#,
                sender.id,
                status_id,
            )
            .execute(&state.db)
            .await?;
        }
        return Ok(());
    }
    if sender.collections_url.as_deref() == Some(target) {
        // `remove_collection`: `@account.collections.find_by(uri:)&.destroy!`.
        let Some(object_uri) = object_uri else {
            return Ok(());
        };
        let removed = sqlx::query_scalar!(
            "DELETE FROM collections WHERE account_id = $1 AND uri = $2 RETURNING id",
            sender.id,
            object_uri,
        )
        .fetch_optional(&state.db)
        .await?;
        if let Some(id) = removed {
            crate::api::mastodon::collections::destroy_notifications(&state.db, id).await?;
        }
        return Ok(());
    }
    // `remove_collection_item`: the item of one of the sender's
    // collections, destroyed and uncounted.
    let Some(object_uri) = object_uri else {
        return Ok(());
    };
    let removed = sqlx::query_scalar!(
        r#"DELETE FROM collection_items ci USING collections c
           WHERE c.id = ci.collection_id AND c.account_id = $1 AND c.uri = $2
             AND ci.uri = $3
           RETURNING ci.collection_id"#,
        sender.id,
        target,
        object_uri,
    )
    .fetch_optional(&state.db)
    .await?;
    if let Some(collection_id) = removed {
        crate::api::mastodon::collections::update_item_count(&state.db, collection_id, -1).await?;
    }
    Ok(())
}
