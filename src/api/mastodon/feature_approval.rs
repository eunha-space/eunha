//! `feature_approval.current_user` for the viewer.
//!
//! Mastodon's `REST::AccountSerializer` answers it from `current_user`, for
//! every account it renders, wherever that account sits in the response, and
//! `StatusCacheHydrator#hydrate_account` sets it again on a payload rendered
//! once and handed to many receivers. Eunha renders accounts in many places
//! that do not know the viewer, so, as the hydrator does, it is set on what
//! was rendered: once per response, for every account object in it.

use std::collections::HashMap;

use axum::body::{Body, HttpBody};
use axum::extract::Request;
use axum::http::header;
use axum::middleware::Next;
use axum::response::Response;
use serde_json::Value;

use super::convert::{feature_policy_for_viewer, AccountViewerContext};
use crate::middleware::AuthenticatedUser;
use crate::state::AppState;

/// The largest response body this reads to hydrate. Anything larger is an
/// export or an archive rather than a page of entities.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// Sets `feature_approval.current_user` on every account object in `value`
/// for `viewer`.
pub async fn hydrate(state: &AppState, viewer: i64, value: &mut Value) {
    let mut ids = Vec::new();
    collect(value, &mut ids);
    if ids.is_empty() {
        return;
    }
    ids.sort_unstable();
    ids.dedup();
    let rows = match sqlx::query!(
        r#"SELECT a.id, (a.domain IS NULL) AS "local!",
                  COALESCE(a.discoverable, false) AS "discoverable!",
                  a.locked, a.feature_approval_policy,
                  EXISTS (SELECT 1 FROM follows f
                          WHERE f.account_id = $2 AND f.target_account_id = a.id) AS "viewer_follows!",
                  EXISTS (SELECT 1 FROM follows f
                          WHERE f.account_id = a.id AND f.target_account_id = $2) AS "follows_viewer!"
           FROM accounts a WHERE a.id = ANY($1)"#,
        &ids,
        viewer,
    )
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "could not read feature policies for the viewer");
            return;
        }
    };
    let answers: HashMap<i64, &'static str> = rows
        .into_iter()
        .map(|row| {
            let ctx = AccountViewerContext {
                is_self: row.id == viewer,
                viewer_follows: row.viewer_follows,
                follows_viewer: row.follows_viewer,
            };
            let answer = feature_policy_for_viewer(
                row.local,
                row.discoverable,
                row.locked,
                row.feature_approval_policy,
                Some(&ctx),
            );
            (row.id, answer)
        })
        .collect();
    apply(value, &answers);
}

/// The id of `object` when it is an account: the one entity with a
/// `feature_approval` object.
fn account_id(object: &serde_json::Map<String, Value>) -> Option<i64> {
    object
        .get("feature_approval")
        .filter(|approval| approval.get("current_user").is_some())?;
    object.get("id")?.as_str()?.parse().ok()
}

fn collect(value: &Value, ids: &mut Vec<i64>) {
    match value {
        Value::Object(object) => {
            if let Some(id) = account_id(object) {
                ids.push(id);
            }
            object.values().for_each(|v| collect(v, ids));
        }
        Value::Array(items) => items.iter().for_each(|v| collect(v, ids)),
        _ => {}
    }
}

fn apply(value: &mut Value, answers: &HashMap<i64, &'static str>) {
    match value {
        Value::Object(object) => {
            if let Some(answer) = account_id(object).and_then(|id| answers.get(&id)) {
                if let Some(Value::Object(approval)) = object.get_mut("feature_approval") {
                    approval.insert("current_user".into(), Value::from(*answer));
                }
            }
            object.values_mut().for_each(|v| apply(v, answers));
        }
        Value::Array(items) => items.iter_mut().for_each(|v| apply(v, answers)),
        _ => {}
    }
}

/// Hydrates a JSON API response for its signed-in viewer. A response with no
/// account in it is passed on untouched.
pub async fn for_viewer(req: Request, next: Next) -> Response {
    let state = req.extensions().get::<AppState>().cloned();
    let viewer = req
        .extensions()
        .get::<AuthenticatedUser>()
        .filter(|auth| auth.user_id.is_some())
        .map(|auth| auth.account_id);
    let response = next.run(req).await;
    let (Some(state), Some(viewer)) = (state, viewer) else {
        return response;
    };
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if !is_json || !response.status().is_success() {
        return response;
    }
    // Only a body already whole in memory, as a rendered entity is; a
    // streamed one is passed on as it is.
    let whole = response
        .body()
        .size_hint()
        .exact()
        .is_some_and(|len| len <= MAX_BODY as u64);
    if !whole {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return Response::from_parts(parts, Body::empty());
    };
    if !contains(&bytes, b"\"feature_approval\"") {
        return Response::from_parts(parts, Body::from(bytes));
    }
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    hydrate(&state, viewer, &mut value).await;
    match serde_json::to_vec(&value) {
        Ok(hydrated) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(hydrated))
        }
        Err(_) => Response::from_parts(parts, Body::from(bytes)),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_accounts_wherever_they_sit() {
        let value = json!([
            {"id": "1", "account": {"id": "2", "feature_approval": {"current_user": "denied"}},
             "reblog": {"account": {"id": "3", "feature_approval": {"current_user": "denied"}}}},
            {"id": "4", "feature_approval": {"current_user": "denied"},
             "moved": {"id": "5", "feature_approval": {"current_user": "denied"}}}
        ]);
        let mut ids = Vec::new();
        collect(&value, &mut ids);
        ids.sort_unstable();
        assert_eq!(ids, vec![2, 3, 4, 5]);
    }

    #[test]
    fn sets_only_the_accounts_it_has_answers_for() {
        let mut value = json!({
            "accounts": [
                {"id": "2", "feature_approval": {"automatic": [], "current_user": "denied"}},
                {"id": "3", "feature_approval": {"automatic": [], "current_user": "denied"}}
            ]
        });
        apply(&mut value, &HashMap::from([(2, "automatic")]));
        assert_eq!(
            value["accounts"][0]["feature_approval"]["current_user"],
            "automatic"
        );
        assert_eq!(
            value["accounts"][1]["feature_approval"]["current_user"],
            "denied"
        );
        assert_eq!(
            value["accounts"][0]["feature_approval"]["automatic"],
            json!([])
        );
    }
}
