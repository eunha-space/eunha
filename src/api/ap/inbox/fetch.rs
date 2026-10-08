//! Fetching remote ActivityPub objects into local rows: Mastodon's
//! `ActivityPub::FetchRemoteStatusService`, which processes a fetched status
//! as the `Create` (or `Update`, or `Announce`) it would have come in, and
//! resolving-or-fetching a remote account. These are the shared entry points
//! the inbound activity handlers and the API use to materialise objects they
//! reference.

use serde_json::Value;

use crate::{
    error::{AppError, AppResult},
    state::AppState,
};

/// Resolve a status by URI as Mastodon's `FetchRemoteStatusService` does:
/// fetched from its server, and processed as the `Create` it would have come
/// in — or, when we already hold it, as an `Update`. Returns the local status
/// id, or `None` when the object cannot be had or is not a status.
pub async fn fetch_remote_status(state: &AppState, uri: &str) -> AppResult<Option<i64>> {
    Ok(Box::pin(fetch_remote_status_with(
        state,
        uri,
        FetchOptions::default(),
    ))
    .await?
    .map(|(id, _)| id))
}

/// As [`fetch_remote_status`], for an object already in hand: the caller has
/// fetched it from the server that owns its id (Mastodon's `prefetched_body`),
/// so processing it needs no second request.
pub async fn fetch_remote_status_prefetched(
    state: &AppState,
    uri: &str,
    json: Value,
) -> AppResult<Option<i64>> {
    Ok(Box::pin(fetch_remote_status_with(
        state,
        uri,
        FetchOptions {
            prefetched_body: Some(json),
            ..FetchOptions::default()
        },
    ))
    .await?
    .map(|(id, _)| id))
}

/// Largest depth to which quoted posts are fetched while a status is
/// processed (`VerifyQuoteService::MAX_SYNCHRONOUS_DEPTH`).
pub(super) const MAX_FETCH_DEPTH: u8 = 2;

/// [`fetch_remote_status`] at `depth` in a chain of quotes, with the object
/// already in hand when `prefetched`.
pub(super) async fn fetch_remote_status_at_depth(
    state: &AppState,
    uri: &str,
    prefetched: Option<Value>,
    depth: u8,
) -> AppResult<Option<i64>> {
    Ok(Box::pin(fetch_remote_status_with(
        state,
        uri,
        FetchOptions {
            prefetched_body: prefetched,
            depth,
            ..FetchOptions::default()
        },
    ))
    .await?
    .map(|(id, _)| id))
}

/// `::FetchRemoteStatusService#call` (not the `ActivityPub::` one): the
/// document at `url`, found as `FetchResourceService` finds it — which
/// follows a page's link to its ActivityPub object, and gives nothing for a
/// request that is not answered — unless it is already in hand, and then
/// processed by [`fetch_remote_status_with`]. A status of ours is only
/// looked up.
pub async fn fetch_remote_status_by_url(
    state: &AppState,
    url: &str,
    prefetched_body: Option<Value>,
    request_id: Option<String>,
) -> AppResult<Option<(i64, bool)>> {
    if crate::federation::local_uri::is_local(state, url) {
        let id = crate::federation::local_uri::status(state, url).await;
        return Ok(id.map(|id| (id, false)));
    }
    let (url, json) = match prefetched_body {
        Some(json) => (url.to_owned(), json),
        None => {
            let fetched = crate::federation::fetch_resource::fetch_resource(state, url).await;
            let Some(resource) = fetched.resource else {
                tracing::debug!(url, "could not fetch status");
                return Ok(None);
            };
            (resource.url, resource.json)
        }
    };
    Box::pin(fetch_remote_status_with(
        state,
        &url,
        FetchOptions {
            prefetched_body: Some(json),
            request_id,
            ..FetchOptions::default()
        },
    ))
    .await
}

/// `ActivityPub::FetchRemotePollService`: the status a remote poll is part
/// of, fetched again on behalf of `on_behalf_of` and processed as an update
/// of it by its author (`ProcessStatusUpdateService`), which refreshes the
/// poll's tallies and `last_fetched_at`. A request that is not answered is an
/// `Err`, as Mastodon raises it ([`unanswered`]); a document that cannot be
/// had otherwise changes nothing.
pub async fn fetch_remote_poll(
    state: &AppState,
    status_id: i64,
    on_behalf_of: Option<i64>,
) -> AppResult<()> {
    use crate::federation::json_ld;

    let Some(status) = sqlx::query!(
        r#"SELECT s.uri AS "uri?", a.uri AS "account_uri?"
           FROM statuses s JOIN accounts a ON a.id = s.account_id
           WHERE s.id = $1 AND s.deleted_at IS NULL AND a.domain IS NOT NULL"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let (Some(uri), Some(account_uri)) = (status.uri, status.account_uri) else {
        return Ok(());
    };
    let json = json_ld::fetch_resource(state, &uri, on_behalf_of, json_ld::RaiseOn::None)
        .await
        .map_err(AppError::Internal)?;
    let Some(json) = json.filter(json_ld::supported_context) else {
        return Ok(());
    };
    if !super::status_parser::is_status_type(&json) {
        return Ok(());
    }
    let activity = serde_json::json!({
        "type": "Update",
        "actor": account_uri,
        "object": json,
    });
    Box::pin(super::status::handle_update(
        state,
        &state.instance,
        &activity,
    ))
    .await
}

/// `ActivityPub::FetchRemoteStatusService#call`'s options.
#[derive(Debug, Default, Clone)]
pub struct FetchOptions {
    /// The document, already fetched from the server that owns its id.
    pub prefetched_body: Option<Value>,
    /// The local account the fetch is signed on behalf of; the instance
    /// actor when there is none.
    pub on_behalf_of: Option<i64>,
    /// The actor the status has to be by.
    pub expected_actor_uri: Option<String>,
    /// The chain of fetches this one belongs to; one is made up when there
    /// is none, as Mastodon's `"#{Time.now.utc.to_i}-status-#{uri}"`.
    pub request_id: Option<String>,
    /// How deep in a chain of quotes the status is.
    pub depth: u8,
}

/// `ActivityPub::FetchRemoteStatusService::DISCOVERIES_PER_REQUEST`.
const DISCOVERIES_PER_REQUEST: i64 = 1000;

/// `ActivityPub::FetchRemoteStatusService#call`: the status document at
/// `uri` — fetched, or in hand — processed as the `Create` it would have come
/// in, or, when we already hold it from its author, as an `Update`; an
/// `Announce` document as the `Announce` it is. Returns the status's id and
/// whether it was new to us (`previously_new_record?`).
///
/// A chain of fetches (`request_id`) stops after [`DISCOVERIES_PER_REQUEST`]
/// statuses, which is what bounds a thread whose every reply has its own
/// replies read.
///
/// A request for the status that was not answered at all — a connection
/// error, a refused address, a redirect that could not be followed — is an
/// `Err`, as Mastodon raises one, for the caller to rescue or retry as
/// Mastodon's does ([`unanswered`] says which errors those are). A status
/// the server answers with is `None`.
pub async fn fetch_remote_status_with(
    state: &AppState,
    uri: &str,
    options: FetchOptions,
) -> AppResult<Option<(i64, bool)>> {
    use crate::federation::fetch_resource::type_matches;
    use crate::federation::json_ld;

    if uri.is_empty() || crate::federation::moderation::domain_not_allowed(state, uri).await {
        return Ok(None);
    }
    let request_id = options
        .request_id
        .unwrap_or_else(|| format!("{}-status-{uri}", chrono::Utc::now().timestamp()));

    // `body_to_json(prefetched_body, compare_id: uri)`, or `fetch_status`.
    let json = match options.prefetched_body {
        Some(json) => json
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id == uri)
            .then_some(json),
        None => fetch_status(state, uri, options.on_behalf_of).await?,
    };
    let Some(json) = json else {
        return Ok(None);
    };
    if !json_ld::supported_context(&json) {
        return Ok(None);
    }

    let first_id = |value: Option<&Value>| -> Option<String> {
        let value = match value? {
            Value::Array(items) => items.first()?,
            value => value,
        };
        json_ld::value_or_id(value).map(str::to_owned)
    };
    let (mut activity, actor_uri, object_uri) = if super::status_parser::is_status_type(&json) {
        let actor_uri = first_id(json.get("attributedTo"));
        let object_uri = super::status_parser::uri(&json);
        let mut activity = serde_json::json!({
            "type": "Create",
            "actor": actor_uri,
            "object": json,
        });
        if let Some(context) = activity["object"].get("@context").cloned() {
            activity["@context"] = context;
        }
        (activity, actor_uri, object_uri)
    } else if type_matches(&json, &["Create", "Announce"]) {
        let actor_uri = first_id(json.get("actor"));
        let object_uri = json
            .get("object")
            .and_then(json_ld::value_or_id)
            .map(str::to_owned);
        (json, actor_uri, object_uri)
    } else {
        return Ok(None);
    };
    let (Some(actor_uri), Some(object_uri)) = (actor_uri, object_uri) else {
        return Ok(None);
    };
    // `trustworthy_attribution?`: the document's id and its actor on one host.
    let Some(document_id) = activity
        .get("id")
        .or_else(|| activity["object"].get("id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(None);
    };
    if !same_host(&document_id, &actor_uri) {
        return Ok(None);
    }
    if options
        .expected_actor_uri
        .as_deref()
        .is_some_and(|expected| !expected.is_empty() && expected != actor_uri)
    {
        return Ok(None);
    }
    if crate::federation::local_uri::is_local(state, &object_uri) {
        let id = crate::federation::local_uri::status(state, &object_uri).await;
        return Ok(id.map(|id| (id, false)));
    }

    // `account_from_uri`, and `return if actor.nil? || actor.suspended?`.
    let Ok(account_id) = resolve_or_fetch_remote_account(state, &actor_uri).await else {
        return Ok(None);
    };
    let suspended = sqlx::query_scalar!(
        r#"SELECT (suspended_at IS NOT NULL) AS "suspended!" FROM accounts WHERE id = $1"#,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(true);
    if suspended {
        return Ok(None);
    }

    let announce = type_matches(&activity, &["Announce"]);
    let held_by_actor = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE uri = $1 AND account_id = $2 AND deleted_at IS NULL",
        object_uri,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?;
    // A status we already have is an `Update` rather than a `Create`.
    let update = !announce && held_by_actor.is_some();
    if update {
        activity["type"] = Value::String("Update".into());
    }
    let result_uri = if announce {
        document_id.clone()
    } else {
        object_uri.clone()
    };
    let held = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM statuses WHERE uri = $1) AS "e!""#,
        result_uri,
    )
    .fetch_one(&state.db)
    .await?;

    if discovery_limit_reached(state, &request_id).await {
        return Ok(None);
    }

    if announce {
        Box::pin(super::status::handle_announce(
            state,
            &state.instance,
            &activity,
        ))
        .await?;
    } else if update {
        Box::pin(super::status::handle_update(
            state,
            &state.instance,
            &activity,
        ))
        .await?;
    } else {
        Box::pin(super::create::create(
            state,
            &activity,
            &super::create::CreateOptions {
                fetched: true,
                request_id: Some(request_id),
                depth: options.depth,
            },
        ))
        .await?;
    }
    let id = sqlx::query_scalar!(
        "SELECT id FROM statuses WHERE uri = $1 AND deleted_at IS NULL",
        result_uri,
    )
    .fetch_optional(&state.db)
    .await?;
    Ok(id.map(|id| (id, !held)))
}

/// `FetchRemoteStatusService#fetch_status`: `fetch_resource(uri, true,
/// on_behalf_of, raise_on_error: :all)`, where a `404` for a public status
/// we hold from elsewhere removes it.
async fn fetch_status(
    state: &AppState,
    uri: &str,
    on_behalf_of: Option<i64>,
) -> AppResult<Option<Value>> {
    use crate::federation::json_ld;
    match json_ld::fetch_resource(state, uri, on_behalf_of, json_ld::RaiseOn::All).await {
        Ok(json) => Ok(json),
        Err(error) => {
            let status = error
                .downcast_ref::<ojak::fetch::FetchError>()
                .and_then(ojak::fetch::FetchError::status);
            if status.is_none()
                && crate::federation::json_ld::raises(
                    error
                        .downcast_ref::<ojak::fetch::FetchError>()
                        .unwrap_or(&ojak::fetch::FetchError::NoId),
                    json_ld::RaiseOn::All,
                )
            {
                return Err(AppError::Internal(error));
            }
            if status == Some(404) {
                let orphan = sqlx::query_scalar!(
                    "SELECT id FROM statuses
                     WHERE uri = $1 AND local = false AND deleted_at IS NULL
                       AND visibility IN (0, 1)",
                    uri,
                )
                .fetch_optional(&state.db)
                .await?;
                if let Some(status_id) = orphan {
                    tracing::debug!(uri, "got 404 for an orphaned status, deleting it");
                    super::status::remove_remote_status(state, status_id).await?;
                }
            } else {
                tracing::debug!(uri, %error, "could not fetch status");
            }
            Ok(None)
        }
    }
}

/// Whether `error` is a request for a status that was not answered, which
/// Mastodon raises as one of its `HTTP_CONNECTION_ERRORS` (or a refused
/// host): what its callers rescue, or let their jobs retry.
pub fn unanswered(error: &AppError) -> bool {
    match error {
        AppError::Internal(error) => {
            error
                .downcast_ref::<ojak::fetch::FetchError>()
                .is_some_and(|error| {
                    error.status().is_none()
                        && crate::federation::json_ld::raises(
                            error,
                            crate::federation::json_ld::RaiseOn::All,
                        )
                })
        }
        _ => false,
    }
}

/// Count one more status discovered under `request_id`, and say whether the
/// chain has gone past [`DISCOVERIES_PER_REQUEST`]. A key that lives five
/// minutes, under the instance's prefix.
async fn discovery_limit_reached(state: &AppState, request_id: &str) -> bool {
    let key = state
        .redis_keys
        .key(format!("status_discovery_per_request:{request_id}"));
    let mut redis = state.redis.clone();
    let discoveries: redis::RedisResult<(i64,)> = redis::pipe()
        .cmd("INCRBY")
        .arg(&key)
        .arg(1)
        .cmd("EXPIRE")
        .arg(&key)
        .arg(5 * 60)
        .ignore()
        .query_async(&mut redis)
        .await;
    discoveries.is_ok_and(|(discoveries,)| discoveries > DISCOVERIES_PER_REQUEST)
}

/// Whether two URIs name the same HTTP(S) host, which is how Mastodon decides
/// whether an object's attribution can be believed; for portable ids, the
/// same DID, whose proof is what vouches for them.
use ojak::origin::same_authority as same_host;

/// Looks up a remote account by URI, fetching it from the remote server if unknown.
pub async fn resolve_or_fetch_remote_account(state: &AppState, actor_uri: &str) -> AppResult<i64> {
    resolve_or_fetch_remote_account_inner(state, actor_uri, None).await
}

/// Mastodon's `ActivityPub::FetchRemoteAccountService`: the account at
/// `actor_uri` as its server now describes it, fetched even when known, so
/// that what is read off it (a Move's `alsoKnownAs`) is current. A URI on
/// this instance is the local account, without a fetch.
pub async fn fetch_remote_account(state: &AppState, actor_uri: &str) -> AppResult<i64> {
    let ours = ojak::origin::host_of(actor_uri).is_some_and(|host| {
        host.eq_ignore_ascii_case(&state.instance.domain)
            || state
                .instance
                .aliases
                .iter()
                .any(|alias| host.eq_ignore_ascii_case(alias))
    });
    if ours {
        return crate::federation::local_uri::account(state, actor_uri)
            .await
            .ok_or(AppError::NotFound);
    }
    // `FetchRemoteActorService`: `return if domain_not_allowed?(uri)`.
    if crate::federation::moderation::domain_not_allowed(state, actor_uri).await {
        return Err(AppError::NotFound);
    }
    let actor = crate::federation::fetch::signed_get_json(state, actor_uri)
        .await
        .map_err(AppError::Internal)?;
    // The document has to be the actor asked for: one naming another would
    // rewrite that other account.
    match crate::federation::process_account::process_fetched_actor(
        state, actor_uri, &actor, false, false, None,
    )
    .await
    {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err(AppError::NotFound),
        Err(error) => {
            tracing::debug!(actor_uri, %error, "actor not stored");
            Err(AppError::NotFound)
        }
    }
}

/// As [`resolve_or_fetch_remote_account`], for an actor document already in
/// hand (Mastodon's `prefetched_body`).
pub async fn resolve_or_fetch_remote_account_prefetched(
    state: &AppState,
    actor_uri: &str,
    json: Value,
) -> AppResult<i64> {
    resolve_or_fetch_remote_account_inner(state, actor_uri, Some(json)).await
}

/// The actor document ojak fetched to verify a signature, which the account
/// is stored from: in full for an actor not known yet or not refreshed for a
/// day, and otherwise only its keys, as Mastodon's signature verification
/// refreshes a key that no longer verifies (`keypair_refresh_key!`).
pub async fn store_key_fetched_actor(state: &AppState, actor: Value) -> AppResult<Option<i64>> {
    let Some(id) = actor.get("id").and_then(Value::as_str).map(str::to_owned) else {
        return Ok(None);
    };
    let canonical = crate::federation::portable::canonical(&id);
    let known = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE uri = $1 AND domain IS NOT NULL ORDER BY id LIMIT 1",
        canonical,
    )
    .fetch_optional(&state.db)
    .await?;
    let only_key = known
        .as_ref()
        .is_some_and(|account| !crate::federation::process_account::possibly_stale(account));
    crate::federation::process_account::process_fetched_actor(
        state, &id, &actor, false, only_key, None,
    )
    .await
    .map_err(AppError::Internal)
}

async fn resolve_or_fetch_remote_account_inner(
    state: &AppState,
    actor_uri: &str,
    prefetched: Option<Value>,
) -> AppResult<i64> {
    // A portable actor is fetched by the id as given, whose location hints
    // say where, and known by its canonical id.
    let fetch_uri = actor_uri;
    let canonical = crate::federation::portable::canonical(actor_uri);
    let actor_uri = canonical.as_str();
    // An actor URI on our own domain is a *local* account, not a remote one.
    // Resolve it directly (local accounts store an empty `uri`, so the lookup
    // below would miss it) rather than signed-fetching our own actor endpoint,
    // which would mint a remote-looking duplicate with domain = our own domain.
    // Such duplicates break every `domain IS NULL` local check — e.g. a mention
    // resolving to the duplicate never fires the local mention notification.
    if let Ok(parsed) = url::Url::parse(actor_uri) {
        if parsed
            .host_str()
            .is_some_and(|h| h.eq_ignore_ascii_case(&state.instance.domain))
        {
            let segments: Vec<&str> = parsed
                .path_segments()
                .map(|s| s.collect())
                .unwrap_or_default();
            let local_id = match segments.as_slice() {
                // https://{domain}/users/{username}
                ["users", username] => {
                    sqlx::query_scalar!(
                    "SELECT id FROM accounts WHERE lower(username) = lower($1) AND domain IS NULL",
                    username,
                )
                    .fetch_optional(&state.db)
                    .await?
                }
                // https://{domain}/ap/users/{id}
                ["ap", "users", id] => match id.parse::<i64>() {
                    Ok(numeric) => {
                        sqlx::query_scalar!(
                            "SELECT id FROM accounts WHERE id = $1 AND domain IS NULL",
                            numeric,
                        )
                        .fetch_optional(&state.db)
                        .await?
                    }
                    Err(_) => None,
                },
                _ => None,
            };
            // On our own domain, never fall through to a remote fetch: either we
            // found the local account or there is no such account.
            return local_id.ok_or(AppError::NotFound);
        }
    }

    let known = || async {
        sqlx::query_scalar!(
            "SELECT id FROM accounts WHERE uri = $1 ORDER BY id LIMIT 1",
            actor_uri
        )
        .fetch_optional(&state.db)
        .await
    };
    if let Some(id) = known().await? {
        return Ok(id);
    }
    // `FetchRemoteActorService` and `ProcessAccountService`: an account this
    // instance does not know is neither fetched nor created from a domain it
    // does not federate with.
    if crate::federation::moderation::domain_not_allowed(state, actor_uri).await {
        return Err(AppError::NotFound);
    }

    let actor: Value = match prefetched {
        Some(json) => json,
        None => crate::federation::fetch::signed_get_json(state, fetch_uri)
            .await
            .map_err(AppError::Internal)?,
    };
    match crate::federation::process_account::process_fetched_actor(
        state, actor_uri, &actor, false, false, None,
    )
    .await
    {
        Ok(Some(id)) => Ok(id),
        outcome => {
            if let Err(error) = outcome {
                tracing::debug!(actor_uri, %error, "actor not stored");
            }
            // Another request may have stored it meanwhile.
            known().await?.ok_or(AppError::NotFound)
        }
    }
}
