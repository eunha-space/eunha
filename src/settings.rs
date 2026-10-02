//! Mastodon's `Setting`: site settings an administrator changes, kept in the
//! `settings` table as YAML, with `config/settings.yml`'s defaults for the
//! ones never set.
//!
//! Eunha's instance configuration file carries some of what Mastodon keeps
//! here (the title, descriptions, registrations); those are read from the
//! configuration. This reads the rest.

use serde_yaml::Value;

use crate::state::AppState;

/// `config/settings.yml`'s default for `var`.
fn default(var: &str) -> Value {
    match var {
        "show_domain_blocks" | "show_domain_blocks_rationale" => Value::from("disabled"),
        "trends"
        | "peers_api_enabled"
        | "activity_api_enabled"
        | "profile_directory"
        | "show_staff_badge"
        | "wrapstodon" => Value::from(true),
        "trendable_by_default"
        | "require_invite_text"
        | "noindex"
        | "captcha_enabled"
        | "preview_sensitive_media"
        | "allow_referrer_origin" => Value::from(false),
        "local_live_feed_access"
        | "remote_live_feed_access"
        | "local_topic_feed_access"
        | "remote_topic_feed_access" => Value::from("public"),
        "landing_page" => Value::from("trends"),
        "backups_retention_period" => Value::from(7),
        _ => Value::Null,
    }
}

/// `Setting[var]`.
pub async fn get(state: &AppState, var: &str) -> Value {
    let stored: Option<Option<String>> =
        sqlx::query_scalar!("SELECT value FROM settings WHERE var = $1", var)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten();
    match stored {
        // `Setting#value`: YAML-decoded when present, nil when blank.
        Some(Some(raw)) if !raw.trim().is_empty() => {
            serde_yaml::from_str(&raw).unwrap_or_else(|_| default(var))
        }
        Some(_) => Value::Null,
        None => default(var),
    }
}

/// [`get`] as a string, `""` for anything else.
pub async fn string(state: &AppState, var: &str) -> String {
    match get(state, var).await {
        Value::String(s) => s,
        _ => String::new(),
    }
}

/// `Setting[var] = value`: stored YAML-encoded, the way `Setting#value=`
/// writes it (`--- true`).
pub async fn set(state: &AppState, var: &str, value: Value) -> anyhow::Result<()> {
    let yaml = format!("--- {}", serde_yaml::to_string(&value)?);
    sqlx::query!(
        r#"INSERT INTO settings (var, value, created_at, updated_at)
           VALUES ($1, $2, now(), now())
           ON CONFLICT (var) DO UPDATE SET value = EXCLUDED.value, updated_at = now()"#,
        var,
        yaml,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `AuthorizedFetchHelper#authorized_fetch_mode?`: whether ActivityPub
/// fetches must be signed. The instance configuration's `authorized_fetch`
/// stands where Mastodon reads `AUTHORIZED_FETCH` from its environment, and
/// limited federation mode forces it on. `Setting.authorized_fetch` has no
/// default in `config/settings.yml`, so it is off until an administrator
/// turns it on.
pub async fn authorized_fetch_mode(state: &AppState) -> bool {
    let instance = &state.instance;
    if instance.limited_federation_mode {
        return true;
    }
    match instance.authorized_fetch {
        Some(configured) => configured,
        None => boolean(state, "authorized_fetch").await,
    }
}

/// [`get`] as a boolean, Ruby-truthy: nil and false are false.
pub async fn boolean(state: &AppState, var: &str) -> bool {
    match get(state, var).await {
        Value::Bool(b) => b,
        Value::Null => false,
        _ => true,
    }
}
