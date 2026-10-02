//! Mastodon's `Setting`: site settings an administrator changes, kept in the
//! `settings` table as YAML, with `config/settings.yml`'s defaults for the
//! ones never set.
//!
//! Eunha's instance configuration file carries some of what Mastodon keeps
//! here: the title, the descriptions, the contact address and whether
//! registrations are open. For those the configuration stands where
//! `config/settings.yml` stands in Mastodon: it is the default, and a value an
//! administrator saved in the `settings` table wins over it, as a saved value
//! wins over the YAML default upstream (docs/operating/administration.md).

use std::collections::HashMap;

use serde_yaml::Value;

use crate::config::InstanceConfig;
use crate::state::AppState;

/// `config/settings.yml`'s default for `var`.
pub fn default(var: &str) -> Value {
    match var {
        "site_title" => Value::from("Mastodon"),
        "site_short_description"
        | "site_description"
        | "site_extended_description"
        | "site_terms"
        | "site_contact_username"
        | "site_contact_email"
        | "closed_registrations_message"
        | "thumbnail_description"
        | "bootstrap_timeline_accounts" => Value::from(""),
        "registrations_mode" => Value::from("none"),
        "theme" => Value::from("default"),
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

/// `Setting#value` of a stored row: YAML-decoded when present, nil when
/// blank.
fn decode(var: &str, raw: Option<&str>) -> Value {
    match raw {
        Some(raw) if !raw.trim().is_empty() => {
            serde_yaml::from_str(raw).unwrap_or_else(|_| default(var))
        }
        _ => Value::Null,
    }
}

fn as_string(value: Value) -> String {
    match value {
        Value::String(s) => s,
        _ => String::new(),
    }
}

/// Ruby truthiness: nil and false are false.
fn as_boolean(value: Value) -> bool {
    match value {
        Value::Bool(b) => b,
        Value::Null => false,
        _ => true,
    }
}

/// `Setting[var]`.
pub async fn get(state: &AppState, var: &str) -> Value {
    get_in(&state.db, var).await
}

/// [`get`], from a pool rather than the whole state.
pub async fn get_in(db: &sqlx::PgPool, var: &str) -> Value {
    let stored: Option<Option<String>> =
        sqlx::query_scalar!("SELECT value FROM settings WHERE var = $1", var)
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    match stored {
        Some(raw) => decode(var, raw.as_deref()),
        None => default(var),
    }
}

/// [`get`] as a string, `""` for anything else.
pub async fn string(state: &AppState, var: &str) -> String {
    as_string(get(state, var).await)
}

/// `Setting[var] = value`: stored YAML-encoded, the way `Setting#value=`
/// writes it (`--- true`).
pub async fn set(state: &AppState, var: &str, value: Value) -> anyhow::Result<()> {
    set_in(&state.db, var, value).await
}

/// [`set`] on any executor, so several settings can be saved in one
/// transaction.
pub async fn set_in<'e>(
    db: impl sqlx::PgExecutor<'e>,
    var: &str,
    value: Value,
) -> anyhow::Result<()> {
    let yaml = format!("--- {}", serde_yaml::to_string(&value)?);
    sqlx::query!(
        r#"INSERT INTO settings (var, value, created_at, updated_at)
           VALUES ($1, $2, now(), now())
           ON CONFLICT (var) DO UPDATE SET value = EXCLUDED.value, updated_at = now()"#,
        var,
        yaml,
    )
    .execute(db)
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
    match authorized_fetch_override(&state.instance) {
        Some(forced) => forced,
        None => boolean(state, "authorized_fetch").await,
    }
}

/// What the instance configuration decides about authorized fetch whatever
/// the setting says, if anything: Mastodon's form shows the setting disabled
/// in that case.
pub fn authorized_fetch_override(instance: &InstanceConfig) -> Option<bool> {
    if instance.limited_federation_mode {
        Some(true)
    } else {
        instance.authorized_fetch
    }
}

/// [`get`] as a boolean, Ruby-truthy: nil and false are false.
pub async fn boolean(state: &AppState, var: &str) -> bool {
    as_boolean(get(state, var).await)
}

/// `Setting.registrations_mode`, one of `Form::AdminSettings::REGISTRATION_MODES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationsMode {
    Open,
    Approved,
    None,
}

impl RegistrationsMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Approved => "approved",
            Self::None => "none",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "approved" => Some(Self::Approved),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// What the instance configuration says, standing in for
    /// `config/settings.yml`'s default.
    pub fn from_config(instance: &InstanceConfig) -> Self {
        if !instance.registrations_open {
            Self::None
        } else if instance.approval_required {
            Self::Approved
        } else {
            Self::Open
        }
    }

    /// `registrations_mode != 'none'`.
    pub fn enabled(self) -> bool {
        self != Self::None
    }

    /// `registrations_mode == 'approved'`.
    pub fn approval_required(self) -> bool {
        self == Self::Approved
    }

    /// `registrations_mode == 'open'`.
    pub fn open(self) -> bool {
        self == Self::Open
    }
}

/// Every stored setting, read in one query, for a request that reads several.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    rows: HashMap<String, Option<String>>,
}

impl Snapshot {
    pub async fn load(state: &AppState) -> Self {
        let rows = sqlx::query!("SELECT var, value FROM settings")
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();
        Self {
            rows: rows.into_iter().map(|r| (r.var, r.value)).collect(),
        }
    }

    /// Whether an administrator has saved `var`.
    pub fn stored(&self, var: &str) -> bool {
        self.rows.contains_key(var)
    }

    /// `Setting[var]`.
    pub fn get(&self, var: &str) -> Value {
        match self.rows.get(var) {
            Some(raw) => decode(var, raw.as_deref()),
            None => default(var),
        }
    }

    pub fn string(&self, var: &str) -> String {
        as_string(self.get(var))
    }

    pub fn boolean(&self, var: &str) -> bool {
        as_boolean(self.get(var))
    }

    /// `Setting[var]` as an integer, when it is one.
    pub fn integer(&self, var: &str) -> Option<i64> {
        self.get(var).as_i64()
    }

    /// A text setting the instance configuration provides the default for.
    fn configured(&self, var: &str, configured: &str) -> String {
        if self.stored(var) {
            self.string(var)
        } else {
            configured.to_owned()
        }
    }

    /// `Setting.site_title`, the configured `title` until one is saved.
    pub fn site_title(&self, instance: &InstanceConfig) -> String {
        self.configured("site_title", &instance.title)
    }

    /// `Setting.site_short_description`, the configured `short_description`
    /// (or `description`, when that is all there is) until one is saved.
    pub fn site_short_description(&self, instance: &InstanceConfig) -> String {
        let configured = if instance.short_description.is_empty() {
            &instance.description
        } else {
            &instance.short_description
        };
        self.configured("site_short_description", configured)
    }

    /// `Setting.site_extended_description`, the configured `description`
    /// until one is saved.
    pub fn site_extended_description(&self, instance: &InstanceConfig) -> String {
        self.configured("site_extended_description", &instance.description)
    }

    /// `Setting.site_description`, the legacy text `/api/v1/instance` still
    /// serves as `description`; the configured `description` until saved.
    pub fn site_description(&self, instance: &InstanceConfig) -> String {
        self.configured("site_description", &instance.description)
    }

    /// `Setting.site_contact_email`, the configured `contact_email` until one
    /// is saved.
    pub fn site_contact_email(&self, instance: &InstanceConfig) -> String {
        self.configured(
            "site_contact_email",
            instance.contact_email.as_deref().unwrap_or(""),
        )
    }

    /// The instance configuration as the saved settings amend it, for the
    /// pages written against the configuration: the title, descriptions,
    /// contact address and registrations are the settings'.
    pub fn amend(&self, instance: &InstanceConfig) -> InstanceConfig {
        let mode = self.registrations_mode(instance);
        let contact_email = self.site_contact_email(instance);
        InstanceConfig {
            title: self.site_title(instance),
            short_description: self.site_short_description(instance),
            description: self.site_extended_description(instance),
            contact_email: (!contact_email.is_empty()).then_some(contact_email),
            registrations_open: mode.enabled(),
            approval_required: mode.approval_required(),
            ..instance.clone()
        }
    }

    /// `Setting.registrations_mode`, what the configuration says until a mode
    /// is saved. A saved value Mastodon would not accept reads as the
    /// configured mode.
    pub fn registrations_mode(&self, instance: &InstanceConfig) -> RegistrationsMode {
        if self.stored("registrations_mode") {
            if let Some(mode) = RegistrationsMode::parse(&self.string("registrations_mode")) {
                return mode;
            }
        }
        RegistrationsMode::from_config(instance)
    }
}

/// [`Snapshot::registrations_mode`] for a caller that reads nothing else.
pub async fn registrations_mode(state: &AppState) -> RegistrationsMode {
    registrations_mode_in(&state.db, &state.instance).await
}

/// [`registrations_mode`] for a command with a database and a configuration.
pub async fn registrations_mode_in(
    db: &sqlx::PgPool,
    instance: &InstanceConfig,
) -> RegistrationsMode {
    let stored: Option<Option<String>> =
        sqlx::query_scalar!("SELECT value FROM settings WHERE var = 'registrations_mode'")
            .fetch_optional(db)
            .await
            .ok()
            .flatten();
    let snapshot = Snapshot {
        rows: stored
            .map(|raw| ("registrations_mode".to_owned(), raw))
            .into_iter()
            .collect(),
    };
    snapshot.registrations_mode(instance)
}

/// [`Snapshot::site_title`] for a caller that reads nothing else.
pub async fn site_title(state: &AppState) -> String {
    Snapshot::load(state).await.site_title(&state.instance)
}

/// [`Snapshot::site_contact_email`] for a caller that reads nothing else.
pub async fn site_contact_email(state: &AppState) -> String {
    Snapshot::load(state)
        .await
        .site_contact_email(&state.instance)
}

/// `Setting.min_age.presence`, as a number of years: the age a new account
/// must be to sign up, if the instance asks. The admin form stores it as an
/// integer; a string of digits reads the same, and anything blank is unset.
pub async fn min_age(db: &sqlx::PgPool) -> Option<u32> {
    match get_in(db, "min_age").await {
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_saved_blank_is_nil_and_a_missing_row_the_default() {
        let snapshot = Snapshot {
            rows: [("site_title".to_owned(), Some(String::new()))]
                .into_iter()
                .collect(),
        };
        assert_eq!(snapshot.get("site_title"), Value::Null);
        assert_eq!(snapshot.get("theme"), Value::from("default"));
        assert!(snapshot.boolean("trends"));
    }

    #[test]
    fn values_decode_as_yaml() {
        let snapshot = Snapshot {
            rows: [
                ("trends".to_owned(), Some("--- false\n".to_owned())),
                (
                    "media_cache_retention_period".to_owned(),
                    Some("--- 14\n".to_owned()),
                ),
            ]
            .into_iter()
            .collect(),
        };
        assert!(!snapshot.boolean("trends"));
        assert_eq!(snapshot.integer("media_cache_retention_period"), Some(14));
    }
}
