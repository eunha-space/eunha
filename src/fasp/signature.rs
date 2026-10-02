//! HTTP Message Signatures ([RFC 9421]) as FASP uses them, which is as
//! Mastodon's `Fasp::Request` and `Api::Fasp::BaseController` make them with
//! Linzer.
//!
//! Requests both ways are signed over `@method`, `@target-uri` and
//! `content-digest` with Ed25519, and every request carries a
//! `Content-Digest`, an empty body's included — ojak's RFC 9421 signer and
//! verifier handle those. Responses both ways are signed over `@status` and
//! `content-digest`, which ojak's verifier, written for requests, cannot
//! rebuild, so responses are signed and verified here. A response signature
//! has no `keyid`: the key is the registration's, known to both sides.
//!
//! [RFC 9421]: https://www.rfc-editor.org/rfc/rfc9421.html

use axum::http::HeaderMap;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};

/// Linzer's `DEFAULT_LABEL`.
const LABEL: &str = "sig1";

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
    use ed25519_dalek::Signer as _;
    let params = format!("(\"@status\" \"content-digest\");created={now}");
    let base = format!(
        "\"@status\": {status}\n\"content-digest\": {content_digest}\n\"@signature-params\": {params}"
    );
    let key = ed25519_dalek::SigningKey::from_bytes(seed);
    let signature = BASE64.encode(key.sign(base.as_bytes()).to_bytes());
    (
        format!("{LABEL}={params}"),
        format!("{LABEL}=:{signature}:"),
    )
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
    let input = Input::parse(&signature_input)?;
    if input
        .integer("expires")
        .is_some_and(|expires| expires < now)
    {
        return Err("Signature has expired or is invalid".into());
    }
    let mut base = String::new();
    for name in &input.covered {
        let value = match name.as_str() {
            "@status" => status.to_string(),
            derived if derived.starts_with('@') => {
                return Err(format!("cannot verify the component {derived:?}"));
            }
            field => {
                let values: Vec<String> = headers
                    .get_all(field)
                    .iter()
                    .filter_map(|v| v.to_str().ok())
                    .map(|v| v.trim().to_owned())
                    .collect();
                if values.is_empty() {
                    return Err(format!("Missing component in message: {field:?}"));
                }
                values.join(", ")
            }
        };
        base.push_str(&format!("\"{name}\": {value}\n"));
    }
    base.push_str(&format!("\"@signature-params\": {}", input.params));
    let raw = signature_bytes(&signature, &input.label)?;
    verify_ed25519(key, base.as_bytes(), &raw)
}

/// `Linzer.verify!(request, no_older_than: 5.minutes)` for a request a
/// provider made: the signature must name its key, be no older than five
/// minutes, not have expired, and verify against `key`.
#[allow(clippy::too_many_arguments)]
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
    let fields: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect();
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
        &ojak::sig::rfc9421::VerifyingKey::Ed25519(key),
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

fn verify_ed25519(key: &[u8; 32], message: &[u8], signature: &[u8]) -> Result<(), String> {
    use ed25519_dalek::Verifier as _;
    let signature = ed25519_dalek::Signature::from_slice(signature)
        .map_err(|e| format!("Ed25519 signature: {e}"))?;
    ed25519_dalek::VerifyingKey::from_bytes(key)
        .map_err(|e| format!("Ed25519 public key: {e}"))?
        .verify(message, &signature)
        .map_err(|_| "Failed to verify message: Invalid signature.".to_owned())
}

/// The first signature a `Signature-Input` describes, read as the
/// structured-field dictionary it is.
struct Input {
    label: String,
    covered: Vec<String>,
    /// The `@signature-params` line as RFC 8941 serializes it.
    params: String,
    parameters: sfv::Parameters,
}

impl Input {
    fn parse(header: &str) -> Result<Self, String> {
        let malformed = |why: String| format!("Signature-Input: {why}");
        let dictionary = sfv::Parser::new(header)
            .parse::<sfv::Dictionary>()
            .map_err(|e| malformed(e.to_string()))?;
        let (label, entry) = dictionary
            .first()
            .ok_or_else(|| malformed("no signature".into()))?;
        let sfv::ListEntry::InnerList(list) = entry else {
            return Err(malformed("not a component list".into()));
        };
        let mut covered = Vec::with_capacity(list.items.len());
        for item in &list.items {
            let name = item
                .bare_item
                .as_string()
                .ok_or_else(|| malformed("a covered component is not a string".into()))?;
            if !item.params.is_empty() {
                return Err(format!(
                    "cannot verify the component {:?} with parameters",
                    name.as_str()
                ));
            }
            let name = name.as_str().to_owned();
            if covered.contains(&name) {
                return Err(malformed(format!("{name:?} is covered twice")));
            }
            covered.push(name);
        }
        let mut serializer = sfv::ListSerializer::new();
        serializer.members([entry]);
        let params = serializer
            .finish()
            .ok_or_else(|| malformed("no signature".into()))?;
        Ok(Self {
            label: label.as_str().to_owned(),
            covered,
            params,
            parameters: list.params.clone(),
        })
    }

    fn integer(&self, name: &str) -> Option<i64> {
        let key = sfv::KeyRef::from_str(name).ok()?;
        Some(self.parameters.get(key)?.as_integer()?.into())
    }
}

/// The signature published under `label` in a `Signature` field.
fn signature_bytes(header: &str, label: &str) -> Result<Vec<u8>, String> {
    let malformed = |why: String| format!("Signature: {why}");
    let dictionary = sfv::Parser::new(header)
        .parse::<sfv::Dictionary>()
        .map_err(|e| malformed(e.to_string()))?;
    let key = sfv::KeyRef::from_str(label).map_err(|e| malformed(e.to_string()))?;
    match dictionary.get(key) {
        Some(sfv::ListEntry::Item(item)) => item
            .bare_item
            .as_byte_sequence()
            .map(<[u8]>::to_vec)
            .ok_or_else(|| malformed("not a byte sequence".into())),
        Some(sfv::ListEntry::InnerList(_)) => Err(malformed("not a byte sequence".into())),
        None => Err(malformed(format!("no signature labelled {label:?}"))),
    }
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
