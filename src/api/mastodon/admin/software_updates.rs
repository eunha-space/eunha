//! `Admin::SoftwareUpdatesController`: the releases newer than the one
//! implemented, as the update check recorded them, for `view_devops`; a 404
//! when the check is off.

use std::cmp::Ordering;

use axum::{extract::Extension, Json};
use serde::Serialize;

use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::role::flag,
    state::AppState,
};

/// `SoftwareUpdate.check_enabled?`: an update server is configured.
pub(super) fn check_enabled(state: &AppState) -> bool {
    state
        .config
        .software_update_url
        .as_deref()
        .is_some_and(|url| !url.is_empty())
}

/// `Gem::Version#<=>`, near enough for release numbers: numeric segments by
/// value, and a pre-release segment (`beta`) before any number.
pub(super) fn compare_versions(a: &str, b: &str) -> Ordering {
    fn segments(v: &str) -> Vec<Result<u64, String>> {
        v.split(['.', '-'])
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<u64>().map_err(|_| s.to_owned()))
            .collect()
    }
    let (a, b) = (segments(a), segments(b));
    for i in 0..a.len().max(b.len()) {
        let ordering = match (a.get(i), b.get(i)) {
            (Some(Ok(x)), Some(Ok(y))) => x.cmp(y),
            (Some(Err(x)), Some(Err(y))) => x.cmp(y),
            (Some(Err(_)), _) => Ordering::Less,
            (_, Some(Err(_))) => Ordering::Greater,
            (Some(Ok(x)), None) => x.cmp(&0),
            (None, Some(Ok(y))) => 0.cmp(y),
            (None, None) => Ordering::Equal,
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

#[derive(Debug, Serialize)]
pub struct SoftwareUpdate {
    pub version: String,
    /// `patch`, `minor` or `major`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub urgent: bool,
    pub release_notes: String,
    pub end_of_support: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SoftwareUpdates {
    /// The Mastodon release eunha implements, which the updates are newer
    /// than.
    pub current_version: &'static str,
    pub updates: Vec<SoftwareUpdate>,
}

/// `SoftwareUpdate.pending_to_a`, by version.
pub(super) async fn pending(state: &AppState) -> AppResult<Vec<SoftwareUpdate>> {
    if !check_enabled(state) {
        return Ok(vec![]);
    }
    let rows = sqlx::query!(
        "SELECT version, type, urgent, release_notes, end_of_support FROM software_updates"
    )
    .fetch_all(&state.db)
    .await?;
    let mut updates: Vec<SoftwareUpdate> = rows
        .into_iter()
        .filter(|r| compare_versions(&r.version, crate::version::MASTODON) == Ordering::Greater)
        .map(|r| SoftwareUpdate {
            version: r.version,
            kind: match r.r#type {
                2 => "major",
                1 => "minor",
                _ => "patch",
            },
            urgent: r.urgent,
            release_notes: r.release_notes,
            end_of_support: r.end_of_support.map(|d| d.to_string()),
        })
        .collect();
    updates.sort_by(|a, b| compare_versions(&a.version, &b.version));
    Ok(updates)
}

/// `GET /api/v1/admin/software_updates`.
pub async fn list_software_updates(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<SoftwareUpdates>> {
    if !check_enabled(&state) {
        return Err(AppError::NotFound);
    }
    auth.require_scope("admin:read")?;
    super::require_permission(&state, auth.account_id, flag::VIEW_DEVOPS).await?;
    Ok(Json(SoftwareUpdates {
        current_version: crate::version::MASTODON,
        updates: pending(&state).await?,
    }))
}

#[cfg(test)]
mod tests {
    use super::compare_versions;
    use std::cmp::Ordering;

    #[test]
    fn versions_compare_as_gem_versions_do() {
        assert_eq!(compare_versions("4.7.2", "4.7.1"), Ordering::Greater);
        assert_eq!(compare_versions("4.10.0", "4.9.9"), Ordering::Greater);
        assert_eq!(compare_versions("4.8.0-beta.1", "4.8.0"), Ordering::Less);
        assert_eq!(compare_versions("4.8.0-beta.1", "4.7.1"), Ordering::Greater);
        assert_eq!(compare_versions("4.7.1", "4.7.1"), Ordering::Equal);
    }
}
