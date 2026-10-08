//! How Doorkeeper finds the client a token request comes from, for
//! `/oauth/token`, `/oauth/revoke` and `/oauth/introspect` alike:
//! `Client::Credentials.from_request` with Mastodon's default
//! `client_credentials :from_basic, :from_params`, then
//! `Client.authenticate` (`Application.by_uid_and_secret`).

use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use crate::state::AppState;

/// The client a request names: its uid, and its secret if it gave one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub uid: String,
    pub secret: Option<String>,
}

/// `Errors::MultipleClientAuthMethods`: a secret by more than one method, or
/// two different clients named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MultipleClientAuthMethods;

fn present(value: Option<&str>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty()).map(str::to_owned)
}

/// `Base64.decode64`: what is not in the alphabet is skipped, padding and
/// all, and a trailing partial group decodes as far as it goes.
fn decode64(encoded: &str) -> Vec<u8> {
    use base64::Engine as _;
    let mut alphabet: String = encoded
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/'))
        .collect();
    // A single leftover character carries no whole byte.
    if alphabet.len() % 4 == 1 {
        alphabet.pop();
    }
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(&alphabet)
        .unwrap_or_default()
}

/// `from_basic`: `request.authorization =~ /^Basic (.*)/im`, decoded with
/// `Base64.decode64` and split at the first colon, nothing decoded further.
fn from_basic(headers: &HeaderMap) -> Option<(Option<String>, Option<String>)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let rest = value
        .get(..6)
        .filter(|scheme| scheme.eq_ignore_ascii_case("basic "))
        .map(|_| &value[6..])?;
    let decoded = String::from_utf8_lossy(&decode64(rest)).into_owned();
    Some(match decoded.split_once(':') {
        Some((uid, secret)) => (Some(uid.to_owned()), Some(secret.to_owned())),
        None => (Some(decoded), None),
    })
}

/// `Client::Credentials.from_request(request, :from_basic, :from_params)`:
/// the one client a request names, if it names one, from an
/// `Authorization: Basic` header and `client_id`/`client_secret` both. A
/// bare `client_id` beside another method's secret is the same client
/// identifying itself; anything more is `MultipleClientAuthMethods`.
pub fn from_request(
    headers: &HeaderMap,
    client_id: Option<&str>,
    client_secret: Option<&str>,
) -> Result<Option<Credentials>, MultipleClientAuthMethods> {
    let extracted: Vec<Credentials> = [
        from_basic(headers),
        Some((
            client_id.map(str::to_owned),
            client_secret.map(str::to_owned),
        )),
    ]
    .into_iter()
    .flatten()
    // `extract`: credentials without a uid are none at all.
    .filter_map(|(uid, secret)| {
        Some(Credentials {
            uid: present(uid.as_deref())?,
            secret: present(secret.as_deref()),
        })
    })
    .collect();
    if extracted.iter().filter(|c| c.secret.is_some()).count() > 1 {
        return Err(MultipleClientAuthMethods);
    }
    if extracted.iter().any(|c| c.uid != extracted[0].uid) {
        return Err(MultipleClientAuthMethods);
    }
    Ok(extracted.into_iter().next())
}

/// `ActiveSupport::SecurityUtils.secure_compare`.
pub fn secure_compare(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// `Client.authenticate`, `Application.by_uid_and_secret`: the application
/// by its uid, taken without a secret only when it is a public client, and
/// otherwise only with its own.
pub async fn authenticate(
    state: &AppState,
    credentials: &Credentials,
) -> sqlx::Result<Option<crate::db::models::OauthApplication>> {
    let app = sqlx::query_as!(
        crate::db::models::OauthApplication,
        "SELECT * FROM oauth_applications WHERE uid = $1",
        credentials.uid,
    )
    .fetch_optional(&state.db)
    .await?;
    Ok(app.filter(|app| match &credentials.secret {
        None => !app.confidential,
        Some(secret) => secure_compare(&app.secret, secret),
    }))
}

/// A Doorkeeper `ErrorResponse`: the body, its status, and the headers it
/// sends (`Cache-Control`, and `WWW-Authenticate` naming the error).
pub fn error_response(status: StatusCode, error: &str, description: &str) -> Response {
    let mut response = (
        status,
        Json(serde_json::json!({ "error": error, "error_description": description })),
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache"),
    );
    // `sanitize_error_values`: what RFC 6750 does not allow becomes `_`.
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| match c {
                ' ' | '!' | '#'..='[' | ']'..='~' => c,
                _ => '_',
            })
            .collect()
    };
    if let Ok(value) = HeaderValue::from_str(&format!(
        "Bearer realm=\"Doorkeeper\", error=\"{}\", error_description=\"{}\"",
        sanitize(error),
        sanitize(description)
    )) {
        headers.insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

/// `invalid_request` with `multiple_client_auth_methods`.
pub fn multiple_methods_response() -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "The request utilizes more than one mechanism for authenticating the client.",
    )
}

/// `invalid_client`, a `401`.
pub fn invalid_client_response() -> Response {
    error_response(
        StatusCode::UNAUTHORIZED,
        "invalid_client",
        "Client authentication failed due to unknown client, no client authentication included, or unsupported authentication method.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, value.parse().unwrap());
        headers
    }

    #[test]
    fn basic_credentials_are_not_url_decoded() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode("a%20b:c+d");
        let found = from_request(&basic(&format!("Basic {encoded}")), None, None).unwrap();
        assert_eq!(
            found,
            Some(Credentials {
                uid: "a%20b".into(),
                secret: Some("c+d".into())
            })
        );
        // The scheme in any case; no colon, no secret.
        let encoded = base64::engine::general_purpose::STANDARD.encode("only-id");
        let found = from_request(&basic(&format!("bAsIc {encoded}")), None, None).unwrap();
        assert_eq!(found.unwrap().secret, None);
    }

    #[test]
    fn two_methods_with_secrets_are_refused() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode("id:secret");
        let headers = basic(&format!("Basic {encoded}"));
        assert_eq!(
            from_request(&headers, Some("id"), Some("secret")),
            Err(MultipleClientAuthMethods)
        );
        assert_eq!(
            from_request(&headers, Some("other"), None),
            Err(MultipleClientAuthMethods)
        );
        // The same client identifying itself again is fine.
        assert!(from_request(&headers, Some("id"), None).unwrap().is_some());
        assert_eq!(from_request(&HeaderMap::new(), Some(""), None), Ok(None));
    }
}
