//! Inbound `Add` / `Remove` activities: mirroring a remote actor's featured
//! collections/items and their pinned statuses into local rows.

use serde_json::Value;

use crate::{error::AppResult, state::AppState};

use super::{
    mirror_item_into, refresh_collection_item_count, resolve_or_fetch_remote_account,
    upsert_remote_collection,
};
use ojak_vocab::json_ld_helper::value_or_id;

pub(super) async fn handle_add(state: &AppState, activity: &Value) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let object_uri = activity
        .get("object")
        .and_then(value_or_id)
        .unwrap_or_default();
    let target_uri = activity
        .get("target")
        .and_then(value_or_id)
        .unwrap_or_default();

    if actor_uri.is_empty() || object_uri.is_empty() {
        return Ok(());
    }

    // Collection mirroring: Add(FeaturedCollection) / Add(FeaturedItem).
    if let Some(obj) = activity.get("object").filter(|o| o.is_object()) {
        match obj.get("type").and_then(|t| t.as_str()) {
            Some("FeaturedCollection") => {
                if let Ok(owner_id) = resolve_or_fetch_remote_account(state, actor_uri).await {
                    if let Some(cid) = upsert_remote_collection(state, owner_id, obj).await? {
                        if let Some(items) = obj.get("orderedItems").and_then(|v| v.as_array()) {
                            for it in items {
                                let _ = mirror_item_into(state, cid, it).await;
                            }
                        }
                    }
                }
                return Ok(());
            }
            Some("FeaturedItem") => {
                if let Some(cid) = sqlx::query_scalar!(
                    "SELECT id FROM collections WHERE uri = $1 AND local = false",
                    target_uri,
                )
                .fetch_optional(&state.db)
                .await?
                {
                    mirror_item_into(state, cid, obj).await?;
                }
                return Ok(());
            }
            _ => {}
        }
    }

    // Check that the target is the actor's featured collection (pinned posts)
    let featured_url = sqlx::query_scalar!(
        "SELECT featured_collection_url FROM accounts WHERE uri = $1",
        actor_uri,
    )
    .fetch_optional(&state.db)
    .await?
    .flatten()
    .unwrap_or_default();

    if target_uri != featured_url {
        return Ok(());
    }

    let account_id = sqlx::query_scalar!("SELECT id FROM accounts WHERE uri = $1", actor_uri,)
        .fetch_optional(&state.db)
        .await?;
    let status_id = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
        object_uri,
    )
    .fetch_optional(&state.db)
    .await?;

    if let (Some(aid), Some(sid)) = (account_id, status_id) {
        let pin_id = crate::snowflake::next_id();
        sqlx::query!(
            "INSERT INTO status_pins (id, account_id, status_id, created_at, updated_at) VALUES ($1,$2,$3,now(),now()) ON CONFLICT DO NOTHING",
            pin_id, aid, sid,
        ).execute(&state.db).await?;
    }

    Ok(())
}

pub(super) async fn handle_remove(state: &AppState, activity: &Value) -> AppResult<()> {
    let actor_uri = activity.get("actor").and_then(|a| a.as_str()).unwrap_or("");
    let object_uri = activity
        .get("object")
        .and_then(|o| {
            if o.is_string() {
                o.as_str()
            } else {
                o.get("id").and_then(|i| i.as_str())
            }
        })
        .unwrap_or("");

    // Collection mirroring: Remove(FeaturedCollection) deletes the mirrored
    // collection; Remove(FeaturedItem) deletes the mirrored item. Only the
    // collection owner (the activity actor) may do so.
    let removed_collection = sqlx::query!(
        r#"DELETE FROM collections
           WHERE uri = $1 AND local = false
             AND account_id = (SELECT id FROM accounts WHERE uri = $2)
           RETURNING id"#,
        object_uri,
        actor_uri,
    )
    .fetch_optional(&state.db)
    .await?;
    if removed_collection.is_some() {
        return Ok(());
    }
    let removed_item = sqlx::query!(
        r#"DELETE FROM collection_items
           WHERE uri = $1 AND collection_id IN (
               SELECT id FROM collections
               WHERE account_id = (SELECT id FROM accounts WHERE uri = $2)
           )
           RETURNING collection_id"#,
        object_uri,
        actor_uri,
    )
    .fetch_optional(&state.db)
    .await?;
    if let Some(r) = removed_item {
        refresh_collection_item_count(state, r.collection_id).await?;
        return Ok(());
    }

    let account_id = sqlx::query_scalar!("SELECT id FROM accounts WHERE uri = $1", actor_uri)
        .fetch_optional(&state.db)
        .await?;
    let status_id = sqlx::query_scalar!("SELECT id FROM statuses WHERE uri = $1", object_uri)
        .fetch_optional(&state.db)
        .await?;

    if let (Some(aid), Some(sid)) = (account_id, status_id) {
        sqlx::query!(
            "DELETE FROM status_pins WHERE account_id = $1 AND status_id = $2",
            aid,
            sid
        )
        .execute(&state.db)
        .await?;
    }

    Ok(())
}
