//! Web sessions, as Mastodon keeps them in `session_activations`.
//!
//! A sign-in on eunha's server-rendered account pages activates a session:
//! a row with a random `session_id`, which the `account_session` cookie
//! carries the way Mastodon's signed `_session_id` cookie does, the browser's
//! address and user agent, and an access token for the instance's web app
//! (the `superapp` application, when there is one) with
//! `SessionActivation::DEFAULT_SCOPES`. Revoking the session deletes the row
//! and its token, which ends the cookie and every request made with it.

use sqlx::PgPool;

/// `SessionActivation::DEFAULT_SCOPES`.
const DEFAULT_SCOPES: &str = "read write follow";
/// `Rails.configuration.x.max_session_activations`, `MAX_SESSION_ACTIVATIONS`'s
/// default.
const MAX_SESSION_ACTIVATIONS: i64 = 10;

/// A live session behind a cookie.
#[derive(Debug, Clone)]
pub struct ActiveSession {
    pub id: i64,
    pub session_id: String,
    pub user_id: i64,
    pub access_token_id: Option<i64>,
}

/// `User#activate_session`: a new session for a browser that just signed in,
/// with its own access token, and the oldest beyond the limit purged.
pub async fn activate(
    db: &PgPool,
    user_id: i64,
    ip: Option<std::net::IpAddr>,
    user_agent: Option<&str>,
) -> sqlx::Result<String> {
    let token = crate::crypto::generate_token(32);
    let access_token_id = sqlx::query_scalar!(
        r#"INSERT INTO oauth_access_tokens (application_id, resource_owner_id, token, scopes, created_at)
           VALUES ((SELECT id FROM oauth_applications WHERE superapp ORDER BY id LIMIT 1),
                   $1, $2, $3, now())
           RETURNING id"#,
        user_id,
        token,
        DEFAULT_SCOPES,
    )
    .fetch_one(db)
    .await?;
    activate_with_token(db, user_id, access_token_id, ip, user_agent).await
}

/// A session for a token that already exists: how `/account/sso` turns the
/// web client's sign-in into one.
pub async fn activate_with_token(
    db: &PgPool,
    user_id: i64,
    access_token_id: i64,
    ip: Option<std::net::IpAddr>,
    user_agent: Option<&str>,
) -> sqlx::Result<String> {
    let session_id = crate::crypto::generate_token(16);
    sqlx::query!(
        r#"INSERT INTO session_activations
             (session_id, user_id, access_token_id, ip, user_agent, created_at, updated_at)
           VALUES ($1, $2, $3, $4::text::inet, $5, now(), now())"#,
        session_id,
        user_id,
        access_token_id,
        ip.map(|ip| ip.to_string()),
        user_agent.unwrap_or_default(),
    )
    .execute(db)
    .await?;
    purge_old(db, user_id).await?;
    Ok(session_id)
}

/// `SessionActivation.purge_old`.
async fn purge_old(db: &PgPool, user_id: i64) -> sqlx::Result<()> {
    let stale: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT id FROM session_activations WHERE user_id = $1
           ORDER BY id DESC OFFSET $2"#,
        user_id,
        MAX_SESSION_ACTIVATIONS,
    )
    .fetch_all(db)
    .await?;
    destroy_where_ids(db, &stale).await.map(drop)
}

/// `Warden::Manager.after_fetch`: the session behind a cookie, if it is
/// still active, with its address brought up to date.
pub async fn fetch(
    db: &PgPool,
    session_id: &str,
    ip: Option<std::net::IpAddr>,
) -> Option<ActiveSession> {
    let row = sqlx::query!(
        r#"SELECT id, session_id, user_id, access_token_id, host(ip) AS ip
           FROM session_activations WHERE session_id = $1"#,
        session_id,
    )
    .fetch_optional(db)
    .await
    .ok()??;
    let current = ip.map(|ip| ip.to_string());
    if current.is_some() && row.ip != current {
        let _ = sqlx::query!(
            "UPDATE session_activations SET ip = $1::text::inet, updated_at = now() WHERE id = $2",
            current,
            row.id,
        )
        .execute(db)
        .await;
    }
    Some(ActiveSession {
        id: row.id,
        session_id: row.session_id,
        user_id: row.user_id,
        access_token_id: row.access_token_id,
    })
}

/// `SessionActivation.deactivate`: sign a browser out. Each of these returns
/// the access tokens it deleted, whose streams [`kill_streams`] closes.
pub async fn deactivate(db: &PgPool, session_id: &str) -> sqlx::Result<Vec<i64>> {
    let ids: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM session_activations WHERE session_id = $1",
        session_id
    )
    .fetch_all(db)
    .await?;
    destroy_where_ids(db, &ids).await
}

/// `session_activations.destroy_all`, every session the user has.
pub async fn destroy_all(db: &PgPool, user_id: i64) -> sqlx::Result<Vec<i64>> {
    let ids: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM session_activations WHERE user_id = $1",
        user_id
    )
    .fetch_all(db)
    .await?;
    destroy_where_ids(db, &ids).await
}

/// `User#clear_other_sessions`: every session but `keep`.
pub async fn destroy_others(
    db: &PgPool,
    user_id: i64,
    keep: Option<i64>,
) -> sqlx::Result<Vec<i64>> {
    let ids: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM session_activations WHERE user_id = $1 AND id IS DISTINCT FROM $2",
        user_id,
        keep,
    )
    .fetch_all(db)
    .await?;
    destroy_where_ids(db, &ids).await
}

/// `SessionActivation#destroy`: the row, its access token and its web push
/// subscription (`dependent: :destroy`).
pub async fn destroy_where_ids(db: &PgPool, ids: &[i64]) -> sqlx::Result<Vec<i64>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut tx = db.begin().await?;
    let gone = sqlx::query!(
        r#"DELETE FROM session_activations WHERE id = ANY($1)
           RETURNING access_token_id, web_push_subscription_id"#,
        ids,
    )
    .fetch_all(&mut *tx)
    .await?;
    let tokens: Vec<i64> = gone.iter().filter_map(|r| r.access_token_id).collect();
    let subscriptions: Vec<i64> = gone
        .iter()
        .filter_map(|r| r.web_push_subscription_id)
        .collect();
    if !subscriptions.is_empty() {
        sqlx::query!(
            "DELETE FROM web_push_subscriptions WHERE id = ANY($1)",
            &subscriptions
        )
        .execute(&mut *tx)
        .await?;
    }
    if !tokens.is_empty() {
        sqlx::query!(
            "DELETE FROM web_push_subscriptions WHERE access_token_id = ANY($1)",
            &tokens
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "DELETE FROM oauth_access_tokens WHERE id = ANY($1)",
            &tokens
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(tokens)
}

/// `Doorkeeper::Application.revoke_tokens_and_grants_for`, with what
/// `OAuth::AuthorizedApplicationsController#destroy` does first: the web push
/// subscriptions of the app's tokens removed and their streams closed.
pub async fn revoke_application(
    state: &crate::state::AppState,
    application_id: i64,
    user_id: i64,
) -> sqlx::Result<()> {
    let mut tx = state.db.begin().await?;
    let tokens: Vec<i64> = sqlx::query_scalar!(
        r#"UPDATE oauth_access_tokens SET revoked_at = now()
           WHERE application_id = $1 AND resource_owner_id = $2 AND revoked_at IS NULL
           RETURNING id"#,
        application_id,
        user_id,
    )
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM web_push_subscriptions WHERE access_token_id = ANY($1)",
        &tokens
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        r#"UPDATE oauth_access_grants SET revoked_at = now()
           WHERE application_id = $1 AND resource_owner_id = $2 AND revoked_at IS NULL"#,
        application_id,
        user_id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    kill_streams(state, tokens).await;
    Ok(())
}

/// `User#revoke_access!`: every grant and token the user has revoked, their
/// web push subscriptions removed and their streams closed.
pub async fn revoke_access(state: &crate::state::AppState, user_id: i64) -> sqlx::Result<()> {
    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "UPDATE oauth_access_grants SET revoked_at = now() WHERE resource_owner_id = $1 AND revoked_at IS NULL",
        user_id,
    )
    .execute(&mut *tx)
    .await?;
    let tokens: Vec<i64> = sqlx::query_scalar!(
        r#"UPDATE oauth_access_tokens SET revoked_at = now()
           WHERE resource_owner_id = $1 AND revoked_at IS NULL
           RETURNING id"#,
        user_id,
    )
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM web_push_subscriptions WHERE access_token_id = ANY($1)",
        &tokens
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    kill_streams(state, tokens).await;
    Ok(())
}

/// `AccessTokenExtension#push_to_streaming_api`.
pub async fn kill_streams(state: &crate::state::AppState, token_ids: Vec<i64>) {
    if !token_ids.is_empty() {
        state.streaming.kill_tokens(&token_ids).await;
    }
}
