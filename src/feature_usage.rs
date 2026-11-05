//! How often a feature is used, by where it was used from: Mastodon's
//! `FeatureUsageTracker`, which so far counts follows by the `ref` their
//! request names.
//!
//! Mastodon: app/lib/feature_usage_tracker.rb

use chrono::{Duration, Utc};

use crate::state::AppState;

/// `REF_VALUES`.
const REF_VALUES: [&str; 10] = [
    "author_attribution",
    "collection",
    "featured_account",
    "hover_card",
    "inline_suggestions",
    "onboarding",
    "profile",
    "search",
    "status",
    "suggestions",
];

/// `EXPIRE_AFTER`: Rails' six months, in seconds.
const EXPIRE_AFTER_SECONDS: i64 = 15_778_476;

/// The field counted for what a request named, `unknown` for anything else.
fn field(reference: Option<&str>) -> &str {
    reference
        .filter(|reference| REF_VALUES.contains(reference))
        .unwrap_or("unknown")
}

/// `FeatureUsageTracker.for(:follow).increment(ref)`. The count is not worth
/// failing the request for.
pub async fn increment_follow(state: &AppState, reference: Option<&str>) {
    let day =
        (Utc::now() - Duration::seconds(Utc::now().timestamp().rem_euclid(86_400))).timestamp();
    let key = state
        .redis_keys
        .key(format!("activity:feature_usage:follow:{day}"));
    let mut redis = state.redis.clone();
    let counted: redis::RedisResult<((), ())> = redis::pipe()
        .cmd("HINCRBY")
        .arg(&key)
        .arg(field(reference))
        .arg(1)
        .ignore()
        .cmd("EXPIRE")
        .arg(&key)
        .arg(EXPIRE_AFTER_SECONDS)
        .ignore()
        .query_async(&mut redis)
        .await;
    if let Err(error) = counted {
        tracing::debug!(%error, "could not count a follow");
    }
}

#[cfg(test)]
mod tests {
    use super::field;

    #[test]
    fn a_reference_not_listed_is_unknown() {
        assert_eq!(field(Some("profile")), "profile");
        assert_eq!(field(Some("elsewhere")), "unknown");
        assert_eq!(field(None), "unknown");
    }
}
