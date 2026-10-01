//! `ActivityPub::TagManager#uri_to_actor` and `#uri_to_resource` without
//! fetching: a URI on this instance's domain names a local account or status
//! by its path; any other is looked up by the `uri` it was stored under.

use crate::state::AppState;

fn local_path(state: &AppState, uri: &str) -> Option<Vec<String>> {
    let url = url::Url::parse(uri).ok()?;
    let host = url.host_str()?;
    let ours = host.eq_ignore_ascii_case(&state.instance.domain)
        || state
            .instance
            .aliases
            .iter()
            .any(|alias| host.eq_ignore_ascii_case(alias));
    if !ours {
        return None;
    }
    Some(url.path_segments()?.map(str::to_owned).collect())
}

/// The account a URI names, local or already known.
pub async fn account(state: &AppState, uri: &str) -> Option<i64> {
    match local_path(state, uri) {
        Some(path) => {
            let path: Vec<&str> = path.iter().map(String::as_str).collect();
            match path.as_slice() {
                ["actor"] => Some(crate::federation::instance_actor::INSTANCE_ACTOR_ID),
                ["users", username] => by_username(state, username).await,
                ["ap", "users", id] => by_id(state, id).await,
                [handle] if handle.starts_with('@') => by_username(state, &handle[1..]).await,
                _ => None,
            }
        }
        None => sqlx::query_scalar!("SELECT id FROM accounts WHERE uri = $1", uri)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten(),
    }
}

/// The status a URI names, local or already known, deleted ones included.
pub async fn status(state: &AppState, uri: &str) -> Option<i64> {
    match local_path(state, uri) {
        Some(path) => {
            let path: Vec<&str> = path.iter().map(String::as_str).collect();
            let id = match path.as_slice() {
                ["users", _, "statuses", id] => *id,
                ["ap", "users", _, "statuses", id] => *id,
                [handle, id] if handle.starts_with('@') => *id,
                _ => return None,
            };
            let id: i64 = id.parse().ok()?;
            sqlx::query_scalar!(
                "SELECT s.id FROM statuses s JOIN accounts a ON a.id = s.account_id WHERE s.id = $1 AND a.domain IS NULL",
                id
            )
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
        }
        None => sqlx::query_scalar!("SELECT id FROM statuses WHERE uri = $1", uri)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten(),
    }
}

/// The collection a URI names, if one is stored under it.
pub async fn collection(state: &AppState, uri: &str) -> Option<i64> {
    sqlx::query_scalar!("SELECT id FROM collections WHERE uri = $1", uri)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
}

async fn by_username(state: &AppState, username: &str) -> Option<i64> {
    sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE domain IS NULL AND lower(username) = lower($1)",
        username
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
}

async fn by_id(state: &AppState, id: &str) -> Option<i64> {
    let id: i64 = id.parse().ok()?;
    sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE domain IS NULL AND id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
}
