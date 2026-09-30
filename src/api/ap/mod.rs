pub mod collections;
pub mod inbox;
pub mod note;
pub mod objects;
pub mod serving;

use axum::{
    extract::Path,
    response::{IntoResponse, Redirect, Response},
    routing::get,
    Router,
};

use crate::state::AppState;

/// Where a browser that opens an ActivityPub URI is sent. Everything a
/// server fetches or sends is ojak's (`serving`), answered before these
/// routes when ActivityPub is asked for.
pub fn router() -> Router {
    Router::new()
        .route("/users/{username}", get(profile_by_username))
        .route("/users/{username}/statuses/{id}", get(status_by_username))
        .route("/users/{username}/followers", get(followers_by_username))
        .route("/users/{username}/following", get(following_by_username))
        .route("/ap/users/{id}", get(profile_by_id))
        .route("/ap/users/{id}/statuses/{status_id}", get(status_by_id))
}

// A person who opens an actor, a status, or an account's followers or
// following in a browser is sent to its page, as Mastodon sends them.

async fn profile_by_username(Path(username): Path<String>) -> Redirect {
    Redirect::to(&format!("/@{username}"))
}

async fn status_by_username(Path((username, id)): Path<(String, String)>) -> Redirect {
    Redirect::to(&format!("/@{username}/{id}"))
}

async fn followers_by_username(Path(username): Path<String>) -> Redirect {
    Redirect::to(&format!("/@{username}/followers"))
}

async fn following_by_username(Path(username): Path<String>) -> Redirect {
    Redirect::to(&format!("/@{username}/following"))
}

async fn username_of(state: &AppState, id: &str) -> Option<String> {
    let id: i64 = id.parse().ok()?;
    sqlx::query_scalar!(
        "SELECT username FROM accounts WHERE id = $1 AND domain IS NULL",
        id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
}

async fn profile_by_id(state: AppState, Path(id): Path<String>) -> Response {
    match username_of(&state, &id).await {
        Some(username) => Redirect::to(&format!("/@{username}")).into_response(),
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

async fn status_by_id(state: AppState, Path((id, status_id)): Path<(String, String)>) -> Response {
    match username_of(&state, &id).await {
        Some(username) => Redirect::to(&format!("/@{username}/{status_id}")).into_response(),
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}
