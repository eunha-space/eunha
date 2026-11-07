//! Leased control-plane decisions. Federation and existing content are untouched.
//!
//! Mastodon: none
use crate::{
    config::LimitsConfig,
    error::{AppError, AppResult},
};
use std::sync::RwLock;

#[derive(Clone, Copy)]
struct Decision {
    registrations: bool,
    media_uploads: bool,
    valid_until: Option<i64>,
}
pub struct GrowthLimits(RwLock<Decision>);
impl GrowthLimits {
    pub fn new(config: &LimitsConfig) -> Self {
        Self(RwLock::new(Decision::from(config)))
    }
    pub fn replace(&self, config: &LimitsConfig) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Decision::from(config);
    }
    fn allowed(&self, media: bool) -> bool {
        let decision = *self.0.read().unwrap_or_else(|e| e.into_inner());
        let now = chrono::Utc::now().timestamp();
        decision.valid_until.is_some_and(|until| until <= now)
            || if media {
                decision.media_uploads
            } else {
                decision.registrations
            }
    }
    pub fn registrations(&self) -> bool {
        self.allowed(false)
    }
    pub fn require_media(&self) -> AppResult<()> {
        if self.allowed(true) {
            Ok(())
        } else {
            Err(AppError::Unprocessable("This instance is over its media allowance. Its owner can review usage or upgrade the plan on eunha space.".into()))
        }
    }
}
impl From<&LimitsConfig> for Decision {
    fn from(config: &LimitsConfig) -> Self {
        Self {
            registrations: config.registrations.unwrap_or(true),
            media_uploads: config.media_uploads.unwrap_or(true),
            valid_until: config.valid_until,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_expiry_and_live_changes_preserve_availability() {
        let limits = GrowthLimits::new(&LimitsConfig::default());
        assert!(limits.registrations());
        assert!(limits.require_media().is_ok());
        let mut config = LimitsConfig {
            registrations: Some(false),
            media_uploads: Some(false),
            valid_until: Some(chrono::Utc::now().timestamp() + 60),
            ..Default::default()
        };
        limits.replace(&config);
        assert!(!limits.registrations());
        assert!(limits.require_media().is_err());
        config.valid_until = Some(chrono::Utc::now().timestamp() - 1);
        limits.replace(&config);
        assert!(limits.registrations());
        assert!(limits.require_media().is_ok());
    }
}
