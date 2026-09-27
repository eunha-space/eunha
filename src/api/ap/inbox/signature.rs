//! Authentication of inbound ActivityPub activities: HTTP Signature
//! verification (with body-digest and clock-skew enforcement) and the FEP-8b32
//! Object Integrity Proof fallback, plus the public-key fetch/refresh cache.

use serde_json::Value;

use crate::state::AppState;

use super::same_host;

/// The lower-cased list of header names a `Signature` header claims to cover
/// (its `headers="…"` parameter). Empty when the parameter is absent.
fn signed_headers_list(sig_val: &str) -> Vec<String> {
    for part in sig_val.split(',') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("headers=") {
            return rest
                .trim_matches('"')
                .split_whitespace()
                .map(|s| s.to_ascii_lowercase())
                .collect();
        }
    }
    Vec::new()
}

/// True if an HTTP-date `Date` header is within `skew` of now (in either
/// direction). Rejects malformed dates.
fn date_within_skew(date_val: &str, skew: chrono::Duration) -> bool {
    let trimmed = date_val.trim();
    let parsed = chrono::NaiveDateTime::parse_from_str(trimmed, "%a, %d %b %Y %H:%M:%S GMT")
        .map(|n| n.and_utc())
        .or_else(|_| {
            chrono::DateTime::parse_from_rfc2822(trimmed).map(|d| d.with_timezone(&chrono::Utc))
        });
    match parsed {
        Ok(signed) => (chrono::Utc::now() - signed).abs() <= skew,
        Err(_) => false,
    }
}

/// Verify the HTTP Signature on an inbound activity. Returns `Ok(())` only when
/// the request is signed by a key belonging to the same host as the claimed
/// actor and the signature (and body digest) check out.
pub(super) async fn verify_inbound_signature(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    path_and_query: &str,
    body: &[u8],
    actor_uri: &str,
) -> Result<(), String> {
    let sig_val = headers
        .get("signature")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| "missing Signature header".to_string())?;

    // A `Signature-Input` alongside it means RFC 9421 rather than the cavage
    // draft: the two use the same `Signature` field name but nothing else in
    // common, so they are verified apart.
    if let Some(signature_input) = headers.get("signature-input").and_then(|h| h.to_str().ok()) {
        return verify_rfc9421_signature(
            state,
            headers,
            signature_input,
            sig_val,
            path_and_query,
            body,
            actor_uri,
        )
        .await;
    }

    let kid = crate::federation::signature::key_id_from_header(sig_val)
        .ok_or_else(|| "no keyId in Signature header".to_string())?;
    let key_actor = kid.split('#').next().unwrap_or(kid);

    // The signing key must belong to the same host as the activity's actor, so a
    // valid signature from one server cannot authorize an activity attributed to
    // an actor on another server.
    if !actor_uri.is_empty() && !same_host(key_actor, actor_uri) {
        return Err(format!(
            "signing key host does not match actor ({key_actor} vs {actor_uri})"
        ));
    }

    // feder's verify_request only checks the Digest if a Digest header happens to
    // be present, and never bounds the signature's age. Without the following an
    // attacker could sign only `date` and then swap the body (recomputing the
    // Digest header that the signature never covered), or replay a captured
    // request indefinitely. So, before doing any work:
    //   * require the signature to actually cover (request-target), host, date,
    //     and digest;
    //   * require a Digest header (feder then binds it to the body);
    //   * reject stale or future-dated requests (±1h skew, as Mastodon does).
    let covered = signed_headers_list(sig_val);
    for required in ["(request-target)", "host", "date", "digest"] {
        if !covered.iter().any(|h| h == required) {
            return Err(format!(
                "signature does not cover required header {required:?}"
            ));
        }
    }
    if headers.get("digest").is_none() {
        return Err("missing Digest header".to_string());
    }
    let date_val = headers
        .get("date")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| "missing Date header".to_string())?;
    if !date_within_skew(date_val, chrono::Duration::hours(1)) {
        return Err(format!(
            "Date header outside acceptable clock skew: {date_val:?}"
        ));
    }

    let pem = fetch_public_key(state, key_actor)
        .await
        .map_err(|e| format!("could not fetch public key: {e}"))?;

    let hdr_vec = crate::federation::signature::headers_to_vec(headers);
    let hdr_refs: Vec<(&str, &str)> = hdr_vec
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    match crate::federation::signature::verify_request(
        "post",
        path_and_query,
        &hdr_refs,
        body,
        &pem,
    ) {
        Ok(()) => Ok(()),
        Err(first_err) => {
            let refreshed_pem = refresh_public_key(state, key_actor)
                .await
                .map_err(|e| format!("could not refresh public key: {e}"))?;
            crate::federation::signature::verify_request(
                "post",
                path_and_query,
                &hdr_refs,
                body,
                &refreshed_pem,
            )
            .map_err(|e| format!("{first_err}; after key refresh: {e}"))
        }
    }
}

/// Verify an inbound [RFC 9421] signature.
///
/// Same obligations as the draft path: the key must belong to the actor's
/// host, the signature must cover the body, and it must not be old enough to
/// replay. What differs is where those come from — the covered component list
/// rather than a header list, `content-digest` rather than `Digest`, and the
/// signature's own `created` parameter rather than a `Date` header.
///
/// [RFC 9421]: https://www.rfc-editor.org/rfc/rfc9421.html
#[allow(clippy::too_many_arguments)]
async fn verify_rfc9421_signature(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    signature_input: &str,
    signature: &str,
    path_and_query: &str,
    body: &[u8],
    actor_uri: &str,
) -> Result<(), String> {
    let kid = feder_runtime::rfc9421::key_id(signature_input)
        .ok_or_else(|| "no keyid in Signature-Input".to_string())?;
    let key_actor = kid.split('#').next().unwrap_or(&kid).to_string();

    if !actor_uri.is_empty() && !same_host(&key_actor, actor_uri) {
        return Err(format!(
            "signing key host does not match actor ({key_actor} vs {actor_uri})"
        ));
    }

    // Without a covered digest the signature says nothing about the body, and
    // the activity is the body.
    let covered = feder_runtime::rfc9421::covered_components(signature_input)
        .ok_or_else(|| "malformed Signature-Input".to_string())?;
    for required in ["@method", "@target-uri", "content-digest"] {
        if !covered.iter().any(|c| c == required) {
            return Err(format!("signature does not cover {required:?}"));
        }
    }

    let created = feder_runtime::rfc9421::created_at(signature_input)
        .ok_or_else(|| "Signature-Input has no `created` parameter".to_string())?;
    let age = chrono::Utc::now().timestamp() - created;
    if age.abs() > chrono::Duration::hours(1).num_seconds() {
        return Err(format!("signature created {age}s away from now"));
    }

    // The sender signed the URL it addressed, which federation always reaches
    // over https however this process is fronted.
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .ok_or_else(|| "missing Host header".to_string())?;
    let target_uri = format!("https://{host}{path_and_query}");
    let content_digest = headers.get("content-digest").and_then(|h| h.to_str().ok());

    let verify = |pem: &str| {
        feder_runtime::rfc9421::verify_request(
            "post",
            &target_uri,
            signature_input,
            signature,
            content_digest,
            body,
            &feder_runtime::rfc9421::VerifyingKey::RsaPem(pem),
        )
    };

    let pem = fetch_public_key(state, &key_actor)
        .await
        .map_err(|e| format!("could not fetch public key: {e}"))?;
    match verify(&pem) {
        Ok(()) => Ok(()),
        Err(first_err) => {
            let refreshed = refresh_public_key(state, &key_actor)
                .await
                .map_err(|e| format!("could not refresh public key: {e}"))?;
            verify(&refreshed).map_err(|e| format!("{first_err}; after key refresh: {e}"))
        }
    }
}

/// Authenticate an inbound activity via its FEP-8b32 Object Integrity Proof.
///
/// Used as a fallback when the HTTP Signature can't be verified. The proof's
/// `verificationMethod` must live on the actor's host and be declared by the
/// actor as one of its `assertionMethod` keys, which binds the signing key to
/// the claimed actor.
pub(super) async fn verify_object_integrity(
    state: &AppState,
    activity: &serde_json::Value,
    actor_uri: &str,
) -> Result<(), String> {
    let (proof, cryptosuite, verification_method) =
        feder_runtime::integrity::extract_integrity_proof(activity)
            .ok_or_else(|| "no usable integrity proof".to_string())?;

    // The signing key must belong to the same host as the actor.
    if actor_uri.is_empty() || !same_host(&verification_method, actor_uri) {
        return Err(format!(
            "proof key host does not match actor ({verification_method} vs {actor_uri})"
        ));
    }

    // Fetch the actor document and confirm it declares this verification method
    // as an assertionMethod, then read the key it publishes there.
    let actor_doc = crate::federation::fetch::signed_get_json(state, actor_uri)
        .await
        .map_err(|e| format!("could not fetch actor for proof key: {e}"))?;
    let multibase = assertion_method_key(&actor_doc, &verification_method)
        .ok_or_else(|| format!("actor does not declare assertionMethod {verification_method}"))?;
    let public_key = feder_runtime::integrity::decode_multikey(&multibase)
        .map_err(|e| format!("invalid assertionMethod key: {e}"))?;

    feder_runtime::integrity::verify_object_integrity_proof(activity, &proof, &public_key)
        .map_err(|e| format!("{} proof: {e}", cryptosuite.as_str()))
}

/// Find the `publicKeyMultibase` of the `assertionMethod` whose id equals
/// `verification_method` in a fetched actor document. Handles the field being a
/// single Multikey object or an array of them.
fn assertion_method_key(actor: &serde_json::Value, verification_method: &str) -> Option<String> {
    let methods = actor.get("assertionMethod")?;
    let entries: Vec<&serde_json::Value> = match methods {
        serde_json::Value::Array(a) => a.iter().collect(),
        obj @ serde_json::Value::Object(_) => vec![obj],
        _ => return None,
    };
    entries.into_iter().find_map(|m| {
        if m.get("id").and_then(|v| v.as_str()) == Some(verification_method) {
            m.get("publicKeyMultibase")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        } else {
            None
        }
    })
}

async fn fetch_public_key(state: &AppState, actor_url: &str) -> anyhow::Result<String> {
    if let Some(pem) = sqlx::query_scalar!(
        "SELECT public_key FROM accounts WHERE uri = $1 AND public_key != ''",
        actor_url,
    )
    .fetch_optional(&state.db)
    .await?
    {
        return Ok(pem);
    }

    // Seen for the first time. The activity this request carries will need
    // the account, so create it from the document fetched for its key, as
    // Mastodon does, rather than leave the inbox worker to fetch the same
    // document again. A key published apart from its actor falls through to
    // the key alone.
    let actor = crate::federation::fetch::signed_get_json(state, actor_url).await?;
    let pem = public_key_from_actor(&actor)?;
    let is_the_actor =
        actor.get("id").and_then(Value::as_str) == Some(actor_url) && actor.get("inbox").is_some();
    if is_the_actor {
        // Two first activities from one actor race to create it; the loser's
        // insert fails on the account's uniqueness, and its worker finds the
        // winner's row. Verification needs only the key either way.
        if let Err(e) =
            super::fetch::resolve_or_fetch_remote_account_prefetched(state, actor_url, actor).await
        {
            tracing::debug!(actor = actor_url, error = %e, "account not created from its key fetch");
        }
    }
    Ok(pem)
}

async fn refresh_public_key(state: &AppState, actor_url: &str) -> anyhow::Result<String> {
    let actor = crate::federation::fetch::signed_get_json(state, actor_url).await?;
    let pem = public_key_from_actor(&actor)?;

    sqlx::query!(
        "UPDATE accounts SET public_key = $2, updated_at = now() WHERE uri = $1 AND domain IS NOT NULL",
        actor_url,
        pem,
    )
    .execute(&state.db)
    .await?;

    Ok(pem)
}

fn public_key_from_actor(actor: &Value) -> anyhow::Result<String> {
    Ok(actor
        .get("publicKey")
        .and_then(|k| k.get("publicKeyPem"))
        .and_then(|p| p.as_str())
        .ok_or_else(|| anyhow::anyhow!("no publicKeyPem"))?
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_headers_list_parses_covered_set() {
        let sig = r#"keyId="https://a.test/users/x#main-key",algorithm="rsa-sha256",headers="(request-target) Host Date Digest",signature="abc""#;
        let covered = signed_headers_list(sig);
        assert!(covered.contains(&"(request-target)".to_string()));
        assert!(covered.contains(&"host".to_string()));
        assert!(covered.contains(&"date".to_string()));
        assert!(covered.contains(&"digest".to_string()));
        assert!(signed_headers_list("keyId=\"x\",signature=\"y\"").is_empty());
    }

    #[test]
    fn date_within_skew_accepts_recent_and_rejects_stale() {
        let now = chrono::Utc::now();
        let fmt = "%a, %d %b %Y %H:%M:%S GMT";
        let recent = now.format(fmt).to_string();
        let stale = (now - chrono::Duration::hours(2)).format(fmt).to_string();
        let future = (now + chrono::Duration::hours(2)).format(fmt).to_string();
        assert!(date_within_skew(&recent, chrono::Duration::hours(1)));
        assert!(!date_within_skew(&stale, chrono::Duration::hours(1)));
        assert!(!date_within_skew(&future, chrono::Duration::hours(1)));
        assert!(!date_within_skew("garbage", chrono::Duration::hours(1)));
    }
}
