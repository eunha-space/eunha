//! HTTP Message Signatures ([RFC 9421]) as FASP uses them, which is as
//! Mastodon's `Fasp::Request` and `Api::Fasp::BaseController` make them with
//! Linzer.
//!
//! Requests both ways are signed over `@method`, `@target-uri` and
//! `content-digest` with Ed25519, and every request carries a
//! `Content-Digest`, an empty body's included. Responses both ways are signed
//! over `@status` and `content-digest`. A response signature has no `keyid`:
//! the key is the registration's, known to both sides. Ojak's RFC 9421
//! module signs and verifies both; what is here is the policy Linzer applies
//! for Mastodon: how old a request's signature may be, and that an expired
//! one is refused.
//!
//! [RFC 9421]: https://www.rfc-editor.org/rfc/rfc9421.html

use axum::http::HeaderMap;
use ojak::sig::rfc9421::{SigningKey, VerifyingKey};

/// `Linzer.verify!(…, no_older_than: 5.minutes)`, as the provider API checks
/// requests.
pub const REQUEST_MAX_AGE_SECONDS: i64 = 5 * 60;

/// The `Content-Digest` of a body: `sha-256=:<base64>:`.
pub fn content_digest(body: &[u8]) -> String {
    ojak::sig::rfc9421::content_digest(body)
}

/// `Api::Fasp::BaseController::DIGEST_PATTERN`'s capture: the base64 digest
/// a `Content-Digest` gives for SHA-256, if it gives one.
pub fn sha256_member(header: &str) -> Option<&str> {
    let start = header.find("sha-256=:")? + "sha-256=:".len();
    let rest = &header[start..];
    let end = rest.find(':')?;
    Some(&rest[..end])
}

/// `Linzer.sign!(response, key:, components: %w(@status content-digest))`:
/// the `Signature-Input` and `Signature` of a response.
pub fn sign_response(
    status: u16,
    content_digest: &str,
    seed: &[u8; 32],
    now: i64,
) -> (String, String) {
    match ojak::sig::rfc9421::sign_response(
        status,
        content_digest,
        None,
        &SigningKey::Ed25519(seed),
        now,
    ) {
        Ok(signed) => (signed.signature_input, signed.signature),
        // An Ed25519 seed always signs.
        Err(error) => unreachable!("an Ed25519 signature failed: {error}"),
    }
}

/// `Linzer.verify!(response, key:)`: check a response's signature against the
/// provider's public key, rebuilding whatever it covers from the status and
/// the header fields. A signature that has expired is refused.
pub fn verify_response(
    status: u16,
    headers: &HeaderMap,
    key: &[u8; 32],
    now: i64,
) -> Result<(), String> {
    let signature_input = header(headers, "signature-input").ok_or("signature-input is missing")?;
    let signature = header(headers, "signature").ok_or("signature is missing")?;
    if ojak::sig::rfc9421::expires_at(&signature_input).is_some_and(|expires| expires < now) {
        return Err("Signature has expired or is invalid".into());
    }
    let fields = fields(headers);
    let fields: Vec<(&str, &str)> = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    ojak::sig::rfc9421::verify_response(
        status,
        &signature_input,
        &signature,
        &fields,
        &VerifyingKey::Ed25519(key),
    )
    .map_err(|e| e.to_string())
}

/// `Linzer.verify!(request, no_older_than: 5.minutes)` for a request a
/// provider made: the signature must name its key, be no older than five
/// minutes, not have expired, and verify against `key`.
pub fn verify_request(
    method: &str,
    target_uri: &str,
    headers: &HeaderMap,
    body: &[u8],
    key: &[u8; 32],
    now: i64,
) -> Result<(), String> {
    let signature_input = header(headers, "signature-input").ok_or("signature-input is missing")?;
    let signature = header(headers, "signature").ok_or("signature is missing")?;
    let created = ojak::sig::rfc9421::created_at(&signature_input)
        .ok_or("the signature has no created parameter")?;
    if created < now - REQUEST_MAX_AGE_SECONDS {
        return Err(format!(
            "Signature created more than {REQUEST_MAX_AGE_SECONDS} seconds ago"
        ));
    }
    if ojak::sig::rfc9421::expires_at(&signature_input).is_some_and(|expires| expires < now) {
        return Err("Signature has expired or is invalid".into());
    }
    let fields = fields(headers);
    let fields: Vec<(&str, &str)> = fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let digest = header(headers, "content-digest");
    ojak::sig::rfc9421::verify_request(
        method,
        target_uri,
        &signature_input,
        &signature,
        digest.as_deref(),
        body,
        &fields,
        &VerifyingKey::Ed25519(key),
    )
    .map_err(|e| e.to_string())
}

/// The `keyid` a request's `Signature-Input` names.
pub fn key_id(headers: &HeaderMap) -> Option<String> {
    ojak::sig::rfc9421::key_id(&header(headers, "signature-input")?)
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    let values: Vec<&str> = headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    (!values.is_empty()).then(|| values.join(", "))
}

/// Every header field that is text, by name.
fn fields(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn verifies_the_responses_it_signs() {
        let pem = super::super::keys::generate_private_key_pem().unwrap();
        let (seed, public) = super::super::keys::parse_private_key_pem(&pem).unwrap();
        let digest = content_digest(b"{}");
        let (input, signature) = sign_response(201, &digest, &seed, 1_700_000_000);
        assert_eq!(
            input,
            "sig1=(\"@status\" \"content-digest\");created=1700000000"
        );
        let map = headers(&[
            ("content-digest", &digest),
            ("signature-input", &input),
            ("signature", &signature),
        ]);
        verify_response(201, &map, &public, 1_700_000_010).unwrap();
        assert!(verify_response(200, &map, &public, 1_700_000_010).is_err());
        let other = headers(&[
            ("content-digest", &content_digest(b"[]")),
            ("signature-input", &input),
            ("signature", &signature),
        ]);
        assert!(verify_response(201, &other, &public, 1_700_000_010).is_err());
    }

    #[test]
    fn reads_the_sha256_member_of_a_digest() {
        assert_eq!(sha256_member("sha-256=:abc=:"), Some("abc="));
        assert_eq!(sha256_member("sha-512=:xyz:, sha-256=:abc=:"), Some("abc="));
        assert_eq!(sha256_member("sha-512=:xyz:"), None);
    }
}
