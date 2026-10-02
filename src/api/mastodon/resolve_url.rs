//! Turning a URL a client pasted into search into something local, mirroring
//! Mastodon's `ResolveURLService`.
//!
//! Three paths, in Mastodon's order: a URL on our own domain is recognised from
//! its shape and never fetched; anything else is dereferenced through
//! [`crate::federation::fetch_resource`], which follows the `rel="alternate"`
//! link when the URL serves a page rather than an object; and when that comes
//! back with nothing, what the database already holds is offered instead — a
//! status can be unfetchable (private, or its author's server refusing us) and
//! still be one this viewer is allowed to see.

use crate::{
    api::ap::inbox::{fetch_remote_status_prefetched, resolve_or_fetch_remote_account_prefetched},
    db::models::{vis, Status as DbStatus},
    error::AppResult,
    federation::fetch_resource::{
        fetch_resource, type_matches, FetchedResource, ACTOR_TYPES, OBJECT_TYPES,
    },
    state::AppState,
};

/// What a URL turned out to name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    Account(i64),
    Status(i64),
}

/// Resolve `url` on behalf of `viewer`, which decides what is visible.
pub async fn resolve_url(
    state: &AppState,
    url: &str,
    viewer: Option<i64>,
) -> AppResult<Option<Resolved>> {
    let Ok(parsed) = url::Url::parse(url) else {
        return Ok(None);
    };
    if parsed
        .host_str()
        .is_some_and(|host| host.eq_ignore_ascii_case(&state.instance.domain))
    {
        return process_local_url(state, &parsed, viewer).await;
    }

    let fetched = fetch_resource(state, url).await;
    match fetched.resource {
        Some(resource) => process_url(state, resource, viewer).await,
        None => process_url_from_db(state, url, fetched.response_code, viewer).await,
    }
}

/// A URL on our own domain, recognised by its shape — Mastodon's
/// `process_local_url` asks Rails to route it, which comes to the same thing.
async fn process_local_url(
    state: &AppState,
    parsed: &url::Url,
    viewer: Option<i64>,
) -> AppResult<Option<Resolved>> {
    let segments: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.filter(|part| !part.is_empty()).collect())
        .unwrap_or_default();

    match segments.as_slice() {
        // A status: what the API serves as a status's `url`, and the actor-URI
        // form that is its `uri`.
        [handle, id] if handle.starts_with('@') => local_status(state, id, viewer).await,
        // Both actor-URI schemes: `username_ap_id` and the numeric one eunha
        // defaults to, whose sub-resources hang off `/ap/users/{id}`.
        ["users", _, "statuses", id] => local_status(state, id, viewer).await,
        ["ap", "users", _, "statuses", id] => local_status(state, id, viewer).await,
        // An account: its profile page, and both actor-URI schemes.
        [handle] if handle.starts_with('@') => local_account(state, &handle[1..]).await,
        ["users", username] => local_account(state, username).await,
        ["ap", "users", id] => {
            let Ok(id) = id.parse::<i64>() else {
                return Ok(None);
            };
            Ok(sqlx::query_scalar!(
                "SELECT id FROM accounts WHERE id = $1 AND domain IS NULL",
                id
            )
            .fetch_optional(&state.db)
            .await?
            .map(Resolved::Account))
        }
        _ => Ok(None),
    }
}

/// A status named by its numeric id in a local URL. A path segment that is not
/// an id (`/@alice/following`) names no status, and resolves to nothing.
async fn local_status(
    state: &AppState,
    id: &str,
    viewer: Option<i64>,
) -> AppResult<Option<Resolved>> {
    match id.parse::<i64>() {
        Ok(id) => authorized_status(state, id, viewer).await,
        Err(_) => Ok(None),
    }
}

/// An account named by a handle in a local URL. The handle may spell out a
/// domain — our own, which is still a local account, or someone else's, which
/// is an account we know only if we have met it.
async fn local_account(state: &AppState, handle: &str) -> AppResult<Option<Resolved>> {
    let (username, domain) = match handle.split_once('@') {
        Some((username, domain)) => (username, Some(domain)),
        None => (handle, None),
    };
    if username.is_empty() {
        return Ok(None);
    }
    let domain = domain.filter(|d| !d.eq_ignore_ascii_case(&state.instance.domain));

    // `Account.find_local` / `find_remote` are `unscoped` and do not exclude a
    // suspended account: nothing authorizes an account the way `show?`
    // authorizes a status, and the serializer already blanks an unavailable one
    // down to its `suspended: true` tombstone.
    let id = match domain {
        None => {
            sqlx::query_scalar!(
                "SELECT id FROM accounts WHERE lower(username) = lower($1) AND domain IS NULL",
                username
            )
            .fetch_optional(&state.db)
            .await?
        }
        Some(domain) => {
            sqlx::query_scalar!(
                "SELECT id FROM accounts
                 WHERE lower(username) = lower($1) AND lower(domain) = lower($2)",
                username,
                domain
            )
            .fetch_optional(&state.db)
            .await?
        }
    };
    Ok(id.map(Resolved::Account))
}

/// An object fetched from the server that owns it: store it, and say what it was.
async fn process_url(
    state: &AppState,
    resource: FetchedResource,
    viewer: Option<i64>,
) -> AppResult<Option<Resolved>> {
    if type_matches(&resource.json, &ACTOR_TYPES) {
        let id = resolve_or_fetch_remote_account_prefetched(state, &resource.url, resource.json)
            .await
            .ok();
        return Ok(id.map(Resolved::Account));
    }
    if type_matches(&resource.json, &OBJECT_TYPES) {
        let Some(id) = fetch_remote_status_prefetched(state, &resource.url, resource.json).await?
        else {
            return Ok(None);
        };
        return authorized_status(state, id, viewer).await;
    }
    // Mastodon also resolves a `FeaturedCollection` here. eunha learns of a
    // remote collection only from the `Add`/`Update` that announces one
    // (`api::ap::inbox::quote`) and has nothing that fetches one cold, so a
    // collection's URL resolves to nothing.
    Ok(None)
}

/// Nothing could be fetched. Mastodon's `process_url_from_db` reads the status
/// code to decide what that silence meant.
async fn process_url_from_db(
    state: &AppState,
    url: &str,
    response_code: Option<u16>,
    viewer: Option<i64>,
) -> AppResult<Option<Resolved>> {
    // The origin is unreachable or broken rather than the URL being wrong: an
    // account we already know is a better answer than none.
    if matches!(response_code, None | Some(500 | 502 | 503 | 504)) {
        if let Some(id) =
            sqlx::query_scalar!("SELECT id FROM accounts WHERE uri = $1 AND uri != ''", url)
                .fetch_optional(&state.db)
                .await?
        {
            return Ok(Some(Resolved::Account(id)));
        }
    }

    // A refusal, on the other hand, is what a private status looks like from
    // outside: not fetchable, but ours to show to a viewer it was sent to.
    if viewer.is_none() || !matches!(response_code, Some(401 | 403 | 404)) {
        return Ok(None);
    }
    // We index `uri`, not `url`, so a status is looked for under the URI its
    // web URL implies as well as under the URL itself.
    let guessed_uri = guess_status_uri(url);
    let found = sqlx::query_scalar!(
        "SELECT id FROM statuses
         WHERE deleted_at IS NULL
           AND (uri = $1 OR ($2::text IS NOT NULL AND uri = $2 AND url = $1))
         LIMIT 1",
        url,
        guessed_uri
    )
    .fetch_optional(&state.db)
    .await?;
    match found {
        Some(id) => authorized_status(state, id, viewer).await,
        None => Ok(None),
    }
}

/// Guess a status's `uri` from its web URL, as `ResolveURLService`'s
/// `USERNAME_STATUS_RE` does: `/@alice/123` is `/users/alice/statuses/123`.
fn guess_status_uri(url: &str) -> Option<String> {
    let mut parsed = url::Url::parse(url).ok()?;
    let segments: Vec<String> = parsed
        .path_segments()?
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    let parts: Vec<&str> = segments.iter().map(String::as_str).collect();
    let (handle, id) = match parts.as_slice() {
        [handle, id] if handle.starts_with('@') => (*handle, *id),
        [handle, "statuses", id] if handle.starts_with('@') => (*handle, *id),
        _ => return None,
    };
    let username = &handle[1..];
    if username.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    parsed.set_path(&format!("/users/{username}/statuses/{id}"));
    Some(parsed.to_string())
}

/// `StatusPolicy#show?` for a status we hold: the author has to be available,
/// the audience has to include the viewer, and the author must not have blocked
/// them.
pub(crate) async fn authorized_status(
    state: &AppState,
    id: i64,
    viewer: Option<i64>,
) -> AppResult<Option<Resolved>> {
    let Some(status) = sqlx::query_as!(
        DbStatus,
        "SELECT * FROM statuses WHERE id = $1 AND deleted_at IS NULL",
        id
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(None);
    };

    // `show?` opens with `return false if author.unavailable?`.
    let author_available = sqlx::query_scalar!(
        "SELECT 1 AS e FROM accounts
         WHERE id = $1 AND suspended_at IS NULL AND requested_deletion_at IS NULL",
        status.account_id
    )
    .fetch_optional(&state.db)
    .await?
    .is_some();
    if !author_available {
        return Ok(None);
    }

    match viewer {
        Some(viewer) => {
            match super::statuses::check_status_visible(state, &status, viewer).await {
                Ok(()) => {}
                // Not for this viewer. Anything else went wrong for another
                // reason, and saying "no such status" would bury it.
                Err(crate::error::AppError::NotFound) => return Ok(None),
                Err(e) => return Err(e),
            }
            let blocked = sqlx::query_scalar!(
                "SELECT 1 AS e FROM blocks WHERE account_id = $1 AND target_account_id = $2",
                status.account_id,
                viewer
            )
            .fetch_optional(&state.db)
            .await?
            .is_some();
            if blocked {
                return Ok(None);
            }
        }
        // `current_account.nil?` passes the block checks, but a status that
        // `requires_mention?` or is `private?` has no mention to find.
        None => {
            if !matches!(status.visibility, vis::PUBLIC | vis::UNLISTED) {
                return Ok(None);
            }
        }
    }
    Ok(Some(Resolved::Status(status.id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_a_status_uri_from_its_web_url() {
        assert_eq!(
            guess_status_uri("https://seoul.earth/@alice/109252195869957133").as_deref(),
            Some("https://seoul.earth/users/alice/statuses/109252195869957133")
        );
        assert_eq!(
            guess_status_uri("https://seoul.earth/@alice/statuses/109252195869957133").as_deref(),
            Some("https://seoul.earth/users/alice/statuses/109252195869957133")
        );
    }

    #[test]
    fn guesses_nothing_from_a_url_of_another_shape() {
        // A UUID is not a Mastodon status id: oeee.cafe's URLs are not this
        // shape, and a guess for them would be a lookup nobody could answer.
        assert_eq!(
            guess_status_uri("https://oeee.cafe/@pokemon/75fbf20d-31dd-402e-93f5-de349b58c76f"),
            None
        );
        assert_eq!(guess_status_uri("https://seoul.earth/@alice"), None);
        assert_eq!(guess_status_uri("https://seoul.earth/tags/art"), None);
    }

    /// `USERNAME_STATUS_RE` accepts any alphanumeric last segment, so a
    /// profile's sub-page guesses a URI too. The guess is only ever a key to
    /// look a status up by, and nothing is stored under this one, so it costs a
    /// miss and no more.
    #[test]
    fn a_guess_is_only_a_lookup_key() {
        assert_eq!(
            guess_status_uri("https://seoul.earth/@alice/following").as_deref(),
            Some("https://seoul.earth/users/alice/statuses/following")
        );
    }
}
