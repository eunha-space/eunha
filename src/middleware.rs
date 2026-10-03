use axum::{
    extract::Request,
    middleware::Next,
    response::{IntoResponse as _, Response},
};
use tracing::Instrument as _;

use crate::{config::InstanceConfig, error::AppError, state::AppState};

/// Resolved instance config, injected into request extensions by [`resolve_instance`].
#[derive(Clone)]
pub struct ResolvedInstance(pub InstanceConfig);

/// Injects the instance config into every request's extensions, and runs the
/// request in its tenant's span, so that everything it logs — and every task it
/// spawns through [`crate::tenants::spawn`] — names the instance.
pub async fn resolve_instance(
    state: AppState,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    req.extensions_mut()
        .insert(ResolvedInstance((*state.instance).clone()));
    let span = crate::tenants::span(&state.instance.domain);
    Ok(next.run(req).instrument(span).await)
}

/// Resolved OAuth token + account, injected by [`authenticate`].
#[derive(Clone)]
pub struct AuthenticatedUser {
    pub account_id: i64,
    pub user_id: Option<i64>,
    pub token_id: i64,
    pub scopes: Vec<String>,
    pub application_id: Option<i64>,
    /// Where the user stands, for `require_user!`.
    pub standing: Standing,
    /// `users.disabled`, which the streaming server refuses outright.
    pub user_disabled: bool,
}

/// What `Api::BaseController#require_user!` asks of the user behind a token,
/// in the order it asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// `User#functional?`.
    Functional,
    /// No confirmed e-mail address.
    Unconfirmed,
    /// Awaiting approval.
    Pending,
    /// Confirmed and approved, but disabled, a memorial, moved, or missing
    /// the two-factor authentication its role requires.
    Disabled,
}

/// A valid token with no user behind it, from the client-credentials grant.
#[derive(Clone, Copy)]
pub struct AppToken;

/// A token whose account is suspended or deleting: `require_not_suspended!`
/// refuses every API request made with it.
#[derive(Clone, Copy)]
pub struct UnavailableAccount;

impl AuthenticatedUser {
    /// Returns `Err(AppError::Forbidden)` if the token does not cover `required`.
    ///
    /// Scope hierarchy:
    /// - `"read"` covers every `"read:*"` sub-scope.
    /// - `"write"` covers every `"write:*"` sub-scope.
    /// - `"follow"` covers all social-graph operations (read+write on follows/blocks/mutes).
    pub fn require_scope(&self, required: &str) -> crate::error::AppResult<()> {
        if self.has_scope(required) {
            Ok(())
        } else {
            Err(crate::error::AppError::ForbiddenScope)
        }
    }

    fn has_scope(&self, required: &str) -> bool {
        if self.scopes.iter().any(|s| s == required) {
            return true;
        }
        // Parent scope covers child: "read" → "read:*", "write" → "write:*",
        // and "admin:read" → "admin:read:*" (there is no bare "admin").
        if let Some((parent, _)) = required.rsplit_once(':') {
            if self.scopes.iter().any(|s| s == parent) {
                return true;
            }
        }
        // "follow" covers all social-graph operations (read + write on follows/blocks/mutes)
        if matches!(
            required,
            "write:follows"
                | "write:blocks"
                | "write:mutes"
                | "read:follows"
                | "read:blocks"
                | "read:mutes"
        ) && self.scopes.iter().any(|s| s == "follow")
        {
            return true;
        }
        // "profile" is a narrow scope that covers read:accounts (for verify_credentials)
        if required == "read:accounts" && self.scopes.iter().any(|s| s == "profile") {
            return true;
        }
        false
    }
}

/// Bearer token authentication. Attaches `AuthenticatedUser` if a valid token
/// is present; passes through unauthenticated requests so endpoints can decide
/// whether auth is required.
pub async fn authenticate(state: AppState, mut req: Request, next: Next) -> Response {
    if let Some(token) = extract_bearer(&req) {
        if let Some(tok) = sqlx::query!(
            r#"SELECT t.id, u.account_id AS "account_id?", t.application_id, t.scopes,
                      t.expires_in, t.created_at, t.revoked_at, t.last_used_at, u.id as "user_id?",
                      u.current_sign_in_at AS "current_sign_in_at?",
                      u.disabled as "disabled?", a.suspended_at AS "suspended_at?",
                      a.requested_deletion_at AS "requested_deletion_at?",
                      (u.confirmed_at IS NOT NULL) AS "confirmed?", u.approved AS "approved?",
                      (a.memorial OR a.moved_to_account_id IS NOT NULL
                       OR (COALESCE(r.require_2fa, false) AND NOT u.otp_required_for_login
                           AND NOT EXISTS (SELECT 1 FROM webauthn_credentials w WHERE w.user_id = u.id))
                      ) AS "restricted?"
               FROM oauth_access_tokens t
               LEFT JOIN users u ON u.id = t.resource_owner_id
               LEFT JOIN accounts a ON a.id = u.account_id
               LEFT JOIN user_roles r ON r.id = COALESCE(u.role_id, -99)
               WHERE t.token = $1"#,
            token
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        {
            let valid =
                tok.revoked_at.is_none() && token_not_expired(tok.created_at, tok.expires_in);
            // Mastodon's `Api::BaseController#require_not_suspended!`: an
            // unavailable account cannot act, but its tokens stay intact so
            // unsuspending restores access without a new sign-in.
            let account_unavailable =
                tok.suspended_at.is_some() || tok.requested_deletion_at.is_some();
            // A disabled user's tokens stay valid too: Mastodon refuses them
            // only where `require_user!` runs (see `require_user`).
            let standing = if tok.user_id.is_none() {
                Standing::Functional
            } else if !tok.confirmed.unwrap_or(false) {
                Standing::Unconfirmed
            } else if !tok.approved.unwrap_or(false) {
                Standing::Pending
            } else if tok.disabled.unwrap_or(false) || tok.restricted.unwrap_or(false) {
                Standing::Disabled
            } else {
                Standing::Functional
            };

            // `Api::AccessTokenTrackingConcern`: when the token was last used,
            // and from where, at most once a day.
            if valid
                && tok
                    .last_used_at
                    .is_none_or(|at| at < chrono::Utc::now().naive_utc() - chrono::Duration::hours(24))
            {
                let ip = req
                    .extensions()
                    .get::<crate::remote_ip::ClientIp>()
                    .and_then(|c| c.0)
                    .map(|ip| ip.to_string());
                let _ = sqlx::query!(
                    "UPDATE oauth_access_tokens SET last_used_at = now(), last_used_ip = $1::text::inet WHERE id = $2",
                    ip,
                    tok.id,
                )
                .execute(&state.db)
                .await;
            }
            // `UserTrackingConcern#update_user_sign_in`, a before action of
            // every controller, the API's included: a user whose sign-in time
            // is a day old, or unset, is signed in again now.
            if let (true, Some(user_id)) = (valid, tok.user_id) {
                if tok.current_sign_in_at.is_none_or(|at| {
                    at < chrono::Utc::now().naive_utc() - SIGN_IN_UPDATE_FREQUENCY
                }) {
                    update_sign_in(&state, user_id, false).await;
                }
            }
            if valid && account_unavailable {
                req.extensions_mut().insert(UnavailableAccount);
            } else if let (true, None) = (valid, tok.account_id) {
                req.extensions_mut().insert(AppToken);
            } else if let (true, Some(account_id)) = (valid, tok.account_id) {
                let user = AuthenticatedUser {
                    account_id,
                    user_id: tok.user_id,
                    token_id: tok.id,
                    scopes: tok
                        .scopes
                        .as_deref()
                        .unwrap_or("read")
                        .split(|c: char| c.is_whitespace() || c == ',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .collect(),
                    application_id: tok.application_id,
                    standing,
                    user_disabled: tok.disabled.unwrap_or(false),
                };
                req.extensions_mut().insert(user);
            }
        }
    }
    next.run(req).await
}

/// `UserTrackingConcern::SIGN_IN_UPDATE_FREQUENCY`.
pub const SIGN_IN_UPDATE_FREQUENCY: chrono::Duration = chrono::Duration::hours(24);

/// `User#update_sign_in!(new_sign_in:)` and the `prepare_returning_user!` it
/// ends with: the sign-in time moves to now, the last one to what it was (or
/// now), the count goes up for a new sign-in, and a confirmed user counts
/// towards the day's logins (`ActivityTracker.record('activity:logins', id)`)
/// and, when the sign-in before this one was longer ago than
/// `User::ACTIVE_DURATION`, has the home feed regenerated
/// (`User#regenerate_feed!`, [`crate::home_feed::regenerate_feed`]).
pub async fn update_sign_in(state: &AppState, user_id: i64, new_sign_in: bool) {
    let row = match sqlx::query!(
        r#"UPDATE users SET last_sign_in_at = COALESCE(current_sign_in_at, now()),
                            current_sign_in_at = now(),
                            sign_in_count = sign_in_count + CASE WHEN $2 THEN 1 ELSE 0 END
           WHERE id = $1
           RETURNING account_id, confirmed_at IS NOT NULL AS "confirmed!",
                     last_sign_in_at < now() - make_interval(days => $3) AS "inactive!""#,
        user_id,
        new_sign_in,
        crate::home_feed::ACTIVE_DAYS,
    )
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(row)) if row.confirmed => row,
        Ok(_) => return,
        Err(error) => {
            tracing::warn!(user_id, %error, "could not record a sign-in");
            return;
        }
    };
    record_activity(state, "activity:logins", Some(user_id)).await;
    if row.inactive {
        crate::home_feed::regenerate_feed(state, row.account_id).await;
    }
}

/// `ActivityTracker::EXPIRE_AFTER`: six months, as ActiveSupport counts them.
const ACTIVITY_EXPIRE_AFTER: i64 = 15_778_476;

/// `ActivityTracker.record(prefix, value)` with a value, into the day's
/// HyperLogLog, and `ActivityTracker.increment(prefix)` without one, adding
/// one to the day's counter.
pub async fn record_activity(state: &AppState, prefix: &str, value: Option<i64>) {
    let today = chrono::Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
        .and_utc()
        .timestamp();
    let key = state.redis_keys.key(format!("{prefix}:{today}"));
    let mut redis = state.redis.clone();
    let mut pipe = redis::pipe();
    match value {
        Some(value) => pipe.cmd("PFADD").arg(&key).arg(value).ignore(),
        None => pipe.cmd("INCRBY").arg(&key).arg(1).ignore(),
    };
    let _: redis::RedisResult<()> = pipe
        .cmd("EXPIRE")
        .arg(&key)
        .arg(ACTIVITY_EXPIRE_AFTER)
        .ignore()
        .query_async(&mut redis)
        .await;
}

/// Log failed requests (4xx/5xx) with their method, path and status.
///
/// Never the body, nor the query string: a refused password grant, sign-up or
/// password reset carries the very credentials that were refused, and a
/// streaming URL can carry an access token.
pub async fn log_failures(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    // For a refused delivery: the key it claimed to be signed with, which
    // names the sender when nothing else about the request can be trusted,
    // and the headers its signature covered as they arrived here. The request
    // is passed on untouched: a sender may have signed any header it sent.
    let inbox_post = method == axum::http::Method::POST && path.ends_with("/inbox");
    let key_id = inbox_post.then(|| claimed_key_id(req.headers())).flatten();
    let covered = inbox_post.then(|| covered_headers(req.headers()));

    let response = next.run(req).await;
    let status = response.status();

    if status.is_client_error() || status.is_server_error() {
        tracing::warn!(
            method = %method,
            path = %path,
            status = %status,
            "request failed",
        );
    }

    // Why an inbox refused a delivery is in the body ojak answers with, and
    // nowhere else; without it a peer that cannot reach us is only a count.
    if inbox_post && status.is_client_error() {
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, 4096).await.unwrap_or_default();
        let reason = body_text(&parts, &bytes);
        tracing::warn!(
            path = %path,
            status = %status,
            key_id = key_id.as_deref().unwrap_or(""),
            reason = %reason,
            covered = covered.as_deref().unwrap_or(""),
            "inbox refused a delivery",
        );
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    }

    // A forwarded activity ojak could not establish is accepted and dropped,
    // as Mastodon drops it, with why in the body.
    if inbox_post && status == axum::http::StatusCode::ACCEPTED {
        let (parts, body) = response.into_parts();
        let bytes = axum::body::to_bytes(body, 4096).await.unwrap_or_default();
        if !bytes.is_empty() {
            tracing::debug!(
                path = %path,
                key_id = key_id.as_deref().unwrap_or(""),
                reason = %body_text(&parts, &bytes),
                "inbox dropped a delivery",
            );
        }
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    }

    response
}

/// A response body as text: gzipped if the sender asked for it, and decoded
/// here for the log only.
fn body_text(parts: &axum::http::response::Parts, bytes: &[u8]) -> String {
    let gzip = parts
        .headers
        .get(axum::http::header::CONTENT_ENCODING)
        .is_some_and(|encoding| encoding == "gzip");
    if gzip {
        use std::io::Read as _;
        let mut text = String::new();
        let _ = flate2::read::GzDecoder::new(bytes).read_to_string(&mut text);
        text
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// The headers a request's draft HTTP signature covers, each with the value it
/// arrived with: what a signature that does not verify was made over, as far
/// as this end can tell. The signature itself is left out.
fn covered_headers(headers: &axum::http::HeaderMap) -> String {
    let Some(signature) = headers.get("signature").and_then(|v| v.to_str().ok()) else {
        return String::new();
    };
    let marker = "headers=\"";
    let Some(start) = signature.find(marker).map(|i| i + marker.len()) else {
        return String::new();
    };
    let Some(len) = signature[start..].find('"') else {
        return String::new();
    };
    signature[start..start + len]
        .split_whitespace()
        .map(|name| {
            let value = headers.get(name).and_then(|v| v.to_str().ok());
            let value = match value {
                Some(value) => value,
                None if name.starts_with('(') => "…",
                None => "<absent>",
            };
            format!("{name}: {value}")
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// The `keyId` a request's HTTP signature names, in either scheme.
fn claimed_key_id(headers: &axum::http::HeaderMap) -> Option<String> {
    let field = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    // RFC 9421 names it in Signature-Input as keyid="…"; the draft in
    // Signature as keyId="…".
    let (value, marker) = match field("signature-input") {
        Some(input) => (input, "keyid=\""),
        None => (field("signature")?, "keyId=\""),
    };
    let start = value.find(marker)? + marker.len();
    let end = value[start..].find('"')?;
    Some(value[start..start + end].to_owned())
}

fn extract_bearer(req: &Request) -> Option<String> {
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    header.strip_prefix("Bearer ").map(str::to_string)
}

fn token_not_expired(created_at: chrono::NaiveDateTime, expires_in: Option<i32>) -> bool {
    expires_in
        .map(|seconds| {
            created_at + chrono::Duration::seconds(seconds as i64) > chrono::Utc::now().naive_utc()
        })
        .unwrap_or(true)
}

/// `Api::BaseController`'s `require_functional!` (in limited federation mode),
/// `require_authenticated_user!` (when unauthenticated access is disallowed),
/// `require_not_suspended!` and `require_user!`, in that order, for the
/// Mastodon API routes. Layered with `route_layer`, so the route a request
/// matched is known.
pub async fn api_gates(req: Request, next: Next) -> Response {
    let Some(path) = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str().to_owned())
    else {
        return next.run(req).await;
    };
    if !path.starts_with("/api/") {
        return next.run(req).await;
    }
    let (limited, disallow) = req
        .extensions()
        .get::<AppState>()
        .map(|state| {
            (
                state.instance.limited_federation_mode,
                state.instance.disallows_unauthenticated_api_access(),
            )
        })
        .unwrap_or_default();
    let unavailable = req.extensions().get::<UnavailableAccount>().is_some();
    let auth = req
        .extensions()
        .get::<AuthenticatedUser>()
        .filter(|a| a.user_id.is_some());
    // `ApplicationController#require_functional!`, which `Api::BaseController`
    // skips unless in limited federation mode: a signed-in user who cannot
    // use the account is refused everywhere, as `require_user!` refuses them.
    if limited {
        if unavailable {
            return AppError::ForbiddenMsg("Your login is currently disabled".into())
                .into_response();
        }
        if let Some(auth) = auth.filter(|a| a.standing != Standing::Functional) {
            if let Err(error) = require_user(Some(auth)) {
                return error.into_response();
            }
        }
    }
    // `require_authenticated_user!`, run when `disallow_unauthenticated_api_access?`
    // by every controller that does not skip it. A token with no user behind
    // it is no authenticated user.
    if disallow
        && auth.is_none()
        && !unavailable
        && !open_without_user(req.method(), &path, limited)
    {
        return AppError::UnauthorizedMsg("This method requires an authenticated user".into())
            .into_response();
    }
    if unavailable {
        return AppError::ForbiddenMsg("Your login is currently disabled".into()).into_response();
    }
    if requires_user(req.method(), &path) {
        // Without a token, the route's own `doorkeeper_authorize!` answers.
        let auth = req.extensions().get::<AuthenticatedUser>();
        if auth.is_some() || req.extensions().get::<AppToken>().is_some() {
            // `require_user!` ends in `update_user_sign_in`, which
            // [`authenticate`] already ran for every authenticated request.
            if let Err(error) = require_user(auth) {
                return error.into_response();
            }
        }
    }
    next.run(req).await
}

/// Whether the controller behind a route skips `require_authenticated_user!`,
/// so that it stays open to a request with no user even when
/// `disallow_unauthenticated_api_access?`. Some skip it only outside limited
/// federation mode.
pub fn open_without_user(method: &axum::http::Method, path: &str, limited: bool) -> bool {
    use axum::http::Method;
    match path {
        // `Api::V2::InstancesController` and its v1 subclass.
        "/api/v1/instance" | "/api/v2/instance" => true,
        // `Api::OEmbedController`.
        "/api/oembed" => true,
        // `Api::V1::AppsController` and `Api::V1::AccountsController#create`.
        "/api/v1/apps" | "/api/v1/accounts" => method == Method::POST,
        // `Api::V1::Peers::SearchController`, unless in limited federation mode.
        "/api/v1/peers/search" => !limited,
        // Eunha's sign-in helpers for Elk, which come before any token.
        "/api/{server}/login" | "/api/{server}/oauth/{origin}" => true,
        // `Api::V1::Instances::BaseController`, unless in limited federation mode.
        _ if path.starts_with("/api/v1/instance/") => !limited,
        _ => false,
    }
}

/// `Api::BaseController#require_user!`.
pub fn require_user(auth: Option<&AuthenticatedUser>) -> crate::error::AppResult<()> {
    let Some(auth) = auth.filter(|a| a.user_id.is_some()) else {
        return Err(AppError::Unprocessable(
            "This method requires an authenticated user".into(),
        ));
    };
    let refusal = match auth.standing {
        Standing::Functional => return Ok(()),
        Standing::Unconfirmed => "Your login is missing a confirmed e-mail address",
        Standing::Pending => "Your login is currently pending approval",
        Standing::Disabled => "Your login is currently disabled",
    };
    Err(AppError::ForbiddenMsg(refusal.into()))
}

/// Whether the Mastodon controller action behind a route runs
/// `require_user!`. The public and hashtag timelines run it only when their
/// feed is not public, which their handlers decide.
pub fn requires_user(method: &axum::http::Method, path: &str) -> bool {
    use axum::http::Method;
    let Some(rest) = path
        .strip_prefix("/api/v1/")
        .or_else(|| path.strip_prefix("/api/v2/"))
        .or_else(|| path.strip_prefix("/api/v1_alpha/"))
    else {
        return false;
    };
    let first = rest.split('/').next().unwrap_or("");
    // Controllers that run it for every action.
    if matches!(
        first,
        "announcements"
            | "annual_reports"
            | "async_refreshes"
            | "blocks"
            | "bookmarks"
            | "conversations"
            | "domain_blocks"
            | "donation_campaigns"
            | "endorsements"
            | "favourites"
            | "featured_tags"
            | "filters"
            | "filter_keywords"
            | "filter_statuses"
            | "follow_requests"
            | "followed_tags"
            | "lists"
            | "markers"
            | "media"
            | "mutes"
            | "notifications"
            | "preferences"
            | "push"
            | "reports"
            | "scheduled_statuses"
            | "suggestions"
    ) {
        return true;
    }
    match rest {
        "accounts/verify_credentials"
        | "accounts/update_credentials"
        | "accounts/search"
        | "accounts/relationships"
        | "accounts/familiar_followers"
        | "profile"
        | "profile/avatar"
        | "profile/header"
        | "timelines/home"
        | "timelines/list/{id}"
        | "polls/{id}/votes" => return true,
        // `Api::V1::StatusesController`: all but index and show.
        "statuses" => return method == Method::POST,
        "statuses/{id}" => return method != Method::GET,
        // `Api::V1::CollectionsController`: create, update and destroy.
        "collections" => return method == Method::POST,
        "collections/{id}" => return method != Method::GET,
        _ => {}
    }
    if rest.starts_with("collections/") {
        // `Api::V1::CollectionItemsController`.
        return true;
    }
    if let Some(action) = rest.strip_prefix("statuses/{id}/") {
        return matches!(
            action,
            "favourite"
                | "unfavourite"
                | "reblog"
                | "unreblog"
                | "bookmark"
                | "unbookmark"
                | "pin"
                | "unpin"
                | "mute"
                | "unmute"
                | "translate"
        );
    }
    if let Some(action) = rest.strip_prefix("accounts/{id}/") {
        // `Api::V1::AccountsController` but index, show and create, and the
        // per-account controllers that run it.
        return matches!(
            action,
            "follow"
                | "unfollow"
                | "remove_from_followers"
                | "block"
                | "unblock"
                | "mute"
                | "unmute"
                | "note"
                | "lists"
                | "identity_proofs"
                | "in_collections"
                | "endorse"
                | "unendorse"
                | "pin"
                | "unpin"
        );
    }
    // `Api::V1::TagsController` but show.
    if let Some(action) = rest.strip_prefix("tags/{name}/") {
        return matches!(action, "follow" | "unfollow" | "feature" | "unfeature");
    }
    false
}

#[cfg(test)]
mod claimed_key_tests {
    use super::claimed_key_id;
    use axum::http::HeaderMap;

    #[test]
    fn reads_the_key_either_scheme_names() {
        let mut draft = HeaderMap::new();
        draft.insert(
            "signature",
            r#"keyId="https://a.example/users/bob#main-key",algorithm="rsa-sha256",headers="(request-target) host date digest",signature="abc""#
                .parse()
                .unwrap(),
        );
        assert_eq!(
            claimed_key_id(&draft).as_deref(),
            Some("https://a.example/users/bob#main-key")
        );

        let mut rfc9421 = HeaderMap::new();
        rfc9421.insert(
            "signature-input",
            r#"sig1=("@method" "@target-uri");created=1;keyid="https://b.example/actor#key";alg="rsa-v1_5-sha256""#
                .parse()
                .unwrap(),
        );
        rfc9421.insert("signature", "sig1=:abc:".parse().unwrap());
        assert_eq!(
            claimed_key_id(&rfc9421).as_deref(),
            Some("https://b.example/actor#key")
        );

        assert_eq!(claimed_key_id(&HeaderMap::new()), None);
    }

    #[test]
    fn lists_what_the_signature_covered_as_it_arrived() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "signature",
            r#"keyId="https://a.example/@bob#main-key",algorithm="rsa-sha256",headers="(request-target) host date accept-encoding",signature="abc""#
                .parse()
                .unwrap(),
        );
        headers.insert("host", "seoul.earth".parse().unwrap());
        headers.insert("date", "Mon, 28 Sep 2026 10:00:00 GMT".parse().unwrap());
        assert_eq!(
            super::covered_headers(&headers),
            "(request-target): … | host: seoul.earth | date: Mon, 28 Sep 2026 10:00:00 GMT | accept-encoding: <absent>"
        );
        assert_eq!(super::covered_headers(&HeaderMap::new()), "");
    }
}
