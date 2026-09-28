use axum::{extract::Request, middleware::Next, response::Response};
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
}

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
        // Parent scope covers child: "read" → "read:*", "write" → "write:*"
        if let Some(parent) = required.split(':').next() {
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
            r#"SELECT t.id, u.account_id, t.application_id, t.scopes,
                      t.expires_in, t.created_at, t.revoked_at, u.id as "user_id?",
                      u.disabled as "disabled?", a.suspended_at AS "suspended_at?",
                      a.requested_deletion_at AS "requested_deletion_at?"
               FROM oauth_access_tokens t
               LEFT JOIN users u ON u.id = t.resource_owner_id
               LEFT JOIN accounts a ON a.id = u.account_id
               WHERE t.token = $1"#,
            token
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        {
            // App-only tokens (client_credentials) have no user, so `disabled` is
            // NULL there and must stay valid; a disabled user's tokens are rejected.
            let user_disabled = tok.disabled.unwrap_or(false);
            // Mastodon's `Api::BaseController#require_not_suspended!`: an
            // unavailable account cannot act, but its tokens stay intact so
            // unsuspending restores access without a new sign-in.
            let account_suspended =
                tok.suspended_at.is_some() || tok.requested_deletion_at.is_some();
            let valid = tok.revoked_at.is_none()
                && token_not_expired(tok.created_at, tok.expires_in)
                && !user_disabled
                && !account_suspended;

            if valid {
                let user = AuthenticatedUser {
                    account_id: tok.account_id,
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
                };
                req.extensions_mut().insert(user);
            }
        }
    }
    next.run(req).await
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
    // names the sender when nothing else about the request can be trusted.
    let inbox_post = method == axum::http::Method::POST && path.ends_with("/inbox");
    let key_id = inbox_post.then(|| claimed_key_id(req.headers())).flatten();

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
        tracing::warn!(
            path = %path,
            status = %status,
            key_id = key_id.as_deref().unwrap_or(""),
            reason = %String::from_utf8_lossy(&bytes),
            "inbox refused a delivery",
        );
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    }

    response
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
}
