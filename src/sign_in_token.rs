//! The security code a user who has not signed in for a long time is asked
//! for after their password, mailed to them, since Mastodon 4.7.4.
//!
//! Mastodon: app/models/concerns/user/sign_in_token.rb

use chrono::{Duration, NaiveDateTime, Utc};
use rand::RngExt as _;

use crate::state::AppState;

/// `SUSPICIOUS_INACTIVITY_DURATION`.
const SUSPICIOUS_INACTIVITY_DAYS: i64 = 90;
/// `SIGN_IN_TOKEN_DISABLED_TTL`.
const SIGN_IN_TOKEN_DISABLED_SECONDS: i64 = 96 * 3600;
/// How long `sign_in_token_expired?` takes a code to be good for.
const VALID_MINUTES: i64 = 5;
/// `Devise.friendly_token(6)`'s length.
const LENGTH: usize = 6;
/// The characters `Devise.friendly_token` draws on: URL-safe Base64, with
/// those easily misread replaced.
const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz123456789-_";

fn disabled_key(state: &AppState, user_id: i64) -> String {
    state
        .redis_keys
        .key(format!("user.{user_id}.sign_in_token_disabled"))
}

/// `sign_in_token_disabled?`. An answer Redis could not give is that it is
/// not: the challenge stays on.
pub async fn disabled(state: &AppState, user_id: i64) -> bool {
    let mut redis = state.redis_coordination.clone();
    redis::cmd("EXISTS")
        .arg(disabled_key(state, user_id))
        .query_async::<bool>(&mut redis)
        .await
        .unwrap_or(false)
}

/// `suspicious_inactive_sign_in?`, for a user without two-factor
/// authentication: one with a password, whose last sign-in was too long ago.
pub async fn suspicious_inactive_sign_in(state: &AppState, user_id: i64) -> bool {
    let inactive = sqlx::query_scalar!(
        r#"SELECT encrypted_password <> ''
                  AND current_sign_in_at IS NOT NULL
                  AND current_sign_in_at < $2
           AS "inactive!"
           FROM users WHERE id = $1"#,
        user_id,
        (Utc::now() - Duration::days(SUSPICIOUS_INACTIVITY_DAYS)).naive_utc(),
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .unwrap_or(false);
    inactive && !disabled(state, user_id).await
}

/// `sign_in_token_expired?`.
fn expired(sent_at: Option<NaiveDateTime>) -> bool {
    sent_at
        .is_none_or(|sent_at| sent_at < (Utc::now() - Duration::minutes(VALID_MINUTES)).naive_utc())
}

/// `sign_in_token_expired?` for a user.
pub async fn is_expired(state: &AppState, user_id: i64) -> anyhow::Result<bool> {
    let sent_at = sqlx::query_scalar!(
        "SELECT sign_in_token_sent_at FROM users WHERE id = $1",
        user_id
    )
    .fetch_one(&state.db)
    .await?;
    Ok(expired(sent_at))
}

/// `generate_sign_in_token && save`: the new code, which replaces any other.
pub async fn generate(state: &AppState, user_id: i64) -> anyhow::Result<String> {
    let token: String = {
        let mut rng = rand::rng();
        (0..LENGTH)
            .map(|_| char::from(ALPHABET[rng.random_range(0..ALPHABET.len())]))
            .collect()
    };
    sqlx::query!(
        "UPDATE users SET sign_in_token = $2, sign_in_token_sent_at = now(), updated_at = now()
         WHERE id = $1",
        user_id,
        token,
    )
    .execute(&state.db)
    .await?;
    Ok(token)
}

/// `valid_sign_in_token?`.
pub async fn valid(state: &AppState, user_id: i64, attempt: &str) -> anyhow::Result<bool> {
    let row = sqlx::query!(
        "SELECT sign_in_token, sign_in_token_sent_at FROM users WHERE id = $1",
        user_id
    )
    .fetch_one(&state.db)
    .await?;
    let Some(token) = row.sign_in_token else {
        return Ok(false);
    };
    Ok(!expired(row.sign_in_token_sent_at)
        && crate::api::mastodon::oauth_client::secure_compare(&token, attempt))
}

/// `clear_sign_in_token!`.
pub async fn clear(state: &AppState, user_id: i64) -> anyhow::Result<()> {
    sqlx::query!(
        "UPDATE users SET sign_in_token = NULL, sign_in_token_sent_at = NULL, updated_at = now()
         WHERE id = $1",
        user_id,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `disable_sign_in_token!`: no challenge for a while, and no code left.
pub async fn disable(state: &AppState, user_id: i64) -> anyhow::Result<()> {
    let mut redis = state.redis_coordination.clone();
    redis::cmd("SET")
        .arg(disabled_key(state, user_id))
        .arg("true")
        .arg("EX")
        .arg(SIGN_IN_TOKEN_DISABLED_SECONDS)
        .query_async::<()>(&mut redis)
        .await?;
    clear(state, user_id).await
}

/// `enable_sign_in_token!`.
pub async fn enable(state: &AppState, user_id: i64) -> anyhow::Result<()> {
    let mut redis = state.redis_coordination.clone();
    redis::cmd("DEL")
        .arg(disabled_key(state, user_id))
        .query_async::<()>(&mut redis)
        .await?;
    Ok(())
}
