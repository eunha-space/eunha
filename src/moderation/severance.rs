//! `RelationshipSeveranceEvent`: the follows a domain block cut, kept so the
//! local accounts that lost them are told how many, and can see which.

use serde_json::{json, Value};
use sqlx::PgPool;

use crate::state::AppState;

/// `RelationshipSeveranceEvent#type`.
pub mod kind {
    pub const DOMAIN_BLOCK: i32 = 0;
    pub const USER_DOMAIN_BLOCK: i32 = 1;
    pub const ACCOUNT_SUSPENSION: i32 = 2;

    pub fn to_str(v: i32) -> &'static str {
        match v {
            USER_DOMAIN_BLOCK => "user_domain_block",
            ACCOUNT_SUSPENSION => "account_suspension",
            _ => "domain_block",
        }
    }
}

/// `SeveredRelationship#direction`, from the local account's side.
const PASSIVE: i32 = 0;
const ACTIVE: i32 = 1;

/// `RelationshipSeveranceEvent.create!(type:, target_name:)`.
pub async fn create(db: &PgPool, kind: i32, target_name: &str) -> sqlx::Result<i64> {
    sqlx::query_scalar!(
        r#"INSERT INTO relationship_severance_events (type, target_name, purged, created_at, updated_at)
           VALUES ($1, $2, false, now(), now()) RETURNING id"#,
        kind,
        target_name,
    )
    .fetch_one(db)
    .await
}

/// `DeleteAccountService#record_severed_relationships!` for a remote account:
/// its follows of local accounts are passive for them, and local accounts'
/// follows of it are active.
pub async fn record_follows_of(db: &PgPool, event_id: i64, account_id: i64) -> sqlx::Result<()> {
    sqlx::query!(
        r#"INSERT INTO severed_relationships
             (relationship_severance_event_id, local_account_id, remote_account_id, direction,
              show_reblogs, notify, languages, created_at, updated_at)
           SELECT $1::bigint, f.target_account_id, f.account_id, $3::int, f.show_reblogs, f.notify, f.languages, now(), now()
           FROM follows f WHERE f.account_id = $2
           UNION ALL
           SELECT $1::bigint, f.account_id, f.target_account_id, $4::int, f.show_reblogs, f.notify, f.languages, now(), now()
           FROM follows f WHERE f.target_account_id = $2"#,
        event_id,
        account_id,
        PASSIVE,
        ACTIVE,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `RelationshipSeveranceEvent#import_from_*_follows!` for follows between
/// local accounts and accounts on `domain` (a user domain block, whose
/// follows are the user's own).
pub async fn record_follows_with_domain(
    db: &PgPool,
    event_id: i64,
    local_account_id: i64,
    domain: &str,
) -> sqlx::Result<()> {
    sqlx::query!(
        r#"INSERT INTO severed_relationships
             (relationship_severance_event_id, local_account_id, remote_account_id, direction,
              show_reblogs, notify, languages, created_at, updated_at)
           SELECT $1::bigint, f.account_id, f.target_account_id, $4::int, f.show_reblogs, f.notify, f.languages, now(), now()
           FROM follows f JOIN accounts a ON a.id = f.target_account_id
           WHERE f.account_id = $2 AND a.domain = $3
           UNION ALL
           SELECT $1::bigint, f.target_account_id, f.account_id, $5::int, f.show_reblogs, f.notify, f.languages, now(), now()
           FROM follows f JOIN accounts a ON a.id = f.account_id
           WHERE f.target_account_id = $2 AND a.domain = $3"#,
        event_id,
        local_account_id,
        domain,
        ACTIVE,
        PASSIVE,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// `notify_of_severed_relationships!`: each local account that lost follows
/// gets an `AccountRelationshipSeveranceEvent` with its counts and a
/// `severed_relationships` notification.
pub async fn notify_affected(state: &AppState, event_id: i64) -> anyhow::Result<()> {
    let affected: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT DISTINCT local_account_id FROM severed_relationships
           WHERE relationship_severance_event_id = $1 ORDER BY local_account_id"#,
        event_id,
    )
    .fetch_all(&state.db)
    .await?;
    for account_id in affected {
        let id = sqlx::query_scalar!(
            r#"INSERT INTO account_relationship_severance_events
                 (account_id, relationship_severance_event_id, followers_count, following_count,
                  created_at, updated_at)
               SELECT $1, $2,
                      count(*) FILTER (WHERE direction = $3),
                      count(*) FILTER (WHERE direction = $4),
                      now(), now()
               FROM severed_relationships
               WHERE relationship_severance_event_id = $2 AND local_account_id = $1
               RETURNING id"#,
            account_id,
            event_id,
            PASSIVE,
            ACTIVE,
        )
        .fetch_one(&state.db)
        .await?;
        crate::push::notify_local(
            state,
            account_id,
            "severed_relationships",
            "AccountRelationshipSeveranceEvent",
            id,
            account_id,
        )
        .await;
    }
    Ok(())
}

/// `REST::AccountRelationshipSeveranceEventSerializer`.
pub async fn serialize(state: &AppState, id: i64) -> Option<Value> {
    let row = sqlx::query!(
        r#"SELECT ae.id, e.type AS kind, e.purged, e.target_name, ae.followers_count,
                  ae.following_count, ae.created_at
           FROM account_relationship_severance_events ae
           JOIN relationship_severance_events e ON e.id = ae.relationship_severance_event_id
           WHERE ae.id = $1"#,
        id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()?;
    Some(json!({
        "id": row.id.to_string(),
        "type": kind::to_str(row.kind),
        "purged": row.purged,
        "target_name": row.target_name,
        "followers_count": row.followers_count,
        "following_count": row.following_count,
        "created_at": crate::api::mastodon::convert::mastodon_date(row.created_at),
    }))
}
