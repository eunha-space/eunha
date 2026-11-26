//! Mastodon's `Relay`: a relay this server follows, from the instance actor,
//! to have its own public posts passed on, and, for a bidirectional one, public
//! posts from elsewhere sent to it. A relay that `requires_opt_in` takes only
//! the posts of members who opted in (`RelayPreference`). The delivery side,
//! `Relay.enabled.for_account` in `StatusReachFinder` and `AccountReachFinder`,
//! is in [`crate::federation::delivery`]: an enabled relay that takes everyone's
//! posts, or one the author opted in to. This is following and unfollowing one,
//! what its answer does, and members' preferences.
//!
//! Mastodon: app/models/relay.rb, app/models/relay_preference.rb
//! Mastodon: app/lib/activitypub/activity/accept.rb
//! Mastodon: app/lib/activitypub/activity/reject.rb
//! Mastodon: app/serializers/activitypub/relay/follow_serializer.rb
//! Mastodon: app/serializers/activitypub/relay/undo_follow_serializer.rb
//! Mastodon: app/models/form/relay_preferences_batch.rb

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

/// `Relay.direction`.
pub mod direction {
    /// Posts go to the relay, and it sends others' to this server.
    pub const BIDIRECTIONAL: i32 = 0;
    /// Posts go to the relay; nothing it sends is taken as relayed.
    pub const BROADCAST: i32 = 1;

    pub fn to_str(direction: i32) -> &'static str {
        if direction == BROADCAST {
            "broadcast"
        } else {
            "bidirectional"
        }
    }

    pub fn parse(direction: &str) -> Option<i32> {
        match direction {
            "bidirectional" => Some(BIDIRECTIONAL),
            "broadcast" => Some(BROADCAST),
            _ => None,
        }
    }
}

const PUBLIC: &str = "https://www.w3.org/ns/activitystreams#Public";

/// `ActivityPub::TagManager#generate_uri_for(nil)`.
fn payload_uri(state: &AppState) -> String {
    crate::federation::relationships::generate_uri(state.instance.web_domain())
}

/// `DeliveryFailureTracker.reset!(inbox_url)`: the relay's host is delivered
/// to again, whatever it failed before.
async fn reset_delivery_tracker(state: &AppState, inbox_url: &str) -> anyhow::Result<()> {
    if let Some(host) = ojak::origin::host_of(inbox_url) {
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
    let key_id = crate::federation::instance_actor::key_id(state.instance.web_domain());
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
    let actor = crate::federation::instance_actor::actor_url(state.instance.web_domain());
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
    let actor = crate::federation::instance_actor::actor_url(state.instance.web_domain());
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

/// `Relay.may_forward_to_mastodon.find_by(account:)&.enabled?`: whether this
/// account is an enabled bidirectional relay's actor, whose posts are relayed.
pub async fn forwards_to_us(state: &AppState, account_id: i64) -> bool {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM relays WHERE account_id = $1 AND direction = $2 AND state = $3
           ) AS "e!""#,
        account_id,
        direction::BIDIRECTIONAL,
        state::ACCEPTED,
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(false)
}

/// What `BlockService` adds: blocking a relay's actor records the blocker as
/// opted out of it, in the same transaction as the block.
pub async fn opt_out_on_block(
    conn: &mut sqlx::PgConnection,
    account_id: i64,
    target_account_id: i64,
) -> sqlx::Result<()> {
    sqlx::query!(
        r#"INSERT INTO relay_preferences (account_id, relay_id, enabled, created_at, updated_at)
           SELECT $1, id, false, now(), now() FROM relays WHERE account_id = $2
           ORDER BY id LIMIT 1
           ON CONFLICT (account_id, relay_id)
           DO UPDATE SET enabled = false, updated_at = now()"#,
        account_id,
        target_account_id,
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// A relay a member may opt in to, as `Settings::RelayPreferencesController`
/// lists them: enabled, and taking only the posts of members who opt in.
#[derive(Debug, serde::Serialize)]
pub struct OptInRelay {
    pub id: String,
    pub uri: String,
    pub display_name: String,
    /// The relay actor's bio, as sanitized HTML.
    pub note: String,
    /// Whether this member has opted in.
    pub enabled: bool,
}

/// `Relay.enabled.where(requires_opt_in: true)`, with whether `account_id` is
/// opted in to each (`RelayPreference.where(account:, enabled: true)`).
pub async fn opt_in_relays(state: &AppState, account_id: i64) -> sqlx::Result<Vec<OptInRelay>> {
    let mut relays = sqlx::query_as!(
        OptInRelay,
        r#"SELECT r.id::text AS "id!", r.uri, a.display_name, a.note,
                  EXISTS (SELECT 1 FROM relay_preferences p
                          WHERE p.relay_id = r.id AND p.account_id = $1 AND p.enabled) AS "enabled!"
           FROM relays r JOIN accounts a ON a.id = r.account_id
           WHERE r.state = $2 AND r.requires_opt_in
           ORDER BY r.id"#,
        account_id,
        state::ACCEPTED,
    )
    .fetch_all(&state.db)
    .await?;
    // `account_bio_format(relay.account)`: a remote actor's bio, sanitized.
    for relay in &mut relays {
        relay.note =
            crate::formatter::html_aware(&relay.note, false, &state.urls.formatter_options());
    }
    Ok(relays)
}

/// `Form::RelayPreferencesBatch#save`: the relays among `relay_ids` this
/// member is opted in to, and no others. Opting out of a relay blocks its
/// actor, and opting back in unblocks it, as `BlockService` and
/// `UnblockService`; a relay never opted out of before is only recorded.
pub async fn save_preferences(
    state: &AppState,
    account_id: i64,
    relay_ids: &[i64],
) -> crate::error::AppResult<()> {
    // Opt out of the relays opted in to before.
    let leaving = sqlx::query!(
        r#"SELECT p.id, r.account_id FROM relay_preferences p JOIN relays r ON r.id = p.relay_id
           WHERE p.account_id = $1 AND p.enabled AND NOT (p.relay_id = ANY($2))"#,
        account_id,
        relay_ids,
    )
    .fetch_all(&state.db)
    .await?;
    for preference in leaving {
        crate::api::mastodon::accounts::block(state, account_id, preference.account_id).await?;
        sqlx::query!(
            "UPDATE relay_preferences SET enabled = false, updated_at = now() WHERE id = $1",
            preference.id,
        )
        .execute(&state.db)
        .await?;
    }
    // Opt back in to those opted out of.
    let returning = sqlx::query!(
        r#"SELECT p.id, r.account_id FROM relay_preferences p JOIN relays r ON r.id = p.relay_id
           WHERE p.account_id = $1 AND NOT p.enabled AND p.relay_id = ANY($2)"#,
        account_id,
        relay_ids,
    )
    .fetch_all(&state.db)
    .await?;
    for preference in returning {
        crate::api::mastodon::accounts::unblock(state, account_id, preference.account_id).await?;
        sqlx::query!(
            "UPDATE relay_preferences SET enabled = true, updated_at = now() WHERE id = $1",
            preference.id,
        )
        .execute(&state.db)
        .await?;
    }
    // And the rest. `upsert_all` names relays without checking them, so an id
    // no relay has fails on its foreign key; this skips it instead.
    sqlx::query!(
        r#"INSERT INTO relay_preferences (account_id, relay_id, enabled, created_at, updated_at)
           SELECT $1, r.id, true, now(), now() FROM relays r WHERE r.id = ANY($2)
           ON CONFLICT (account_id, relay_id) DO UPDATE SET enabled = true, updated_at = now()"#,
        account_id,
        relay_ids,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}
