use axum::{extract::Extension, http::Uri, Json};
use std::collections::HashMap;

use super::types::MarkerInfo;
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

// ── GET /api/v1/markers ───────────────────────────────────────────────────

pub async fn get_markers(
    state: AppState,
    uri: Uri,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<HashMap<String, MarkerInfo>>> {
    auth.require_scope("read:statuses")?;
    let user_id = auth.user_id.ok_or(AppError::Unauthorized)?;
    let query = uri.query().unwrap_or("");
    let timelines: Vec<String> = query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            if k == "timeline%5B%5D" || k == "timeline[]" {
                urlencoding::decode(v).ok().map(|s| s.into_owned())
            } else {
                None
            }
        })
        .collect();

    let mut result = HashMap::new();

    for timeline in &timelines {
        let row = sqlx::query!(
            "SELECT last_read_id, lock_version, updated_at FROM markers WHERE user_id = $1 AND timeline = $2",
            user_id, timeline.as_str()
        )
        .fetch_optional(&state.db)
        .await?;

        if let Some(r) = row {
            result.insert(
                timeline.clone(),
                MarkerInfo {
                    last_read_id: r.last_read_id.to_string(),
                    version: r.lock_version,
                    updated_at: super::convert::mastodon_date(r.updated_at),
                },
            );
        }
    }

    Ok(Json(result))
}

// ── POST /api/v1/markers ──────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize)]
struct MarkerPosition {
    /// Held as a `Value` because clients send the id both quoted and bare, and
    /// Rails does not mind which.
    last_read_id: Option<serde_json::Value>,
}

impl MarkerPosition {
    fn id(self) -> Option<String> {
        match self.last_read_id? {
            serde_json::Value::String(s) => Some(s),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }
}

pub async fn set_markers(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    super::extractors::NestedParams(params): super::extractors::NestedParams,
) -> AppResult<Json<HashMap<String, MarkerInfo>>> {
    auth.require_scope("write:statuses")?;
    let user_id = auth.user_id.ok_or(AppError::Unauthorized)?;

    // Rails parses either transparently, and clients send both: the bracket
    // notation of a form post, or the nested object of a JSON one.
    let position = |timeline: &str| {
        params
            .get(timeline)
            .cloned()
            .and_then(|v| serde_json::from_value::<MarkerPosition>(v).ok())
            .and_then(MarkerPosition::id)
    };
    let (home_id, notif_id) = (position("home"), position("notifications"));

    let mut result = HashMap::new();

    for (timeline, last_read_id) in [("home", home_id), ("notifications", notif_id)] {
        let Some(id) = last_read_id else { continue };
        let id_int: i64 = id.parse().unwrap_or(0);

        sqlx::query!(
            r#"INSERT INTO markers (user_id, timeline, last_read_id, lock_version, updated_at, created_at)
               VALUES ($1, $2, $3, 1, now(), now())
               ON CONFLICT (user_id, timeline) DO UPDATE
                 SET last_read_id = EXCLUDED.last_read_id,
                     lock_version = markers.lock_version + 1,
                     updated_at = now()"#,
            user_id, timeline, id_int
        )
        .execute(&state.db)
        .await?;

        let row = sqlx::query!(
            "SELECT last_read_id, lock_version, updated_at FROM markers WHERE user_id = $1 AND timeline = $2",
            user_id, timeline
        )
        .fetch_one(&state.db)
        .await?;

        result.insert(
            timeline.to_string(),
            MarkerInfo {
                last_read_id: row.last_read_id.to_string(),
                version: row.lock_version,
                updated_at: super::convert::mastodon_date(row.updated_at),
            },
        );
    }

    Ok(Json(result))
}
