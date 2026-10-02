//! `Fasp::Request`: a signed request to a provider, and the check of its
//! signed answer.
//!
//! Every request is JSON both ways, carries a `Content-Digest` of its body —
//! an empty one for a request without — and an RFC 9421 signature over
//! `@method`, `@target-uri` and `content-digest`, keyed by the identifier the
//! provider gave at registration. An answer of 400 or more is refused, and so
//! is one whose `Content-Digest` is missing or not exactly the digest of its
//! body, or whose signature does not verify against the provider's key.
//!
//! Requests go out through the SSRF-guarded client, as Mastodon sends them
//! through `Request::Socket`. A request that cannot connect counts against
//! the provider's host at the resolution of minutes; one that is answered,
//! however, clears its failures.

use serde_json::Value;

use super::Provider;
use crate::state::AppState;

/// `Fasp::Request::COVERED_COMPONENTS`, for the record: ojak's signer covers
/// exactly these when a body, even an empty one, is given.
pub const COVERED_COMPONENTS: [&str; 3] = ["@method", "@target-uri", "content-digest"];

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `Mastodon::HTTP_CONNECTION_ERRORS`: the provider could not be reached.
    #[error("could not reach the provider: {0}")]
    Connection(String),
    /// `Mastodon::UnexpectedResponseError`.
    #[error("the provider answered {0}")]
    UnexpectedResponse(u16),
    /// `Mastodon::SignatureVerificationError` and `Linzer::VerifyError`.
    #[error("the provider's answer is not signed as it should be: {0}")]
    Signature(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl Error {
    pub fn is_connection(&self) -> bool {
        matches!(self, Self::Connection(_))
    }
}

pub async fn get(
    state: &AppState,
    provider: &Provider,
    path: &str,
) -> Result<Option<Value>, Error> {
    perform(state, provider, reqwest::Method::GET, path, None).await
}

pub async fn post(
    state: &AppState,
    provider: &Provider,
    path: &str,
    body: Option<&Value>,
) -> Result<Option<Value>, Error> {
    perform(state, provider, reqwest::Method::POST, path, body).await
}

pub async fn delete(
    state: &AppState,
    provider: &Provider,
    path: &str,
) -> Result<Option<Value>, Error> {
    perform(state, provider, reqwest::Method::DELETE, path, None).await
}

/// `body.present? ? body.to_json : ''`.
fn encode(body: Option<&Value>) -> String {
    match body {
        None | Some(Value::Null) => String::new(),
        Some(Value::Object(map)) if map.is_empty() => String::new(),
        Some(Value::Array(items)) if items.is_empty() => String::new(),
        Some(Value::String(s)) if s.trim().is_empty() => String::new(),
        Some(value) => value.to_string(),
    }
}

async fn perform(
    state: &AppState,
    provider: &Provider,
    method: reqwest::Method,
    path: &str,
    body: Option<&Value>,
) -> Result<Option<Value>, Error> {
    let url = provider.url(path);
    let body = encode(body);
    crate::federation::safe_fetch::validate_url(&url)?;
    let seed = provider.server_key()?;
    let signed = ojak::sig::rfc9421::sign_request(
        method.as_str(),
        &url,
        Some(body.as_bytes()),
        &provider.remote_identifier,
        &ojak::sig::rfc9421::SigningKey::Ed25519(&seed),
        chrono::Utc::now().timestamp(),
    )
    .map_err(|e| anyhow::anyhow!("could not sign the request: {e}"))?;
    let digest = signed
        .content_digest
        .unwrap_or_else(|| super::signature::content_digest(body.as_bytes()));

    let sent = state
        .fetch
        .request(method, &url)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .header("content-digest", digest)
        .header("signature-input", signed.signature_input)
        .header("signature", signed.signature)
        .body(body)
        .send()
        .await;
    let response = match sent {
        Ok(response) => response,
        Err(error) => {
            track_failure(state, provider).await;
            return Err(Error::Connection(error.to_string()));
        }
    };
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            track_failure(state, provider).await;
            return Err(Error::Connection(error.to_string()));
        }
    };

    validate(provider, status, &headers, &bytes)?;
    if let Some(host) = provider.host() {
        if let Err(error) = state.delivery_failures.track_success(&host).await {
            tracing::warn!(%error, host, "could not clear a FASP's failures");
        }
    }

    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| Error::Other(anyhow::anyhow!("the provider's answer is not JSON: {e}")))
}

async fn track_failure(state: &AppState, provider: &Provider) {
    if let Some(host) = provider.host() {
        if let Err(error) = state.delivery_failures.track_failure_minutes(&host).await {
            tracing::warn!(%error, host, "could not count a FASP's failure");
        }
    }
}

/// `Fasp::Request#validate!`.
fn validate(
    provider: &Provider,
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) -> Result<(), Error> {
    if status >= 400 {
        return Err(Error::UnexpectedResponse(status));
    }
    let digest = headers
        .get("content-digest")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| Error::Signature("content-digest missing".into()))?;
    if digest != super::signature::content_digest(body) {
        return Err(Error::Signature("content-digest does not match".into()));
    }
    if headers
        .get("signature-input")
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| v.trim().is_empty())
    {
        return Err(Error::Signature("signature-input is missing".into()));
    }
    let key = provider.provider_public_key()?;
    super::signature::verify_response(status, headers, &key, chrono::Utc::now().timestamp())
        .map_err(Error::Signature)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blank_body_is_sent_empty() {
        assert_eq!(encode(None), "");
        assert_eq!(encode(Some(&serde_json::json!({}))), "");
        assert_eq!(
            encode(Some(&serde_json::json!({"hello": "world"}))),
            r#"{"hello":"world"}"#
        );
    }
}
