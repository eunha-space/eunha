use axum::{
    extract::{Extension, Form, Query},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use super::extractors::FormOrJson;

use super::types::{AppCredentials, CredentialApplication, Token};
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
    headers: axum::http::HeaderMap,
) -> AppResult<Json<AppCredentials>> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
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

#[derive(Debug, Deserialize)]
pub struct RegisterAppForm {
    pub client_name: String,
    pub redirect_uris: Option<String>,
    pub scopes: Option<String>,
    pub website: Option<String>,
}

pub async fn register_app(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    FormOrJson(form): FormOrJson<RegisterAppForm>,
) -> AppResult<Json<CredentialApplication>> {
    let client_id = generate_token(32);
    let client_secret = generate_token(64);
    let redirect_uris = form
        .redirect_uris
        .unwrap_or_else(|| "urn:ietf:wg:oauth:2.0:oob".into());
    let scopes = normalize_scopes(&form.scopes.unwrap_or_else(|| "read".into()));
    validate_oauth_scopes(&scopes)?;

    let app = sqlx::query_as!(
        OauthApplication,
        r#"INSERT INTO oauth_applications
             (name, uid, secret, redirect_uri, scopes, website, created_at, updated_at)
           VALUES ($1,$2,$3,$4,$5,$6,now(),now())
           RETURNING *"#,
        form.client_name,
        client_id,
        client_secret,
        redirect_uris,
        scopes,
        form.website,
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
    pub grant_type: String,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub redirect_uri: Option<String>,
    pub code: Option<String>,
    pub scope: Option<String>,
    pub code_verifier: Option<String>,
}

/// Doorkeeper's `client_credentials_methods`, `from_basic` then
/// `from_params`: the client's id and secret from an `Authorization: Basic`
/// header (`client_secret_basic`), each form-decoded, or else from the request
/// (`client_secret_post`).
fn client_credentials(
    headers: &axum::http::HeaderMap,
    form: &TokenRequest,
) -> Option<(String, String)> {
    use base64::Engine as _;
    let basic = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|token| {
            base64::engine::general_purpose::STANDARD
                .decode(token.trim())
                .ok()
        })
        .and_then(|decoded| String::from_utf8(decoded).ok())
        .and_then(|decoded| {
            let (id, secret) = decoded.split_once(':')?;
            let decode = |s: &str| {
                urlencoding::decode(&s.replace('+', " "))
                    .map(|s| s.into_owned())
                    .ok()
            };
            Some((decode(id)?, decode(secret)?))
        });
    basic.or_else(|| Some((form.client_id.clone()?, form.client_secret.clone()?)))
}

/// A Doorkeeper error answer, `{"error":…,"error_description":…}` with a
/// `400`.
fn oauth_error(error: &str, description: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": error, "error_description": description })),
    )
        .into_response()
}

/// `AccessGrant.generate_code_challenge`: the `S256` challenge of a verifier.
fn s256_challenge(verifier: &str) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier))
}

pub async fn issue_token(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    headers: axum::http::HeaderMap,
    FormOrJson(form): FormOrJson<TokenRequest>,
) -> AppResult<axum::response::Response> {
    use axum::response::IntoResponse as _;

    // `grant_flows %w(authorization_code client_credentials)`: Doorkeeper
    // turns any other grant type away before it looks at the client, with
    // `unsupported_grant_type`. The password grant is among them, and
    // `resource_owner_from_credentials` would refuse it anyway.
    if !matches!(
        form.grant_type.as_str(),
        "authorization_code" | "client_credentials"
    ) {
        return Ok((
            axum::http::StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "unsupported_grant_type",
                "error_description": "The authorization grant type is not supported by the authorization server.",
            })),
        )
            .into_response());
    }
    let (client_id, client_secret) =
        client_credentials(&headers, &form).ok_or(AppError::Unauthorized)?;
    tracing::info!(
        grant_type = %form.grant_type,
        client_id = %client_id,
        instance = %instance.domain,
        "token request",
    );
    // Verify client credentials
    let app = sqlx::query_as!(
        OauthApplication,
        "SELECT * FROM oauth_applications WHERE uid = $1",
        client_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| {
        tracing::warn!(client_id = %client_id, instance = %instance.domain, "unknown client_id");
        AppError::Unauthorized
    })?;

    if app.secret != client_secret {
        tracing::warn!(client_id = %client_id, "client_secret mismatch");
        return Err(AppError::Unauthorized);
    }

    let (user_id, scopes) = match form.grant_type.as_str() {
        "client_credentials" => (
            None,
            app.scopes.clone().unwrap_or_else(|| "read".to_string()),
        ),

        "authorization_code" => {
            let code_str = form
                .code
                .as_deref()
                .ok_or(AppError::Unprocessable("missing code".into()))?;
            // `AuthorizationCodeRequest#validate_redirect_uri`: the code is
            // only good with the redirect URI it was issued for.
            let grant = sqlx::query!(
                r#"SELECT redirect_uri, code_challenge, code_challenge_method
                   FROM oauth_access_grants
                   WHERE token = $1 AND application_id = $2 AND revoked_at IS NULL
                     AND created_at + expires_in * interval '1 second' > now()"#,
                code_str,
                app.id,
            )
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| {
                tracing::warn!(code = %code_str, "authorization code not found or expired");
                AppError::Unauthorized
            })?;
            let challenge = grant.code_challenge.filter(|c| !c.is_empty());
            let verifier = form.code_verifier.as_deref().filter(|v| !v.is_empty());
            // `AuthorizationCodeRequest#validate_params`: a grant made with
            // PKCE wants its verifier.
            if challenge.is_some() && verifier.is_none() {
                return Ok(oauth_error(
                    "invalid_request",
                    "Missing required parameter: code_verifier.",
                ));
            }
            if !form
                .redirect_uri
                .as_deref()
                .is_some_and(|uri| redirect_uri_matches(uri, &grant.redirect_uri))
            {
                tracing::warn!(client_id = %client_id, "redirect_uri does not match the code's");
                return Err(AppError::Unauthorized);
            }
            // `validate_code_verifier`: the verifier's `S256` challenge is the
            // grant's, and a verifier for a grant made without one fails.
            if let Some(verifier) = verifier {
                let matches = match challenge.as_deref() {
                    Some(challenge) if grant.code_challenge_method.as_deref() == Some("plain") => {
                        verifier == challenge
                    }
                    Some(challenge) => s256_challenge(verifier) == challenge,
                    None => false,
                };
                if !matches {
                    return Ok(oauth_error("invalid_grant", INVALID_GRANT));
                }
            }
            let code = sqlx::query!(
                r#"DELETE FROM oauth_access_grants
                   WHERE token = $1 AND application_id = $2
                     AND created_at + expires_in * interval '1 second' > now()
                   RETURNING resource_owner_id, scopes"#,
                code_str,
                app.id,
            )
            .fetch_optional(&state.db)
            .await?
            .ok_or_else(|| {
                tracing::warn!(code = %code_str, "authorization code not found or expired");
                AppError::Unauthorized
            })?;
            // The sign-in was recorded on the authorization page, as
            // `Auth::SessionsController` records it; the exchange records none.
            let _account_id = sqlx::query_scalar!(
                "SELECT account_id FROM users WHERE id = $1 AND disabled = false",
                code.resource_owner_id,
            )
            .fetch_optional(&state.db)
            .await?
            .ok_or(AppError::Unauthorized)?;
            (
                Some(code.resource_owner_id),
                code.scopes.unwrap_or_else(|| "read".to_string()),
            )
        }

        _ => unreachable!("only the grant flows Mastodon enables get this far"),
    };

    let token_str = generate_token(64);
    let created_at = chrono::Utc::now();

    sqlx::query!(
        r#"INSERT INTO oauth_access_tokens (application_id, resource_owner_id, token, scopes, created_at)
           VALUES ($1, $2, $3, $4, now())"#,
        app.id,
        user_id,
        token_str,
        scopes,
    )
    .execute(&state.db)
    .await?;

    Ok(Json(Token {
        access_token: token_str,
        token_type: "Bearer".to_string(),
        scope: scopes,
        created_at: created_at.timestamp(),
    })
    .into_response())
}

// ── POST /oauth/revoke ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct RevokeRequest {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub token: String,
}

pub async fn revoke_token(
    state: AppState,
    FormOrJson(form): FormOrJson<RevokeRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let revoked: Vec<i64> = sqlx::query_scalar!(
        r#"UPDATE oauth_access_tokens SET revoked_at = now()
           WHERE token = $1 AND revoked_at IS NULL
           RETURNING id"#,
        form.token,
    )
    .fetch_all(&state.db)
    .await?;
    // `Oauth::TokensController#unsubscribe_for_token`, and the token's
    // streams closed (`AccessTokenExtension#push_to_streaming_api`).
    sqlx::query!(
        "DELETE FROM web_push_subscriptions WHERE access_token_id = ANY($1)",
        &revoked
    )
    .execute(&state.db)
    .await?;
    crate::sessions::kill_streams(&state, revoked).await;
    Ok(Json(serde_json::json!({})))
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

/// Reject app registration requesting a scope Mastodon wouldn't configure.
fn validate_oauth_scopes(scopes: &str) -> AppResult<()> {
    for scope in scopes.split(' ').filter(|s| !s.is_empty()) {
        if !VALID_OAUTH_SCOPES.contains(&scope) {
            return Err(AppError::Unprocessable(
                "The requested scope is invalid, unknown, or malformed.".into(),
            ));
        }
    }
    Ok(())
}

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

#[derive(Debug, Deserialize)]
pub struct ElkLoginBody {
    pub force_login: Option<bool>,
    pub origin: String,
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
    Json(body): Json<ElkLoginBody>,
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
}

impl AuthorizationExtras {
    fn present(value: &Option<String>) -> Option<&str> {
        value.as_deref().filter(|v| !v.is_empty())
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
        ] {
            if let Some(value) = Self::present(value) {
                query.append_pair(key, value);
            }
        }
        query.finish()
    }
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
            Outcome::Redirect(url) => Redirect::to(&url).into_response(),
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
    pub force_login: Option<String>,
    pub lang: Option<String>,
    #[serde(flatten)]
    pub extras: AuthorizationExtras,
}

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
    render_authorize(
        &instance,
        &app.name,
        &params.client_id,
        &params.redirect_uri,
        scope,
        &params.extras,
        locale,
        "",
    )
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

/// Sign in on the authorization page, then grant the code. The password,
/// then whatever [`crate::api::account::sign_in`] asks for, as Mastodon's
/// session sign-in would before `Oauth::AuthorizationsController` answers.
pub async fn authorize_submit(
    state: AppState,
    Extension(ResolvedInstance(instance)): Extension<ResolvedInstance>,
    client_ip: Option<Extension<crate::remote_ip::ClientIp>>,
    headers: axum::http::HeaderMap,
    Form(form): Form<AuthorizeForm>,
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
                ..
            },
        ) => {
            // `require_functional!`, which the authorization page runs once
            // the user is signed in: an address still unconfirmed sends them
            // to `auth/setup`, signed in, the page kept to come back to
            // (`store_current_location`).
            if !user_confirmed(&state, user_id).await {
                let back = format!(
                    "/oauth/authorize?{}",
                    extras.query(&client_id, &redirect_uri, &scope)
                );
                return crate::api::account::sign_in_and_redirect(
                    &state,
                    user_id,
                    ip,
                    user_agent.as_deref(),
                    "/auth/setup",
                    Some(&back),
                )
                .await;
            }
            let scope = (!scope.is_empty()).then_some(scope);
            match issue_grant(&state, &client_id, &redirect_uri, scope, user_id, &extras).await {
                Ok(outcome) => outcome.into_response(),
                Err(_) => form_error(locale.t("invalid_credentials")),
            }
        }
        Step::SignedIn(_, Continuation::Account) | Step::Restart(_) => {
            form_error(locale.t("session_timeout"))
        }
        Step::Render(page) => sign_in::render(&state, locale, &page),
        Step::Failed(_) => form_error(locale.t("err_server")),
    }
}

/// The password half of a sign-in: the user it belongs to, or `None`, with
/// the failure recorded.
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
             AND u.disabled = false"#,
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
/// body both, as Rails merges them. Doorkeeper builds the answer from the
/// redirect URI as given; eunha sends it only to a redirect URI the client
/// registered (`oauth-deny-checks-the-redirect-uri` in divergences.toml).
pub async fn authorize_deny(
    state: AppState,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    body: axum::body::Bytes,
) -> Response {
    if crate::api::account::signed_in_user(&headers, &state)
        .await
        .is_none()
    {
        return crate::api::account::sign_in_redirect(
            uri.path_and_query()
                .map_or("/oauth/authorize", |p| p.as_str()),
        );
    }
    let mut params: DenyParams =
        serde_urlencoded::from_str(uri.query().unwrap_or("")).unwrap_or_default();
    if let Ok(form) = serde_urlencoded::from_bytes::<DenyParams>(&body) {
        params.client_id = form.client_id.or(params.client_id);
        params.redirect_uri = form.redirect_uri.or(params.redirect_uri);
        let extras = form.extras;
        params.extras.state = extras.state.or(params.extras.state);
        params.extras.response_mode = extras.response_mode.or(params.extras.response_mode);
    }
    let registered = match params.client_id.as_deref() {
        Some(client_id) => sqlx::query_scalar!(
            "SELECT redirect_uri FROM oauth_applications WHERE uid = $1",
            client_id
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten(),
        None => None,
    };
    let Some(registered) = registered else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "invalid_client",
                "error_description": "Client authentication failed due to unknown client, no client authentication included, or unsupported authentication method.",
            })),
        )
            .into_response();
    };
    let redirect_uri = params.redirect_uri.unwrap_or_default();
    if !redirect_uri_allowed(&redirect_uri, &registered) {
        return oauth_error("invalid_redirect_uri", INVALID_REDIRECT_URI);
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
