//! Mastodon's `Relay`: a relay this server follows, from the instance actor,
//! to have public posts from elsewhere sent to it and its own public posts
//! passed on. The delivery side (enabled relays in `StatusReachFinder` and
//! `AccountReachFinder`) is in [`crate::federation::delivery`]; this is
//! following and unfollowing one, and what its answer does.

use serde_json::json;

use crate::state::AppState;

/// `Relay.state`.
pub mod state {
    pub const IDLE: i32 = 0;
    pub const PENDING: i32 = 1;
    pub const ACCEPTED: i32 = 2;
    pub const REJECTED: i32 = 3;

    pub fn to_str(state: i32) -> &'static str {
        match state {
            PENDING => "pending",
            ACCEPTED => "accepted",
            REJECTED => "rejected",
            _ => "idle",
        }
    }
}

const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// `ActivityPub::TagManager#generate_uri_for(nil)`.
fn payload_uri(state: &AppState) -> String {
    format!(
        "https://{}/payloads/{}",
        state.instance.domain,
        uuid::Uuid::new_v4()
    )
}

/// `DeliveryFailureTracker.reset!(inbox_url)`: the relay's host is delivered
/// to again, whatever it failed before.
async fn reset_delivery_tracker(state: &AppState, inbox_url: &str) -> anyhow::Result<()> {
    if let Some(host) = url::Url::parse(inbox_url)
        .ok()
        .as_ref()
        .and_then(crate::federation::delivery_failures::host)
    {
        state.delivery_failures.restart(&host).await?;
    }
    Ok(())
}

async fn deliver(
    state: &AppState,
    activity: serde_json::Value,
    inbox_url: &str,
) -> anyhow::Result<()> {
    // `Account.representative` signs, and must have keys.
    crate::federation::instance_actor::get_or_create(state).await?;
    let key_id = crate::federation::instance_actor::key_id(&state.instance.domain);
    crate::federation::delivery::deliver_to_inboxes(
        state,
        activity,
        vec![inbox_url.to_owned()],
        key_id,
    )
    .await?;
    Ok(())
}

/// `Relay#enable!`: a `Follow` of the public collection from the instance
/// actor, the relay pending until it answers.
pub async fn enable(state: &AppState, id: i64) -> anyhow::Result<()> {
    let inbox_url: String = sqlx::query_scalar!("SELECT inbox_url FROM relays WHERE id = $1", id)
        .fetch_one(&state.db)
        .await?;
    let activity_id = payload_uri(state);
    sqlx::query!(
        "UPDATE relays SET state = $2, follow_activity_id = $3, updated_at = now() WHERE id = $1",
        id,
        state::PENDING,
        activity_id,
    )
    .execute(&state.db)
    .await?;
    reset_delivery_tracker(state, &inbox_url).await?;
    let actor = crate::federation::instance_actor::actor_url(&state.instance.domain);
    let follow = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": activity_id,
        "type": "Follow",
        "actor": actor,
        "object": PUBLIC,
    });
    deliver(state, follow, &inbox_url).await
}

/// `Relay#disable!`: an `Undo` of the `Follow` it was enabled with.
pub async fn disable(state: &AppState, id: i64) -> anyhow::Result<()> {
    let row = sqlx::query!(
        "SELECT inbox_url, follow_activity_id FROM relays WHERE id = $1",
        id
    )
    .fetch_one(&state.db)
    .await?;
    let activity_id = payload_uri(state);
    sqlx::query!(
        "UPDATE relays SET state = $2, follow_activity_id = NULL, updated_at = now() WHERE id = $1",
        id,
        state::IDLE,
    )
    .execute(&state.db)
    .await?;
    reset_delivery_tracker(state, &row.inbox_url).await?;
    let actor = crate::federation::instance_actor::actor_url(&state.instance.domain);
    let undo = json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": activity_id,
        "type": "Undo",
        "actor": actor,
        "object": {
            "id": row.follow_activity_id,
            "type": "Follow",
            "actor": actor,
            "object": PUBLIC,
        },
    });
    deliver(state, undo, &row.inbox_url).await
}

/// `ActivityPub::Activity::Accept#accept_follow_for_relay` and
/// `Reject#reject_follow_for_relay`: the answer to a relay's `Follow`, found by
/// the `Follow`'s id. Whether it was one.
pub async fn answered(state: &AppState, follow_uri: &str, accepted: bool) -> anyhow::Result<bool> {
    let answered = sqlx::query!(
        "UPDATE relays SET state = $2, updated_at = now() WHERE follow_activity_id = $1",
        follow_uri,
        if accepted {
            state::ACCEPTED
        } else {
            state::REJECTED
        },
    )
    .execute(&state.db)
    .await?;
    Ok(answered.rows_affected() > 0)
}

/// `Relay.find_by(inbox_url:)&.enabled?`: whether an actor with this inbox is
/// an enabled relay.
pub async fn is_enabled_relay_inbox(state: &AppState, inbox_url: &str) -> bool {
    if inbox_url.is_empty() {
        return false;
    }
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM relays WHERE inbox_url = $1 AND state = $2) AS "e!""#,
        inbox_url,
        state::ACCEPTED,
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(false)
}
