//! A member's automated post deletion, for the settings page:
//! `StatusesCleanupController`, the form Mastodon has no API for, and the
//! validations of `AccountStatusesCleanupPolicy`. [`crate::statuses_cleanup`]
//! carries the policy out.
//!
//! Mastodon: app/controllers/statuses_cleanup_controller.rb
//! Mastodon: app/models/account_statuses_cleanup_policy.rb

use axum::{
    response::{IntoResponse, Response},
    routing::get,
    Extension, Json, Router,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{
    api::{eunha::two_factor::signed_in, mastodon::extractors::Params},
    email_subscriptions::ValidationErrors,
    error::AppResult,
    middleware::AuthenticatedUser,
    state::AppState,
    statuses_cleanup::{self, Policy},
};

pub fn routes() -> Router {
    Router::new().route(
        "/api/eunha/v1/statuses_cleanup",
        get(show).put(update).patch(update),
    )
}

/// `AccountStatusesCleanupPolicy::ALLOWED_MIN_STATUS_AGE`: a week, two weeks,
/// a month, two, three and six months, a year and two, as Rails counts them.
pub const ALLOWED_MIN_STATUS_AGE: [i32; 8] = [
    604_800, 1_209_600, 2_629_746, 5_259_492, 7_889_238, 15_778_476, 31_556_952, 63_113_904,
];

#[derive(Debug, Serialize)]
pub struct PolicyEntity {
    pub enabled: bool,
    pub min_status_age: i32,
    pub keep_direct: bool,
    pub keep_pinned: bool,
    pub keep_polls: bool,
    pub keep_media: bool,
    pub keep_self_fav: bool,
    pub keep_self_bookmark: bool,
    pub min_favs: Option<i32>,
    pub min_reblogs: Option<i32>,
}

impl PolicyEntity {
    /// `build_statuses_cleanup_policy(enabled: false)`: the columns' defaults,
    /// and off, for an account that has never saved one.
    fn unsaved() -> Self {
        Self {
            enabled: false,
            min_status_age: 1_209_600,
            keep_direct: true,
            keep_pinned: true,
            keep_polls: false,
            keep_media: false,
            keep_self_fav: true,
            keep_self_bookmark: true,
            min_favs: None,
            min_reblogs: None,
        }
    }

    fn from_policy(policy: Policy) -> Self {
        Self {
            enabled: policy.enabled,
            min_status_age: policy.min_status_age,
            keep_direct: policy.keep_direct,
            keep_pinned: policy.keep_pinned,
            keep_polls: policy.keep_polls,
            keep_media: policy.keep_media,
            keep_self_fav: policy.keep_self_fav,
            keep_self_bookmark: policy.keep_self_bookmark,
            min_favs: policy.min_favs,
            min_reblogs: policy.min_reblogs,
        }
    }

    /// `update_last_inspected`: whether what the last run decided to keep may
    /// now have to go, so that the next one has to start again. A shorter age
    /// does not, because the mark only passes posts already old enough.
    fn widened_by(&self, new: &Self) -> bool {
        let dropped = |old: bool, new: bool| old && !new;
        let lowered_bar = |old: Option<i32>, new: Option<i32>| match (old, new) {
            (Some(_), None) => true,
            (Some(old), Some(new)) => new > old,
            (None, _) => false,
        };
        dropped(self.keep_direct, new.keep_direct)
            || dropped(self.keep_pinned, new.keep_pinned)
            || dropped(self.keep_polls, new.keep_polls)
            || dropped(self.keep_media, new.keep_media)
            || dropped(self.keep_self_fav, new.keep_self_fav)
            || dropped(self.keep_self_bookmark, new.keep_self_bookmark)
            || lowered_bar(self.min_favs, new.min_favs)
            || lowered_bar(self.min_reblogs, new.min_reblogs)
    }
}

/// A field that was sent, null included, as against one that was not.
fn sent<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize, Default)]
pub struct Update {
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    enabled: Option<bool>,
    #[serde(default, deserialize_with = "sent")]
    min_status_age: Option<Value>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    keep_direct: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    keep_pinned: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    keep_polls: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    keep_media: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    keep_self_fav: Option<bool>,
    #[serde(
        default,
        deserialize_with = "crate::api::mastodon::extractors::rails::opt_bool"
    )]
    keep_self_bookmark: Option<bool>,
    #[serde(default, deserialize_with = "sent")]
    min_favs: Option<Value>,
    #[serde(default, deserialize_with = "sent")]
    min_reblogs: Option<Value>,
}

/// A number as a form or JSON sends it. `Err` when it is not a whole number the
/// column can hold; `Ok(None)` when it is blank.
fn integer(value: &Value) -> Result<Option<i32>, ()> {
    let number = match value {
        Value::Null => return Ok(None),
        Value::Number(number) => number.as_i64().ok_or(())?,
        Value::String(text) if text.trim().is_empty() => return Ok(None),
        Value::String(text) => text.trim().parse().map_err(|_| ())?,
        _ => return Err(()),
    };
    i32::try_from(number).map(Some).map_err(|_| ())
}

/// `validates :min_favs, numericality: { greater_than_or_equal_to: 1, allow_nil: true }`.
fn threshold(
    sent: Option<&Value>,
    current: Option<i32>,
    attribute: &'static str,
    errors: &mut ValidationErrors,
) -> Option<i32> {
    let Some(value) = sent else {
        return current;
    };
    match integer(value) {
        Ok(Some(number)) if number >= 1 => Some(number),
        Ok(None) => None,
        Ok(Some(_)) => {
            errors.add(
                attribute,
                "greater_than_or_equal_to",
                "must be greater than or equal to 1",
            );
            current
        }
        Err(()) => {
            errors.add(attribute, "not_a_number", "is not a number");
            current
        }
    }
}

pub async fn show(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<PolicyEntity>> {
    let (account_id, _) = signed_in(auth, "read:accounts")?;
    let policy = statuses_cleanup::find_policy(&state, account_id).await?;
    Ok(Json(policy.map_or_else(
        PolicyEntity::unsaved,
        PolicyEntity::from_policy,
    )))
}

pub async fn update(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Params(form): Params<Update>,
) -> AppResult<Response> {
    let (account_id, _) = signed_in(auth, "write:accounts")?;
    let existing = statuses_cleanup::find_policy(&state, account_id).await?;
    let saved = existing.is_some();
    let old = existing.map_or_else(PolicyEntity::unsaved, PolicyEntity::from_policy);

    let mut errors = ValidationErrors::default();
    let min_status_age = match form.min_status_age.as_ref().map(integer) {
        None => old.min_status_age,
        Some(Ok(Some(age))) if ALLOWED_MIN_STATUS_AGE.contains(&age) => age,
        Some(_) => {
            errors.add("min_status_age", "inclusion", "is not included in the list");
            old.min_status_age
        }
    };
    let new = PolicyEntity {
        enabled: form.enabled.unwrap_or(old.enabled),
        min_status_age,
        keep_direct: form.keep_direct.unwrap_or(old.keep_direct),
        keep_pinned: form.keep_pinned.unwrap_or(old.keep_pinned),
        keep_polls: form.keep_polls.unwrap_or(old.keep_polls),
        keep_media: form.keep_media.unwrap_or(old.keep_media),
        keep_self_fav: form.keep_self_fav.unwrap_or(old.keep_self_fav),
        keep_self_bookmark: form.keep_self_bookmark.unwrap_or(old.keep_self_bookmark),
        min_favs: threshold(
            form.min_favs.as_ref(),
            old.min_favs,
            "min_favs",
            &mut errors,
        ),
        min_reblogs: threshold(
            form.min_reblogs.as_ref(),
            old.min_reblogs,
            "min_reblogs",
            &mut errors,
        ),
    };
    if !errors.is_empty() {
        return Ok(errors.into_response());
    }

    if saved {
        sqlx::query!(
            "UPDATE account_statuses_cleanup_policies
             SET enabled = $2, min_status_age = $3, keep_direct = $4, keep_pinned = $5,
                 keep_polls = $6, keep_media = $7, keep_self_fav = $8,
                 keep_self_bookmark = $9, min_favs = $10, min_reblogs = $11,
                 updated_at = now()
             WHERE account_id = $1",
            account_id,
            new.enabled,
            new.min_status_age,
            new.keep_direct,
            new.keep_pinned,
            new.keep_polls,
            new.keep_media,
            new.keep_self_fav,
            new.keep_self_bookmark,
            new.min_favs,
            new.min_reblogs,
        )
        .execute(&state.db)
        .await?;
    } else {
        sqlx::query!(
            "INSERT INTO account_statuses_cleanup_policies
               (account_id, enabled, min_status_age, keep_direct, keep_pinned, keep_polls,
                keep_media, keep_self_fav, keep_self_bookmark, min_favs, min_reblogs,
                created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, now(), now())",
            account_id,
            new.enabled,
            new.min_status_age,
            new.keep_direct,
            new.keep_pinned,
            new.keep_polls,
            new.keep_media,
            new.keep_self_fav,
            new.keep_self_bookmark,
            new.min_favs,
            new.min_reblogs,
        )
        .execute(&state.db)
        .await?;
    }
    if old.widened_by(&new) {
        statuses_cleanup::forget_last_inspected(&state, account_id).await;
    }
    Ok(Json(new).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_come_as_forms_and_json_send_them() {
        assert_eq!(integer(&json!(5)), Ok(Some(5)));
        assert_eq!(integer(&json!("5")), Ok(Some(5)));
        assert_eq!(integer(&json!("")), Ok(None));
        assert_eq!(integer(&Value::Null), Ok(None));
        assert_eq!(integer(&json!("many")), Err(()));
        assert_eq!(integer(&json!(1.5)), Err(()));
        assert_eq!(integer(&json!(i64::MAX)), Err(()));
    }

    #[test]
    fn only_a_wider_policy_starts_again() {
        let old = PolicyEntity {
            min_favs: Some(5),
            ..PolicyEntity::unsaved()
        };
        let with = |change: fn(&mut PolicyEntity)| {
            let mut new = PolicyEntity {
                min_favs: Some(5),
                ..PolicyEntity::unsaved()
            };
            change(&mut new);
            old.widened_by(&new)
        };
        assert!(!with(|_| {}));
        assert!(!with(|new| new.keep_media = true));
        assert!(!with(|new| new.min_favs = Some(3)));
        assert!(!with(|new| new.min_reblogs = Some(3)));
        assert!(with(|new| new.keep_pinned = false));
        assert!(with(|new| new.min_favs = None));
        assert!(with(|new| new.min_favs = Some(9)));
    }
}
