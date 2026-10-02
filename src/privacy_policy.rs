//! Mastodon's `PrivacyPolicy`: the `site_terms` setting an administrator
//! writes, or the policy Mastodon ships when that is blank.
//!
//! Between the two eunha reads the instance configuration's `privacy_policy`,
//! which is where it kept the policy before it read the setting; see the
//! `privacy-policy-config-fallback` divergence.

use chrono::{NaiveDate, NaiveDateTime};
use serde::Serialize;

use crate::{
    error::{AppError, AppResult},
    state::AppState,
};

/// `PrivacyPolicy::DEFAULT_PRIVACY_POLICY`, Mastodon's
/// `config/templates/privacy-policy.md`.
pub const DEFAULT_PRIVACY_POLICY: &str = include_str!("templates/privacy-policy.md");

/// `PrivacyPolicy::DEFAULT_UPDATED_AT`.
fn default_updated_at() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2022, 10, 7)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .expect("valid date")
}

pub struct PrivacyPolicy {
    pub text: String,
    pub updated_at: NaiveDateTime,
    /// Whether `updated_at` is the `DateTime` constant, whose `iso8601`
    /// spells UTC `+00:00` where a record's time spells it `Z`.
    pub default_time: bool,
}

/// `PrivacyPolicy.current`.
pub async fn current(state: &AppState) -> AppResult<PrivacyPolicy> {
    let custom = sqlx::query!("SELECT value, updated_at FROM settings WHERE var = 'site_terms'")
        .fetch_optional(&state.db)
        .await?;
    if let Some(row) = custom {
        // `Setting#value` is YAML; `present?` is not blank.
        let text = row
            .value
            .as_deref()
            .and_then(|raw| serde_yaml::from_str::<serde_yaml::Value>(raw).ok())
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        if !text.trim().is_empty() {
            return Ok(PrivacyPolicy {
                text,
                updated_at: row.updated_at.unwrap_or_else(default_updated_at),
                default_time: false,
            });
        }
    }
    let configured = &state.instance.privacy_policy;
    let text = if configured.trim().is_empty() {
        DEFAULT_PRIVACY_POLICY
    } else {
        configured
    };
    Ok(PrivacyPolicy {
        text: text.to_owned(),
        updated_at: default_updated_at(),
        default_time: true,
    })
}

/// `REST::PrivacyPolicySerializer`.
#[derive(Debug, Serialize)]
pub struct Rest {
    pub updated_at: String,
    pub content: String,
}

pub fn serialize(state: &AppState, policy: &PrivacyPolicy) -> AppResult<Rest> {
    Ok(Rest {
        // `updated_at.iso8601`: seconds, no fraction.
        updated_at: policy
            .updated_at
            .format(if policy.default_time {
                "%Y-%m-%dT%H:%M:%S+00:00"
            } else {
                "%Y-%m-%dT%H:%M:%SZ"
            })
            .to_string(),
        content: crate::markdown::render_policy(&policy.text, &state.instance.domain)
            .map_err(AppError::Unrescued)?,
    })
}
