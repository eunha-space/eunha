use axum::{
    extract::{Extension, Query},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use super::extractors::Params;

use super::types::{AppCredentials, CredentialApplication};
use crate::{
    db::models::OauthApplication,
    error::{AppError, AppResult},
    middleware::ResolvedInstance,
    state::AppState,
};

// ── GET /api/v1/apps/verify_credentials ───────────────────────────────────

pub async fn verify_app_credentials(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    bearer: Option<Extension<crate::middleware::BearerToken>>,
) -> AppResult<Json<AppCredentials>> {
    let token = bearer
        .as_ref()
        .map(|Extension(t)| t.0.as_str())
        .ok_or(AppError::Unauthorized)?;

    let row = sqlx::query!(
        r#"SELECT a.id, a.name, a.website, COALESCE(a.scopes, 'read') as "scopes!", a.redirect_uri
           FROM oauth_access_tokens t
           JOIN oauth_applications a ON a.id = t.application_id
           WHERE t.token = $1 AND t.revoked_at IS NULL
             AND (t.expires_in IS NULL OR t.created_at + t.expires_in * interval '1 second' > now())"#,
        token,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::Unauthorized)?;

    let uris: Vec<String> = row.redirect_uri.lines().map(str::to_owned).collect();
    let redirect_uri = uris
        .first()
        .cloned()
        .unwrap_or_else(|| row.redirect_uri.clone());
    Ok(Json(AppCredentials {
        id: row.id.to_string(),
        name: row.name,
        website: row.website,
        scopes: normalize_scopes(&row.scopes)
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
        redirect_uri,
        redirect_uris: uris,
        vapid_key: Some(instance.vapid_public_key.clone()),
    }))
}

// ── POST /api/v1/apps ──────────────────────────────────────────────────────

/// `redirect_uris`, a string of lines or an array of them.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum RedirectUris {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
pub struct RegisterAppForm {
    pub client_name: Option<String>,
    pub redirect_uris: Option<RedirectUris>,
    pub scopes: Option<String>,
    pub website: Option<String>,
}

/// `ApplicationExtension::APP_NAME_LIMIT`.
const APP_NAME_LIMIT: usize = 60;
/// `APP_REDIRECT_URI_LIMIT` and `APP_WEBSITE_LIMIT`.
const APP_URI_LIMIT: usize = 2_000;

/// What `Doorkeeper::RedirectUriValidator` makes of one redirect URI, with
/// Mastodon's `forbid_redirect_uri` and `force_ssl_in_redirect_uri false`:
/// the errors, in its order, or `invalid_uri` alone when Ruby's `URI.parse`
/// would raise.
fn redirect_uri_errors(uri: &str) -> Result<Vec<&'static str>, ()> {
    // `URI::RFC3986_Parser`: no spaces, controls, non-ASCII or the
    // characters it never allows.
    if uri
        .chars()
        .any(|c| !c.is_ascii_graphic() || "<>\"{}|\\^`".contains(c))
        || uri.matches('#').count() > 1
    {
        return Err(());
    }
    let scheme_end = uri.find(':').filter(|&i| {
        let scheme = &uri[..i];
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    let (scheme, rest) = match scheme_end {
        Some(i) => (Some(uri[..i].to_ascii_lowercase()), &uri[i + 1..]),
        None => (None, uri),
    };
    let (rest, fragment) = match rest.split_once('#') {
        Some((rest, fragment)) => (rest, Some(fragment)),
        None => (rest, None),
    };
    let host = rest.strip_prefix("//").map(|authority| {
        let authority = authority.split(['/', '?']).next().unwrap_or("");
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        if host.starts_with('[') {
            host.split_inclusive(']').next().unwrap_or("").to_owned()
        } else {
            host.split(':').next().unwrap_or("").to_owned()
        }
    });
    let opaque = scheme.is_some() && !rest.is_empty() && !rest.starts_with('/');
    let mut errors = Vec::new();
    if matches!(scheme.as_deref(), Some("data" | "vbscript" | "javascript")) {
        errors.push("is forbidden by the server.");
    }
    if fragment.is_some() {
        errors.push("cannot contain a fragment.");
    }
    if opaque || scheme.as_deref() == Some("localhost") {
        errors.push("must specify a scheme.");
    }
    if scheme.is_none() && host.as_deref().unwrap_or("").is_empty() {
        errors.push("must be an absolute URI.");
    }
    if matches!(scheme.as_deref(), Some("http" | "https"))
        && host.as_deref().unwrap_or("").is_empty()
    {
        errors.push("must be a valid URI.");
    }
    Ok(errors)
}

/// `URLValidator`, for the website: `http` or `https`, with a host.
fn website_valid(website: &str) -> bool {
    url::Url::parse(website)
        .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
}

/// `Doorkeeper::Application.create!`'s validations, Doorkeeper's then
/// `ApplicationExtension`'s, as `RecordInvalid` lists them.
fn application_errors(name: &str, redirect_uri: &str, scopes: &str, website: &str) -> Vec<String> {
    let mut errors = Vec::new();
    if name.trim().is_empty() {
        errors.push("Application name can't be blank".to_owned());
    }
    // `RedirectUriValidator`; `allow_blank_redirect_uri` is false while the
    // authorization code flow is on.
    if redirect_uri.trim().is_empty() {
        errors.push("Redirect URI can't be blank".to_owned());
    } else {
        let mut uri_errors = Vec::new();
        for uri in redirect_uri.split_whitespace() {
            if uri == OOB_REDIRECT_URI || uri == "urn:ietf:wg:oauth:2.0:oob:auto" {
                continue;
            }
            match redirect_uri_errors(uri) {
                Ok(found) => uri_errors.extend(found),
                Err(()) => {
                    uri_errors.push("must be a valid URI.");
                    break;
                }
            }
        }
        errors.extend(uri_errors.into_iter().map(|e| format!("Redirect URI {e}")));
    }
    // `enforce_configured_scopes`: `scopes_match_configured`.
    if !scopes.trim().is_empty()
        && (scopes.contains(['\n', '\r', '\t'])
            || !scopes
                .split_whitespace()
                .all(|s| VALID_OAUTH_SCOPES.contains(&s)))
    {
        errors.push("Scopes doesn't match those configured on the server.".to_owned());
    }
    if name.chars().count() > APP_NAME_LIMIT {
        errors.push(format!(
            "Application name is too long (maximum is {APP_NAME_LIMIT} characters)"
        ));
    }
    if redirect_uri.chars().count() > APP_URI_LIMIT {
        errors.push(format!(
            "Redirect URI is too long (maximum is {APP_URI_LIMIT} characters)"
        ));
    }
    if !website.trim().is_empty() {
        if !website_valid(website) {
            errors.push("Application website is not a valid URL".to_owned());
        }
        if website.chars().count() > APP_URI_LIMIT {
            errors.push(format!(
                "Application website is too long (maximum is {APP_URI_LIMIT} characters)"
            ));
        }
    }
    errors
}

/// `Api::V1::AppsController#create`: `Doorkeeper::Application.create!`,
/// whose validations answer `422` with `Validation failed: …`.
pub async fn register_app(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Params(form): Params<RegisterAppForm>,
) -> AppResult<Json<CredentialApplication>> {
    let client_id = generate_token(32);
    let client_secret = generate_token(64);
    // `redirect_uri=`: an array is joined a line each.
    let redirect_uris = match form.redirect_uris {
        Some(RedirectUris::One(uris)) => uris,
        Some(RedirectUris::Many(uris)) => uris.join("\n"),
        None => String::new(),
    };
    // `app_scopes_or_default`, stored as `Scopes#to_s`: each scope once.
    let mut scope_list: Vec<&str> = Vec::new();
    let requested = form.scopes.unwrap_or_else(|| DEFAULT_SCOPES.to_owned());
    for scope in requested.split_whitespace() {
        if !scope_list.contains(&scope) {
            scope_list.push(scope);
        }
    }
    let scopes = scope_list.join(" ");
    let name = form.client_name.unwrap_or_default();
    let website = form.website.filter(|w| !w.is_empty());
    let errors = application_errors(
        &name,
        &redirect_uris,
        &requested,
        website.as_deref().unwrap_or(""),
    );
    if !errors.is_empty() {
        return Err(AppError::Unprocessable(format!(
            "Validation failed: {}",
            errors.join(", ")
        )));
    }

    let app = sqlx::query_as!(
        OauthApplication,
        r#"INSERT INTO oauth_applications
             (name, uid, secret, redirect_uri, scopes, website, created_at, updated_at)
           VALUES ($1,$2,$3,$4,$5,$6,now(),now())
           RETURNING *"#,
        name,
        client_id,
        client_secret,
        redirect_uris,
        scopes,
        website,
    )
    .fetch_one(&state.db)
    .await?;

    Ok(Json(app_to_credential(&app, &instance.vapid_public_key)))
}

fn app_to_credential(app: &OauthApplication, vapid_key: &str) -> CredentialApplication {
    let uris: Vec<String> = app.redirect_uri.lines().map(str::to_owned).collect();
    let redirect_uri = uris
        .first()
        .cloned()
        .unwrap_or_else(|| app.redirect_uri.clone());
    CredentialApplication {
        id: app.id.to_string(),
        name: app.name.clone(),
        website: app.website.clone(),
        scopes: normalize_scopes(app.scopes.as_deref().unwrap_or("read"))
            .split_whitespace()
            .map(str::to_owned)
            .collect(),
        redirect_uri,
        redirect_uris: uris,
        client_id: app.uid.clone(),
        client_secret: app.secret.clone(),
        client_secret_expires_at: 0,
        vapid_key: Some(vapid_key.to_string()),
    }
}

// ── POST /oauth/token ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct TokenRequest {
    pub grant_type: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub redirect_uri: Option<String>,
    pub code: Option<String>,
    pub scope: Option<String>,
    pub code_verifier: Option<String>,
}

/// A Doorkeeper error answer, `{"error":…,"error_description":…}` with a
/// `400`.
fn oauth_error(error: &str, description: &str) -> Response {
    crate::api::mastodon::oauth_client::error_response(StatusCode::BAD_REQUEST, error, description)
}

/// `InvalidRequestResponse` for a missing parameter.
fn missing_param(name: &str) -> Response {
    oauth_error(
        "invalid_request",
        &format!("Missing required parameter: {name}."),
    )
}

/// `AccessGrant.generate_code_challenge`: the `S256` challenge of a verifier.
fn s256_challenge(verifier: &str) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier))
}

/// `Doorkeeper.configuration.default_scopes`.
const DEFAULT_SCOPES: &str = "read";

/// `Scopes.from_string`: the scopes of a string, each once.
fn scope_set(scopes: &str) -> std::collections::BTreeSet<&str> {
    scopes.split_whitespace().collect()
}

/// `Helpers::ScopeChecker.valid?`: present, on one line, and every scope
/// among the application's, or the server's when it has none.
fn scopes_valid(scope_str: &str, app_scopes: Option<&str>) -> bool {
    let app_scopes = app_scopes.filter(|s| !s.trim().is_empty());
    !scope_str.trim().is_empty()
        && !scope_str.contains(['\n', '\r', '\t'])
        && scope_str.split_whitespace().all(|scope| match app_scopes {
            Some(app) => app.split_whitespace().any(|a| a == scope),
            None => VALID_OAUTH_SCOPES.contains(&scope),
        })
}

/// `AccessToken.find_or_create_for` with Mastodon's `reuse_access_token`: the
/// newest unrevoked, unexpired token of the application for the owner with
/// the same scopes (`matching_token_for`, `include_expired: false`), which
/// never expires and so is always `reusable?`, or else a new one.
async fn find_or_create_token(
    conn: &mut sqlx::PgConnection,
    application_id: i64,
    resource_owner_id: Option<i64>,
    scopes: &str,
) -> sqlx::Result<(String, String, chrono::NaiveDateTime)> {
    let wanted = scope_set(scopes);
    let held = sqlx::query!(
        r#"SELECT token, COALESCE(scopes, '') AS "scopes!", created_at
           FROM oauth_access_tokens
           WHERE application_id = $1 AND resource_owner_id IS NOT DISTINCT FROM $2
             AND revoked_at IS NULL
             AND (expires_in IS NULL OR created_at + expires_in * interval '1 second' > now())
           ORDER BY created_at DESC, id DESC"#,
        application_id,
        resource_owner_id,
    )
    .fetch_all(&mut *conn)
    .await?;
    if let Some(token) = held.into_iter().find(|t| scope_set(&t.scopes) == wanted) {
        return Ok((token.token, token.scopes, token.created_at));
    }
    let token = generate_token(64);
    let created_at = sqlx::query_scalar!(
        r#"INSERT INTO oauth_access_tokens (application_id, resource_owner_id, token, scopes, created_at)
           VALUES ($1, $2, $3, $4, now())
           RETURNING created_at"#,
        application_id,
        resource_owner_id,
        token,
        scopes,
    )
    .fetch_one(&mut *conn)
    .await?;
    Ok((token, scopes.to_owned(), created_at))
}

/// `TokenResponse`: the token, with its blank fields left out
/// (`expires_in` and `refresh_token`, which Mastodon never sets), and the
/// headers that keep it out of caches.
fn token_response(token: String, scopes: String, created_at: chrono::NaiveDateTime) -> Response {
    let mut body = serde_json::json!({
        "access_token": token,
        "token_type": "Bearer",
        "created_at": created_at.and_utc().timestamp(),
    });
    if !scopes.trim().is_empty() {
        body["scope"] = scopes.into();
    }
    (
        [
            (axum::http::header::CACHE_CONTROL, "no-store, no-cache"),
            (axum::http::header::PRAGMA, "no-cache"),
        ],
        Json(body),
    )
        .into_response()
}

/// `Doorkeeper::TokensController#create`, with Mastodon's configuration:
/// the `authorization_code` and `client_credentials` grant flows, tokens
/// that never expire (`access_token_expires_in nil`), no refresh tokens, and
/// `reuse_access_token`.
pub async fn issue_token(
    state: AppState,
    headers: axum::http::HeaderMap,
    Params(form): Params<TokenRequest>,
) -> AppResult<Response> {
    use crate::api::mastodon::oauth_client;

    // `Request.token_strategy`.
    let grant_type = form.grant_type.as_deref().unwrap_or("");
    if grant_type.trim().is_empty() {
        return Ok(missing_param("grant_type"));
    }
    if !matches!(grant_type, "authorization_code" | "client_credentials") {
        return Ok(oauth_error(
            "unsupported_grant_type",
            "The authorization grant type is not supported by the authorization server.",
        ));
    }
    // `Request::AuthorizationCode#grant` asks for the code before the client
    // is looked at.
    let code = form.code.as_deref().filter(|c| !c.trim().is_empty());
    if grant_type == "authorization_code" && code.is_none() {
        return Ok(missing_param("code"));
    }
    // `server.client`.
    let credentials = match oauth_client::from_request(
        &headers,
        form.client_id.as_deref(),
        form.client_secret.as_deref(),
    ) {
        Ok(credentials) => credentials,
        Err(_) => return Ok(oauth_client::multiple_methods_response()),
    };
    let client = match &credentials {
        Some(credentials) => oauth_client::authenticate(&state, credentials).await?,
        None => None,
    };

    if grant_type == "client_credentials" {
        // `ClientCredentials::Validator`.
        let Some(app) = client else {
            return Ok(oauth_client::invalid_client_response());
        };
        let scopes = match form.scope.as_deref().filter(|s| !s.is_empty()) {
            Some(scope) => scope.to_owned(),
            // `build_scopes`: the default scopes the application has, or
            // the default scopes when it has none.
            None => match app.scopes.as_deref().filter(|s| !s.trim().is_empty()) {
                None => DEFAULT_SCOPES.to_owned(),
                Some(app_scopes) => scope_set(app_scopes)
                    .into_iter()
                    .filter(|s| scope_set(DEFAULT_SCOPES).contains(s))
                    .collect::<Vec<_>>()
                    .join(" "),
            },
        };
        let app_scopes = app.scopes.as_deref().filter(|s| !s.trim().is_empty());
        if !(scopes.trim().is_empty() && app_scopes.is_none()) && !scopes_valid(&scopes, app_scopes)
        {
            return Ok(oauth_error(
                "invalid_scope",
                "The requested scope is invalid, unknown, or malformed.",
            ));
        }
        let (token, scopes, created_at) =
            find_or_create_token(&mut *state.db.acquire().await?, app.id, None, &scopes).await?;
        return Ok(token_response(token, scopes, created_at));
    }

    // `AuthorizationCodeRequest`.
    let code = code.unwrap_or_default();
    let grant = sqlx::query!(
        r#"SELECT id, application_id, redirect_uri, code_challenge, code_challenge_method,
                  revoked_at IS NULL AND created_at + expires_in * interval '1 second' > now()
                  AS "accessible!"
           FROM oauth_access_grants WHERE token = $1"#,
        code,
    )
    .fetch_optional(&state.db)
    .await?;
    let verifier = form
        .code_verifier
        .as_deref()
        .filter(|v| !v.trim().is_empty());
    let challenge = grant
        .as_ref()
        .and_then(|g| g.code_challenge.clone())
        .filter(|c| !c.is_empty());
    let challenge = challenge.as_deref();
    // `validate_params`.
    if challenge.is_some() && verifier.is_none() {
        return Ok(missing_param("code_verifier"));
    }
    let redirect_uri = form.redirect_uri.as_deref().unwrap_or("");
    if redirect_uri.trim().is_empty() {
        return Ok(missing_param("redirect_uri"));
    }
    // `validate_client`.
    let Some(app) = client else {
        return Ok(oauth_client::invalid_client_response());
    };
    // `validate_grant`, `validate_redirect_uri`, `validate_code_verifier`.
    let Some(grant) = grant.filter(|g| g.application_id == app.id && g.accessible) else {
        return Ok(oauth_error("invalid_grant", INVALID_GRANT));
    };
    if !redirect_uri_allowed(redirect_uri, &grant.redirect_uri) {
        return Ok(oauth_error("invalid_grant", INVALID_GRANT));
    }
    let verified = match verifier {
        None => challenge.is_none(),
        Some(verifier) => match grant.code_challenge_method.as_deref() {
            Some("S256") => challenge == Some(s256_challenge(verifier).as_str()),
            Some("plain") => challenge == Some(verifier),
            _ => false,
        },
    };
    if !verified {
        return Ok(oauth_error("invalid_grant", INVALID_GRANT));
    }

    // `before_successful_response`: the grant locked, refused if it was
    // revoked meanwhile (`InvalidGrantReuse`), revoked, and a token found or
    // made for its owner and scopes.
    let mut tx = state.db.begin().await?;
    let locked = sqlx::query!(
        r#"SELECT resource_owner_id, COALESCE(scopes, '') AS "scopes!", revoked_at IS NOT NULL AS "revoked!"
           FROM oauth_access_grants WHERE id = $1 FOR UPDATE"#,
        grant.id,
    )
    .fetch_one(&mut *tx)
    .await?;
    if locked.revoked {
        return Ok(oauth_error("invalid_grant", INVALID_GRANT));
    }
    sqlx::query!(
        "UPDATE oauth_access_grants SET revoked_at = now() WHERE id = $1",
        grant.id
    )
    .execute(&mut *tx)
    .await?;
    let (token, scopes, created_at) = find_or_create_token(
        &mut tx,
        app.id,
        Some(locked.resource_owner_id),
        &locked.scopes,
    )
    .await?;
    tx.commit().await?;
    Ok(token_response(token, scopes, created_at))
}

// ── POST /oauth/revoke ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RevokeRequest {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub token: Option<String>,
    pub token_type_hint: Option<String>,
}

/// `Oauth::TokensController#revoke`, Doorkeeper's `TokensController#revoke`
/// beneath it (RFC 7009).
///
/// `validate_presence_of_client` comes first: the request names a client,
/// with its secret unless it is a public one (`by_uid_and_secret`), or it
/// is refused with `403 unauthorized_client`. A token nobody holds is a
/// `200`; one issued to another client is refused, `403` again; a token
/// issued to no client may be revoked by any.
pub async fn revoke_token(
    state: AppState,
    headers: axum::http::HeaderMap,
    Params(form): Params<RevokeRequest>,
) -> AppResult<Response> {
    use crate::api::mastodon::oauth_client;

    let refused = || {
        (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "unauthorized_client",
                "error_description": "You are not authorized to revoke this token",
            })),
        )
            .into_response()
    };
    let credentials = match oauth_client::from_request(
        &headers,
        form.client_id.as_deref(),
        form.client_secret.as_deref(),
    ) {
        Ok(credentials) => credentials,
        Err(_) => return Ok(oauth_client::multiple_methods_response()),
    };
    let client = match &credentials {
        Some(credentials) => oauth_client::authenticate(&state, credentials).await?,
        None => None,
    };
    let Some(client) = client else {
        return Ok(refused());
    };
    let client_id = client.id;

    // `revocable_token`: the access token, then the refresh token, unless
    // the hint says it is a refresh token. Revoked and expired tokens are
    // found too.
    let token = form.token.as_deref().unwrap_or("");
    let by_access = form.token_type_hint.as_deref() != Some("refresh_token");
    let found = sqlx::query!(
        r#"SELECT id, application_id,
                  (revoked_at IS NULL OR revoked_at > now())
                  AND (expires_in IS NULL OR created_at + expires_in * interval '1 second' > now())
                  AS "accessible!"
           FROM oauth_access_tokens
           WHERE ($2 AND token = $1) OR refresh_token = $1
           ORDER BY ($2 AND token = $1) DESC
           LIMIT 1"#,
        token,
        by_access,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(found) = found else {
        return Ok(Json(serde_json::json!({})).into_response());
    };
    // `authorized?`.
    if found
        .application_id
        .is_some_and(|application_id| application_id != client_id)
    {
        return Ok(refused());
    }
    if found.accessible {
        sqlx::query!(
            "UPDATE oauth_access_tokens SET revoked_at = now() WHERE id = $1",
            found.id
        )
        .execute(&state.db)
        .await?;
        // `Oauth::TokensController#unsubscribe_for_token`, and the token's
        // streams closed (`AccessTokenExtension#push_to_streaming_api`).
        sqlx::query!(
            "DELETE FROM web_push_subscriptions WHERE access_token_id = $1",
            found.id
        )
        .execute(&state.db)
        .await?;
        crate::sessions::kill_streams(&state, vec![found.id]).await;
    }
    Ok(Json(serde_json::json!({})).into_response())
}

/// Normalize an OAuth scope string: split on whitespace or commas, deduplicate,
/// and rejoin with spaces. Ensures "read,write" and "read write" are equivalent.
fn normalize_scopes(s: &str) -> String {
    s.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The scopes Mastodon's Doorkeeper accepts (default + optional_scopes). App
/// registration with any scope outside this set is rejected
/// (`enforce_configured_scopes`).
const VALID_OAUTH_SCOPES: &[&str] = &[
    "read",
    "write",
    "follow",
    "push",
    "profile",
    "read:accounts",
    "read:blocks",
    "read:bookmarks",
    "read:collections",
    "read:favourites",
    "read:filters",
    "read:follows",
    "read:lists",
    "read:mutes",
    "read:notifications",
    "read:search",
    "read:statuses",
    "write:accounts",
    "write:blocks",
    "write:bookmarks",
    "write:collections",
    "write:conversations",
    "write:favourites",
    "write:filters",
    "write:follows",
    "write:lists",
    "write:media",
    "write:mutes",
    "write:notifications",
    "write:reports",
    "write:statuses",
    "admin:read",
    "admin:read:accounts",
    "admin:read:reports",
    "admin:read:domain_allows",
    "admin:read:domain_blocks",
    "admin:read:ip_blocks",
    "admin:read:email_domain_blocks",
    "admin:read:canonical_email_blocks",
    "admin:write",
    "admin:write:accounts",
    "admin:write:reports",
    "admin:write:domain_allows",
    "admin:write:domain_blocks",
    "admin:write:ip_blocks",
    "admin:write:email_domain_blocks",
    "admin:write:canonical_email_blocks",
];

/// Whether every requested scope is within the granted (app) scope set.
/// Doorkeeper rejects an authorization requesting scopes the app didn't register.
fn scope_is_subset(requested: &str, granted: &str) -> bool {
    let granted_set: std::collections::HashSet<&str> =
        granted.split(' ').filter(|s| !s.is_empty()).collect();
    requested
        .split(' ')
        .filter(|s| !s.is_empty())
        .all(|s| granted_set.contains(s))
}

fn generate_token(len: usize) -> String {
    use rand::RngCore;
    let mut rng = rand::rng();
    (0..len)
        .map(|_| format!("{:02x}", rng.next_u32() as u8))
        .collect()
}

// ── POST /api/:server/login  (Elk single-instance sign-in hook) ────────────

/// Elk sends JSON; a form is read the same way.
#[derive(Debug, Deserialize)]
pub struct ElkLoginBody {
    #[serde(default, deserialize_with = "super::extractors::rails::opt_bool")]
    pub force_login: Option<bool>,
    #[serde(deserialize_with = "super::extractors::rails::string")]
    pub origin: String,
    #[serde(default, deserialize_with = "super::extractors::rails::opt_string")]
    pub lang: Option<String>,
}

/// Build the redirect_uri Elk expects: `{origin}/api/{server}/oauth/{encoded_origin}`.
/// This matches Elk's `getRedirectURI(origin, server)` in server/utils/shared.ts.
fn elk_redirect_uri(origin: &str, server: &str) -> String {
    let origin = origin.trim_end_matches('/');
    format!(
        "{}/api/{}/oauth/{}",
        origin,
        server,
        urlencoding::encode(origin)
    )
}

pub async fn elk_login(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    super::extractors::Params(body): super::extractors::Params<ElkLoginBody>,
) -> AppResult<Json<String>> {
    let redirect_uri = elk_redirect_uri(&body.origin, &instance.domain);
    let scopes = "read write follow push";

    // Find or create a stable "Elk" OAuth app for this instance, keeping
    // redirect_uri in sync with the current origin.
    let existing = sqlx::query_as!(
        OauthApplication,
        "SELECT * FROM oauth_applications WHERE name = 'Elk' LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await?;

    let app = match existing {
        Some(a) if a.redirect_uri == redirect_uri => a,
        Some(a) => {
            // Origin changed (or old entry used /signin/callback) — update in place.
            sqlx::query!(
                "UPDATE oauth_applications SET redirect_uri = $1 WHERE id = $2",
                redirect_uri,
                a.id,
            )
            .execute(&state.db)
            .await?;
            OauthApplication {
                redirect_uri: redirect_uri.clone(),
                ..a
            }
        }
        None => {
            let client_id = generate_token(32);
            let client_secret = generate_token(64);
            sqlx::query_as!(
                OauthApplication,
                r#"INSERT INTO oauth_applications
                     (name, uid, secret, redirect_uri, scopes, created_at, updated_at)
                   VALUES ('Elk', $1, $2, $3, $4, now(), now())
                   RETURNING *"#,
                client_id,
                client_secret,
                redirect_uri,
                scopes,
            )
            .fetch_one(&state.db)
            .await?
        }
    };

    let force = body.force_login.unwrap_or(false);
    let lang = body.lang.unwrap_or_default();
    let encoded_redirect = urlencoding::encode(&redirect_uri);
    let encoded_scope = urlencoding::encode(scopes);

    let mut url = format!(
        "https://{}/oauth/authorize?client_id={}&redirect_uri={}&response_type=code&scope={}",
        instance.domain, app.uid, encoded_redirect, encoded_scope,
    );
    if force {
        url.push_str("&force_login=true");
    }
    if !lang.is_empty() {
        url.push_str(&format!("&lang={}", urlencoding::encode(&lang)));
    }

    Ok(Json(url))
}

// ── GET /api/:server/oauth/:origin  (Elk OAuth callback — server route) ───

#[derive(Debug, Deserialize)]
pub struct ElkOAuthCallbackQuery {
    pub code: Option<String>,
}

pub async fn elk_oauth_callback(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    axum::extract::Path((_server, encoded_origin)): axum::extract::Path<(String, String)>,
    Query(q): Query<ElkOAuthCallbackQuery>,
) -> Response {
    let origin = urlencoding::decode(&encoded_origin)
        .map(|s| s.into_owned())
        .unwrap_or_default();

    let code = match q.code {
        Some(c) => c,
        None => {
            tracing::warn!("elk_oauth_callback: missing code");
            return Redirect::to(&format!("{}/signin/callback?error=missing_code", origin))
                .into_response();
        }
    };

    let app = match sqlx::query_as!(
        OauthApplication,
        "SELECT * FROM oauth_applications WHERE name = 'Elk' LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    {
        Some(a) => a,
        None => {
            tracing::warn!(
                "elk_oauth_callback: no Elk app found for {}",
                instance.domain
            );
            return Redirect::to(&format!("{}/signin/callback?error=no_app", origin))
                .into_response();
        }
    };

    let code_row = sqlx::query!(
        r#"DELETE FROM oauth_access_grants
           WHERE token = $1 AND application_id = $2
             AND created_at + expires_in * interval '1 second' > now()
           RETURNING resource_owner_id, scopes"#,
        code,
        app.id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let Some(code_row) = code_row else {
        tracing::warn!("elk_oauth_callback: code not found or expired");
        return Redirect::to(&format!("{}/signin/callback?error=invalid_code", origin))
            .into_response();
    };

    let account_id = sqlx::query_scalar!(
        "SELECT account_id FROM users WHERE id = $1",
        code_row.resource_owner_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    let Some(_account_id) = account_id else {
        tracing::warn!("elk_oauth_callback: resource_owner has no account");
        return Redirect::to(&format!("{}/signin/callback?error=no_account", origin))
            .into_response();
    };

    let token_str = generate_token(64);
    let db_ok = sqlx::query!(
        r#"INSERT INTO oauth_access_tokens (application_id, resource_owner_id, token, scopes, created_at)
           VALUES ($1, $2, $3, $4, now())"#,
        app.id,
        code_row.resource_owner_id,
        token_str,
        code_row.scopes.as_deref().unwrap_or("read"),
    )
    .execute(&state.db)
    .await
    .is_ok();

    if !db_ok {
        tracing::error!("elk_oauth_callback: failed to insert access token");
        return Redirect::to(&format!("{}/signin/callback?error=db_error", origin)).into_response();
    }

    tracing::info!(
        instance = %instance.domain,
        "elk_oauth_callback: issued token, redirecting to signin/callback"
    );
    let redirect = format!(
        "{}/signin/callback?server={}&token={}",
        origin.trim_end_matches('/'),
        instance.domain,
        token_str,
    );
    Redirect::to(&redirect).into_response()
}

/// Doorkeeper's `URIChecker.matches?`: the same URI as one registered,
/// whatever extra query the request adds, unless the registered URI has a
/// query of its own, which must then be the request's exactly.
fn redirect_uri_matches(url: &str, registered: &str) -> bool {
    if url == registered {
        return true;
    }
    let (Ok(mut url), Ok(mut registered)) = (url::Url::parse(url), url::Url::parse(registered))
    else {
        return false;
    };
    if registered.query().is_some() {
        let pairs = |u: &url::Url| {
            let mut pairs: Vec<(String, String)> = u.query_pairs().into_owned().collect();
            pairs.sort();
            pairs
        };
        if pairs(&url) != pairs(&registered) {
            return false;
        }
        registered.set_query(None);
    }
    url.set_query(None);
    url == registered
}

/// Doorkeeper's `URIChecker.valid_for_authorization?` with Mastodon's
/// `forbid_redirect_uri`: a URI that matches one of the application's
/// registered redirect URIs, one per line, and is not a `data:`,
/// `javascript:` or `vbscript:` URI.
fn redirect_uri_allowed(url: &str, registered: &str) -> bool {
    let forbidden = url::Url::parse(url).map_or(true, |u| {
        matches!(
            u.scheme().to_ascii_lowercase().as_str(),
            "data" | "javascript" | "vbscript"
        )
    });
    !forbidden
        && registered
            .split_whitespace()
            .any(|candidate| redirect_uri_matches(url, candidate))
}

/// Doorkeeper's `invalid_redirect_uri`.
const INVALID_REDIRECT_URI: &str = "The redirect uri included is not valid.";

/// Doorkeeper's `invalid_grant`.
const INVALID_GRANT: &str = "The provided authorization grant is invalid, expired, revoked, does not match the redirection URI used in the authorization request, or was issued to another client.";

/// Doorkeeper's `native_redirect_uri`, the out-of-band redirect.
const OOB_REDIRECT_URI: &str = "urn:ietf:wg:oauth:2.0:oob";

/// What an authorization request carries besides its client, redirect URI and
/// scope, kept through the sign-in to the grant: `PreAuthorization`'s
/// `state`, `code_challenge`, `code_challenge_method` and `response_mode`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthorizationExtras {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub code_challenge: Option<String>,
    #[serde(default)]
    pub code_challenge_method: Option<String>,
    #[serde(default)]
    pub response_mode: Option<String>,
    /// Mastodon's own: when true, the authorization page asks even though a
    /// token would let it answer at once (`can_authorize_response?`).
    #[serde(default)]
    pub force_login: Option<String>,
}

impl AuthorizationExtras {
    fn present(value: &Option<String>) -> Option<&str> {
        value.as_deref().filter(|v| !v.is_empty())
    }

    /// `truthy_param?('force_login')`: `ActiveModel::Type::Boolean` casts
    /// anything but a blank or one of its false values to true.
    fn force_login(&self) -> bool {
        Self::present(&self.force_login).is_some_and(|value| {
            !matches!(value, "0" | "f" | "F" | "false" | "FALSE" | "off" | "OFF")
        })
    }

    /// `PreAuthorization#validate_response_mode`, `#validate_code_challenge_method`:
    /// what is wrong with these, if anything.
    fn error(&self) -> Option<&'static str> {
        if !matches!(
            Self::present(&self.response_mode),
            None | Some("query" | "fragment" | "form_post")
        ) {
            return Some("The authorization server does not support this response mode.");
        }
        if Self::present(&self.code_challenge).is_some()
            && Self::present(&self.code_challenge_method) != Some("S256")
        {
            return Some("The code_challenge_method must be S256.");
        }
        None
    }

    /// The query string that asks for the same authorization again.
    fn query(&self, client_id: &str, redirect_uri: &str, scope: &str) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", scope);
        for (key, value) in [
            ("state", &self.state),
            ("code_challenge", &self.code_challenge),
            ("code_challenge_method", &self.code_challenge_method),
            ("response_mode", &self.response_mode),
            ("force_login", &self.force_login),
        ] {
            if let Some(value) = Self::present(value) {
                query.append_pair(key, value);
            }
        }
        query.finish()
    }
}

/// `redirect_to`, which Rails answers with a `302`.
fn found(location: &str) -> Response {
    (
        StatusCode::FOUND,
        [(axum::http::header::LOCATION, location.to_owned())],
    )
        .into_response()
}

/// Where the browser goes with what the authorization page decided.
enum Outcome {
    /// `redirect_to auth.redirect_uri`.
    Redirect(String),
    /// `render :form_post`: the fields, posted to the redirect URI.
    FormPost(String, Vec<(&'static str, String)>),
}

impl IntoResponse for Outcome {
    fn into_response(self) -> Response {
        match self {
            Outcome::Redirect(url) => found(&url),
            Outcome::FormPost(action, fields) => {
                let escape = |s: &str| {
                    s.replace('&', "&amp;")
                        .replace('"', "&quot;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;")
                };
                let inputs: String = fields
                    .iter()
                    .map(|(name, value)| {
                        format!(
                            "<input type=\"hidden\" name=\"{name}\" value=\"{}\">",
                            escape(value)
                        )
                    })
                    .collect();
                Html(format!(
                    "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\"><title>Submit this form</title></head>\
                     <body><h1>Submit this form</h1>\
                     <form method=\"post\" action=\"{}\" id=\"authorization_form\">{inputs}\
                     <input type=\"submit\" value=\"Submit\"></form>\
                     <script>window.onload = function () {{ document.getElementById(\"authorization_form\").submit(); }};</script>\
                     </body></html>\n",
                    escape(&action)
                ))
                .into_response()
            }
        }
    }
}

/// `CodeResponse`/`ErrorResponse#redirect_uri`: `fields` merged into the
/// redirect URI's query (`URIBuilder.uri_with_query`), into its fragment for
/// `response_mode=fragment`, or posted to it for `form_post`. Blank fields are
/// left out.
fn respond_to_client(
    redirect_uri: &str,
    response_mode: Option<&str>,
    fields: Vec<(&'static str, String)>,
) -> Outcome {
    let fields: Vec<(&'static str, String)> =
        fields.into_iter().filter(|(_, v)| !v.is_empty()).collect();
    if response_mode == Some("form_post") {
        return Outcome::FormPost(redirect_uri.to_owned(), fields);
    }
    let Ok(mut url) = url::Url::parse(redirect_uri) else {
        return Outcome::Redirect(redirect_uri.to_owned());
    };
    let encode = |pairs: &[(String, String)]| {
        url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish()
    };
    let fields: Vec<(String, String)> =
        fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
    if response_mode == Some("fragment") {
        url.set_fragment(Some(&encode(&fields)));
    } else {
        let mut pairs: Vec<(String, String)> = url
            .query_pairs()
            .into_owned()
            .filter(|(k, _)| !fields.iter().any(|(f, _)| f == k))
            .collect();
        pairs.extend(fields);
        url.set_query(Some(&encode(&pairs)));
    }
    Outcome::Redirect(url.to_string())
}

// ── GET /oauth/authorize ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AuthorizeParams {
    pub client_id: String,
    pub redirect_uri: String,
    pub response_type: Option<String>,
    pub scope: Option<String>,
    pub lang: Option<String>,
    #[serde(flatten)]
    pub extras: AuthorizationExtras,
}

/// `Oauth::AuthorizationsController#new`. A signed-in user is asked to
/// authorize or deny the client (`render :new`), unless Doorkeeper may
/// answer at once: the application is the instance's own
/// (`skip_authorization`, `superapp`), or it is confidential, the user holds
/// an unrevoked token of it with the same scopes (`matching_token?`), and
/// the request does not say `force_login` (`can_authorize_response?`).
///
/// Without a session the page asks the person to sign in first, where
/// Mastodon's `authenticate_resource_owner!` sends them to its sign-in page
/// to come back (`oauth-authorization-page-signs-in` in divergences.toml).
pub async fn authorize_form(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    Query(params): Query<AuthorizeParams>,
    headers: axum::http::HeaderMap,
) -> Response {
    let app = match sqlx::query_as!(
        OauthApplication,
        "SELECT * FROM oauth_applications WHERE uid = $1",
        params.client_id,
    )
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(a)) => a,
        _ => return (StatusCode::BAD_REQUEST, "Unknown client_id").into_response(),
    };
    // A code is only ever sent where the application said it may go.
    if !redirect_uri_allowed(&params.redirect_uri, &app.redirect_uri) {
        return (StatusCode::BAD_REQUEST, INVALID_REDIRECT_URI).into_response();
    }

    let accept_lang = headers.get("accept-language").and_then(|v| v.to_str().ok());
    let locale = crate::locale::Locale::detect(params.lang.as_deref(), accept_lang);
    // Whether to offer signing up is the registrations setting's.
    let instance = crate::settings::Snapshot::load(&state)
        .await
        .amend(&instance);
    // `validate_params` and `validate_response_type`: the code flow alone.
    match params.response_type.as_deref().filter(|t| !t.is_empty()) {
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "Missing required parameter: response_type.",
            )
                .into_response()
        }
        Some("code") => {}
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "The authorization server does not support this response type.",
            )
                .into_response()
        }
    }
    if let Some(error) = params.extras.error() {
        return (StatusCode::BAD_REQUEST, error).into_response();
    }
    let scope = params.scope.as_deref().unwrap_or("read");
    // The requested scope must be within the app's registered scopes.
    if !scope_is_subset(scope, app.scopes.as_deref().unwrap_or("read")) {
        return (
            StatusCode::BAD_REQUEST,
            "The requested scope is invalid, unknown, or malformed.",
        )
            .into_response();
    }
    let Some(user_id) = crate::api::account::signed_in_user(&headers, &state).await else {
        return render_authorize(
            &instance,
            &app.name,
            &params.client_id,
            &params.redirect_uri,
            scope,
            &params.extras,
            locale,
            "",
        );
    };
    let here = format!(
        "/oauth/authorize?{}",
        params
            .extras
            .query(&params.client_id, &params.redirect_uri, scope)
    );
    // `require_functional!`, the page kept to come back to
    // (`store_current_location`).
    let continuation = crate::api::account::sign_in::Continuation::Oauth {
        client_id: params.client_id.clone(),
        redirect_uri: params.redirect_uri.clone(),
        scope: scope.to_owned(),
        lang: locale.as_str().to_owned(),
        extras: params.extras.clone(),
    };
    if let Some(response) = require_functional(&state, user_id, &here, continuation, locale).await {
        return response;
    }
    let answer_at_once = app.superapp
        || (!params.extras.force_login()
            && app.confidential
            && matching_token(&state, app.id, user_id, scope).await);
    if answer_at_once {
        return match issue_grant(
            &state,
            &params.client_id,
            &params.redirect_uri,
            Some(scope.to_owned()),
            user_id,
            &params.extras,
        )
        .await
        {
            Ok(outcome) => outcome.into_response(),
            Err(error) => (StatusCode::BAD_REQUEST, error).into_response(),
        };
    }
    render_consent(
        &state,
        &app.name,
        &params.client_id,
        &params.redirect_uri,
        scope,
        &params.extras,
        locale,
        user_id,
        &here,
    )
    .await
}

/// `AccessTokenMixin.matching_token_for`: an unrevoked token the user holds
/// of the application, expired or not, whose scopes are the requested ones
/// (`scopes_match?`, in any order).
async fn matching_token(state: &AppState, application_id: i64, user_id: i64, scope: &str) -> bool {
    let wanted: std::collections::BTreeSet<&str> = scope.split_whitespace().collect();
    let held: Vec<Option<String>> = sqlx::query_scalar!(
        r#"SELECT scopes FROM oauth_access_tokens
           WHERE application_id = $1 AND resource_owner_id = $2 AND revoked_at IS NULL"#,
        application_id,
        user_id,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    held.iter().any(|scopes| {
        scopes
            .as_deref()
            .unwrap_or("")
            .split_whitespace()
            .collect::<std::collections::BTreeSet<_>>()
            == wanted
    })
}

/// `ScopeTransformer::Scope`: what `grouped_scopes` makes of one scope, its
/// key (`admin/accounts`, `follow`, `all`…) and the access it gives.
fn scope_group(scope: &str) -> (String, Vec<&'static str>) {
    let mut parts: Vec<&str> = scope.split(':').collect();
    let namespace = (parts.first() == Some(&"admin") && parts.len() > 1).then(|| {
        parts.remove(0);
        "admin"
    });
    let (access, term): (Option<&'static str>, Option<&str>) = match parts.as_slice() {
        ["read"] => (Some("read"), None),
        ["write"] => (Some("write"), None),
        ["read", term] => (Some("read"), Some(*term)),
        ["write", term] => (Some("write"), Some(*term)),
        [term] => (None, Some(*term)),
        _ => (None, Some(scope)),
    };
    let term = term.unwrap_or("all");
    let access = if term == "profile" {
        vec!["read"]
    } else {
        access.map_or_else(|| vec!["read", "write"], |a| vec![a])
    };
    let key = match namespace {
        Some(namespace) => format!("{namespace}/{term}"),
        None => term.to_owned(),
    };
    (key, access)
}

/// `ApplicationHelper#grouped_scopes`: the scopes merged by key, in the order
/// they first appear, each with its access.
fn grouped_scopes(scope: &str) -> Vec<(String, String)> {
    let mut groups: Vec<(String, Vec<&'static str>)> = Vec::new();
    for scope in scope.split_whitespace() {
        let (key, access) = scope_group(scope);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            // `merge!`: the accesses together, deduplicated and sorted.
            Some((_, existing)) => {
                existing.extend(access);
                existing.sort_unstable();
                existing.dedup();
            }
            None => groups.push((key, access)),
        }
    }
    groups
        .into_iter()
        .map(|(key, access)| (key, access.join("/")))
        .collect()
}

/// `doorkeeper.authorizations.new`: the authorize-or-deny page.
#[allow(clippy::too_many_arguments)]
async fn render_consent(
    state: &AppState,
    app_name: &str,
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    extras: &AuthorizationExtras,
    locale: crate::locale::Locale,
    user_id: i64,
    here: &str,
) -> Response {
    let username = sqlx::query_scalar!(
        "SELECT a.username FROM users u JOIN accounts a ON a.id = u.account_id WHERE u.id = $1",
        user_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    let permissions: Vec<minijinja::Value> = grouped_scopes(scope)
        .into_iter()
        .map(|(key, access)| {
            minijinja::context! {
                title => locale.t(&format!("scope_title_{key}")),
                access => locale.t(&format!("scope_access_{access}")),
            }
        })
        .collect();
    let prompt = minijinja::Value::from_safe_string(locale.t("authorization_prompt_html").replace(
        "%{client_name}",
        &format!("<strong>{}</strong>", escape_html(app_name)),
    ));
    let html = crate::templates::render(
        "authorize_consent.html",
        minijinja::context! {
            lang => locale.as_str(),
            domain => state.instance.domain,
            username => username,
            client_id => client_id,
            redirect_uri => redirect_uri,
            scope => scope,
            extras => extras,
            permissions => permissions,
            prompt => prompt,
            continue_to => here,
            t_title => locale.t("authorization_required"),
            t_review_permissions => locale.t("review_permissions"),
            t_authorize => locale.t("authorize_button"),
            t_deny => locale.t("deny_button"),
            t_signed_in_as => locale.t("signed_in_as"),
            t_logout => locale.t("logout"),
        },
    );
    Html(html).into_response()
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// The sign-in form of the authorization page.
#[allow(clippy::too_many_arguments)]
fn render_authorize(
    instance: &crate::config::InstanceConfig,
    app_name: &str,
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    extras: &AuthorizationExtras,
    locale: crate::locale::Locale,
    error: &str,
) -> Response {
    let base = format!(
        "/oauth/authorize?{}",
        extras.query(client_id, redirect_uri, scope)
    );
    let (toggle_en_url, toggle_ko_url) = (format!("{base}&lang=en"), format!("{base}&lang=ko"));
    let signup_url = format!("/auth/signup?lang={}", locale.as_str());
    let html = crate::templates::render(
        "authorize.html",
        minijinja::context! {
            domain => instance.domain,
            app_name => app_name,
            client_id => client_id,
            redirect_uri => redirect_uri,
            scope => scope,
            extras => extras,
            error => error,
            lang => locale.as_str(),
            toggle_en_url => toggle_en_url,
            toggle_ko_url => toggle_ko_url,
            registrations_open => instance.registrations_open,
            signup_url => signup_url,
            t_sign_in_to => locale.t("sign_in_to"),
            t_authorize => locale.t("authorize"),
            t_email => locale.t("email"),
            t_password => locale.t("password"),
            t_sign_in => locale.t("sign_in"),
            t_no_account => locale.t("no_account"),
            t_sign_up => locale.t("sign_up"),
            t_forgot_password => locale.t("forgot_password"),
        },
    );
    Html(html).into_response()
}

// ── POST /oauth/authorize ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AuthorizeForm {
    pub client_id: String,
    pub redirect_uri: String,
    pub scope: Option<String>,
    pub email: Option<String>,
    pub password: Option<String>,
    pub lang: Option<String>,
    #[serde(flatten)]
    pub extras: AuthorizationExtras,
    /// A step after the password: the second factor, or the setup the
    /// user's role requires.
    #[serde(flatten)]
    pub step: crate::api::account::sign_in::Submitted,
}

#[derive(Debug, Default, Deserialize)]
struct MethodOverride {
    #[serde(rename = "_method")]
    method: Option<String>,
}

/// `POST /oauth/authorize`, three things in one. With `_method=delete`, the
/// deny button's form, it is `DELETE` (`Rack::MethodOverride`). With an
/// email and password, or a step after them, it is the authorization page's
/// sign-in. Otherwise it is the authorize button:
/// `Oauth::AuthorizationsController#create`, for a signed-in user.
pub async fn authorize_submit(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    client_ip: Option<Extension<crate::remote_ip::ClientIp>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> Response {
    let method: MethodOverride = serde_urlencoded::from_bytes(&body).unwrap_or_default();
    if method
        .method
        .is_some_and(|m| m.eq_ignore_ascii_case("delete"))
    {
        return authorize_deny(state, headers, uri, body).await;
    }
    let Ok(form) = serde_urlencoded::from_bytes::<AuthorizeForm>(&body) else {
        return (
            StatusCode::BAD_REQUEST,
            "Missing required parameter: client_id.",
        )
            .into_response();
    };
    if form.email.is_some() || form.password.is_some() || form.step.is_attempt() {
        return sign_in_to_authorize(state, instance, client_ip, headers, form).await;
    }

    // `create`: `authenticate_resource_owner!` sends a signed-out browser to
    // sign in, then back to the page.
    let scope = form.scope.clone().unwrap_or_else(|| "read".to_string());
    let page = format!(
        "/oauth/authorize?{}",
        form.extras
            .query(&form.client_id, &form.redirect_uri, &scope)
    );
    let Some(user_id) = crate::api::account::signed_in_user(&headers, &state).await else {
        return found(&page);
    };
    let locale = crate::locale::Locale::detect(form.lang.as_deref(), None);
    let continuation = crate::api::account::sign_in::Continuation::Oauth {
        client_id: form.client_id.clone(),
        redirect_uri: form.redirect_uri.clone(),
        scope: scope.clone(),
        lang: locale.as_str().to_owned(),
        extras: form.extras.clone(),
    };
    if let Some(response) = require_functional(&state, user_id, &page, continuation, locale).await {
        return response;
    }
    match issue_grant(
        &state,
        &form.client_id,
        &form.redirect_uri,
        Some(scope),
        user_id,
        &form.extras,
    )
    .await
    {
        Ok(outcome) => outcome.into_response(),
        Err(error) => (StatusCode::BAD_REQUEST, error).into_response(),
    }
}

/// Sign in on the authorization page, then back to it, as Mastodon's
/// session sign-in returns to the page it was sent from. The password,
/// then whatever [`crate::api::account::sign_in`] asks for.
async fn sign_in_to_authorize(
    state: AppState,
    instance: crate::config::InstanceConfig,
    client_ip: Option<Extension<crate::remote_ip::ClientIp>>,
    headers: axum::http::HeaderMap,
    form: AuthorizeForm,
) -> Response {
    use crate::api::account::sign_in::{self, Continuation, Step};

    let ip = client_ip.and_then(|Extension(c)| c.0);
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let locale = crate::locale::Locale::detect(form.lang.as_deref(), None);
    let instance = crate::settings::Snapshot::load(&state)
        .await
        .amend(&instance);
    let app_name = sqlx::query_scalar!(
        "SELECT name FROM oauth_applications WHERE uid = $1",
        form.client_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| form.client_id.clone());
    let scope = form.scope.clone().unwrap_or_else(|| "read".to_string());
    let form_error = |error: &str| {
        render_authorize(
            &instance,
            &app_name,
            &form.client_id,
            &form.redirect_uri,
            &scope,
            &form.extras,
            locale,
            error,
        )
    };

    let continuation = Continuation::Oauth {
        client_id: form.client_id.clone(),
        redirect_uri: form.redirect_uri.clone(),
        scope: form.scope.clone().unwrap_or_default(),
        lang: locale.as_str().to_string(),
        extras: form.extras.clone(),
    };
    let step = if form.step.is_attempt() {
        sign_in::continue_attempt(&state, &form.step, continuation, ip, user_agent.as_deref()).await
    } else {
        match check_password(
            &state,
            form.email.as_deref().unwrap_or(""),
            form.password.as_deref().unwrap_or(""),
            ip,
            user_agent.as_deref(),
        )
        .await
        {
            Some(user_id) => {
                sign_in::after_password(&state, user_id, continuation, ip, user_agent.as_deref())
                    .await
            }
            None => return form_error(locale.t("invalid_credentials")),
        }
    };

    match step {
        Step::SignedIn(
            user_id,
            Continuation::Oauth {
                client_id,
                redirect_uri,
                scope,
                extras,
                lang,
            },
        ) => {
            // Signed in, back to the page (`after_sign_in_path_for`, the
            // stored location), which asks to authorize or answers at once.
            // An address still unconfirmed goes on to `auth/setup`, where
            // the page's `require_functional!` would send it, with the page
            // kept to come back to.
            let scope = if scope.is_empty() {
                "read".to_owned()
            } else {
                scope
            };
            let back = format!(
                "/oauth/authorize?{}&lang={}",
                extras.query(&client_id, &redirect_uri, &scope),
                urlencoding::encode(&lang)
            );
            let (target, return_to) = if user_confirmed(&state, user_id).await {
                (back.as_str(), None)
            } else {
                ("/auth/setup", Some(back.as_str()))
            };
            crate::api::account::sign_in_and_redirect(
                &state,
                user_id,
                ip,
                user_agent.as_deref(),
                target,
                return_to,
            )
            .await
        }
        Step::SignedIn(_, Continuation::Account) | Step::Restart(_) => {
            form_error(locale.t("session_timeout"))
        }
        Step::Render(page) => sign_in::render(&state, locale, &page),
        Step::Failed(_) => form_error(locale.t("err_server")),
    }
}

/// The password half of a sign-in: the user it belongs to, or `None`, with
/// the failure recorded. A suspended account, or one being deleted, is
/// refused as the account pages refuse it, since the session it would start
/// is not one the authorization page can use.
async fn check_password(
    state: &AppState,
    email: &str,
    password: &str,
    ip: Option<std::net::IpAddr>,
    user_agent: Option<&str>,
) -> Option<i64> {
    let user = sqlx::query!(
        r#"SELECT u.id, u.encrypted_password
           FROM users u
           JOIN accounts a ON a.id = u.account_id
           WHERE lower(u.email) = lower($1)
             AND u.disabled = false
             AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL"#,
        email.trim(),
    )
    .fetch_optional(&state.db)
    .await
    .ok()??;
    // `find_user_from_params`: an account with no password cannot use one.
    if user.encrypted_password.is_empty() {
        return None;
    }
    if crate::crypto::verify_password(password, &user.encrypted_password)
        .await
        .is_err()
    {
        crate::accounts::record_login(
            &state.db,
            user.id,
            ip,
            user_agent,
            "password",
            false,
            Some("invalid"),
        )
        .await;
        return None;
    }
    Some(user.id)
}

/// `ApplicationController#require_functional!`, which the authorization
/// page's every action runs for a signed-in user: nothing for a functional
/// one; the two-factor setup the role requires (`mfa_setup_path(oauth:
/// true)`, eunha's sign-in setup step, which comes back to the page); the
/// account page (`edit_user_registration_path`) for one confirmed but
/// otherwise not functional — pending approval, a memorial or moved; and
/// `auth/setup` for an unconfirmed address, the page kept to come back to.
async fn require_functional(
    state: &AppState,
    user_id: i64,
    page: &str,
    continuation: crate::api::account::sign_in::Continuation,
    locale: crate::locale::Locale,
) -> Option<Response> {
    use crate::api::account::sign_in::{self, Step};
    let standing = crate::user_standing::UserStanding::of_user(&state.db, user_id)
        .await
        .ok()
        .flatten()?;
    if standing.functional() {
        return None;
    }
    if standing.missing_2fa {
        return Some(
            match sign_in::begin_required_setup(state, user_id, continuation).await {
                Step::Render(next) => sign_in::render(state, locale, &next),
                _ => (StatusCode::INTERNAL_SERVER_ERROR, locale.t("err_server")).into_response(),
            },
        );
    }
    Some(if standing.confirmed {
        crate::api::account::redirect_storing_location("/account", page)
    } else {
        crate::api::account::redirect_storing_location("/auth/setup", page)
    })
}

/// Whether the user has confirmed their address.
async fn user_confirmed(state: &AppState, user_id: i64) -> bool {
    sqlx::query_scalar!(
        r#"SELECT confirmed_at IS NOT NULL AS "confirmed!" FROM users WHERE id = $1"#,
        user_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .unwrap_or(false)
}

/// `Oauth::AuthorizationsController#create`: an authorization code for the
/// signed-in user, and where to send them with it.
async fn issue_grant(
    state: &AppState,
    client_id: &str,
    redirect_uri: &str,
    scope: Option<String>,
    user_id: i64,
    extras: &AuthorizationExtras,
) -> Result<Outcome, String> {
    let app = sqlx::query_as!(
        OauthApplication,
        "SELECT * FROM oauth_applications WHERE uid = $1",
        client_id,
    )
    .fetch_optional(&state.db)
    .await
    .map_err(|_| "Database error".to_string())?
    .ok_or_else(|| "Unknown application".to_string())?;
    // The form posts the redirect URI back, so it is checked again here.
    if !redirect_uri_allowed(redirect_uri, &app.redirect_uri) {
        return Err(INVALID_REDIRECT_URI.to_string());
    }

    let scopes = scope.unwrap_or_else(|| app.scopes.clone().unwrap_or_else(|| "read".to_string()));
    // The granted scope must stay within the app's registered scopes.
    if !scope_is_subset(&scopes, app.scopes.as_deref().unwrap_or("read")) {
        return Err("invalid_scope".to_string());
    }
    if let Some(error) = extras.error() {
        return Err(error.to_string());
    }
    let code = generate_token(32);
    // `Authorization::Code#pkce_attributes`.
    let code_challenge = AuthorizationExtras::present(&extras.code_challenge);
    let code_challenge_method =
        code_challenge.and(AuthorizationExtras::present(&extras.code_challenge_method));

    sqlx::query!(
        r#"INSERT INTO oauth_access_grants
             (application_id, resource_owner_id, token, redirect_uri, scopes, expires_in,
              code_challenge, code_challenge_method, created_at)
           VALUES ($1, $2, $3, $4, $5, 600, $6, $7, now())"#,
        app.id,
        user_id,
        code,
        redirect_uri,
        scopes,
        code_challenge,
        code_challenge_method,
    )
    .execute(&state.db)
    .await
    .map_err(|_| "Database error".to_string())?;

    // `CodeResponse`: an out-of-band client is shown the code
    // (`oob_redirect`); any other gets it, with the state it sent, at its
    // redirect URI.
    if redirect_uri == OOB_REDIRECT_URI {
        return Ok(Outcome::Redirect(format!(
            "/oauth/authorize/native?code={}",
            urlencoding::encode(&code)
        )));
    }
    Ok(respond_to_client(
        redirect_uri,
        AuthorizationExtras::present(&extras.response_mode),
        vec![
            ("code", code),
            ("state", extras.state.clone().unwrap_or_default()),
        ],
    ))
}

// ── GET /oauth/authorize/native ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct NativeParams {
    pub code: Option<String>,
}

/// `Doorkeeper::AuthorizationsController#show`, where an out-of-band client's
/// user is sent with the code to copy into it.
pub async fn authorize_native(Query(params): Query<NativeParams>) -> Response {
    let code = params
        .code
        .unwrap_or_default()
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    Html(format!(
        "<!DOCTYPE html>\n<html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>Authorization code</title><link rel=\"stylesheet\" href=\"/auth.css\"></head>\
         <body class=\"centered\"><div class=\"card\">\
         <p>Copy this authorization code and paste it to the application.</p>\
         <input type=\"text\" class=\"oauth-code\" readonly value=\"{code}\" onclick=\"this.select()\">\
         </div></body></html>\n"
    ))
    .into_response()
}

// ── DELETE /oauth/authorize ───────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
pub struct DenyParams {
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    #[serde(flatten)]
    pub extras: AuthorizationExtras,
}

/// `Doorkeeper::AuthorizationsController#destroy`: the signed-in user turns
/// the client away (`CodeRequest#deny`), and the client hears
/// `access_denied`, with its state, at its redirect URI — or, out of band, as
/// a `400`.
///
/// `authenticate_resource_owner!` comes first: without a session the browser
/// is sent to sign in. The parameters are read from the query and a form
/// body both, as Rails merges them. Since Doorkeeper 5.9.9 (Mastodon 4.7.3)
/// the client and its redirect URI are checked first, as they are before
/// authorizing (`refuse_invalid_client?`), and a refusal is shown, never
/// sent to the redirect URI: a missing `client_id` is `invalid_request`, an
/// unknown one `invalid_client`, and a redirect URI the client did not
/// register `invalid_redirect_uri`.
pub async fn authorize_deny(
    state: AppState,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> Response {
    let Some(user_id) = crate::api::account::signed_in_user(&headers, &state).await else {
        return crate::api::account::sign_in_redirect(
            uri.path_and_query()
                .map_or("/oauth/authorize", |p| p.as_str()),
        );
    };
    let mut params: DenyParams =
        serde_urlencoded::from_str(uri.query().unwrap_or("")).unwrap_or_default();
    if let Ok(form) = serde_urlencoded::from_bytes::<DenyParams>(&body) {
        params.client_id = form.client_id.or(params.client_id);
        params.redirect_uri = form.redirect_uri.or(params.redirect_uri);
        let extras = form.extras;
        params.extras.state = extras.state.or(params.extras.state);
        params.extras.response_mode = extras.response_mode.or(params.extras.response_mode);
    }
    // `require_functional!`, before `destroy`.
    let client_id = params.client_id.clone().unwrap_or_default();
    let redirect_uri = params.redirect_uri.clone().unwrap_or_default();
    let page = format!(
        "/oauth/authorize?{}",
        params.extras.query(&client_id, &redirect_uri, "read")
    );
    let locale = crate::locale::Locale::detect(None, None);
    let continuation = crate::api::account::sign_in::Continuation::Oauth {
        client_id,
        redirect_uri,
        scope: "read".to_owned(),
        lang: locale.as_str().to_owned(),
        extras: params.extras.clone(),
    };
    if let Some(response) = require_functional(&state, user_id, &page, continuation, locale).await {
        return response;
    }
    // `validate_client_id`: blank is missing.
    let Some(client_id) = params.client_id.as_deref().filter(|id| !id.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            "Missing required parameter: client_id.",
        )
            .into_response();
    };
    let registered = sqlx::query_scalar!(
        "SELECT redirect_uri FROM oauth_applications WHERE uid = $1",
        client_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();
    let Some(registered) = registered else {
        return (
            StatusCode::UNAUTHORIZED,
            "Client authentication failed due to unknown client, no client authentication included, or unsupported authentication method.",
        )
            .into_response();
    };
    let redirect_uri = params.redirect_uri.unwrap_or_default();
    if !redirect_uri_allowed(&redirect_uri, &registered) {
        return (StatusCode::BAD_REQUEST, INVALID_REDIRECT_URI).into_response();
    }
    let description = "The resource owner or authorization server denied the request.";
    let state_param = params.extras.state.clone().unwrap_or_default();
    if redirect_uri == OOB_REDIRECT_URI {
        let mut body =
            serde_json::json!({ "error": "access_denied", "error_description": description });
        if !state_param.is_empty() {
            body["state"] = state_param.into();
        }
        return (StatusCode::BAD_REQUEST, Json(body)).into_response();
    }
    respond_to_client(
        &redirect_uri,
        AuthorizationExtras::present(&params.extras.response_mode),
        vec![
            ("error", "access_denied".to_owned()),
            ("error_description", description.to_owned()),
            ("state", state_param),
        ],
    )
    .into_response()
}
