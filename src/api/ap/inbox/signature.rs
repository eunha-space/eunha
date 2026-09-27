//! Authentication of inbound ActivityPub activities: HTTP Signature
//! verification, whose policy is feder's, and the FEP-8b32 Object Integrity
//! Proof fallback, plus the public-key fetch/refresh cache.

use serde_json::Value;

use crate::state::AppState;

/// Verify the HTTP Signature on an inbound activity, draft-cavage or RFC 9421.
///
/// feder's verification does the checking, in its three steps: read the
/// signature; hold it to the policy before any key is fetched — it covers the
/// request target, the host, when it was made and the body's digest, which has
/// to be sent and match; it was made within an hour; and it was made for this
/// instance's domain or one of its aliases; then check it against the key.
/// What stays here is what needs this instance: the rule that the key belongs
/// to the activity's actor's origin, and fetching the key, from `accounts` or
/// the actor document, refreshed once when it does not verify.
pub(super) async fn verify_inbound_signature(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    path_and_query: &str,
    body: &[u8],
    actor_uri: &str,
) -> Result<(), String> {
    use feder_runtime::verification::{self, Key, Policy, Request};

    let header_vec = crate::federation::signature::headers_to_vec(headers);
    let header_refs: Vec<(&str, &str)> = header_vec
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let request = Request {
        method: "POST",
        path_and_query,
        headers: &header_refs,
        body,
    };
    let hosts: Vec<&str> = std::iter::once(state.instance.domain.as_str())
        .chain(state.instance.aliases.iter().map(String::as_str))
        .collect();

    let signature = verification::parse(&request).map_err(|e| e.to_string())?;
    let key_actor = verification::key_owner(&signature.key_id).to_owned();

    // The signing key must have the same origin as the activity's actor, so a
    // valid signature from one server cannot authorize an activity attributed
    // to an actor on another.
    if !actor_uri.is_empty() && !feder_core::origin::same_origin(&key_actor, actor_uri) {
        return Err(format!(
            "signing key origin does not match actor ({key_actor} vs {actor_uri})"
        ));
    }

    verification::check(
        &signature,
        &request,
        &Policy::new(&hosts),
        chrono::Utc::now().timestamp(),
    )
    .map_err(|e| e.to_string())?;

    let pem = fetch_public_key(state, &key_actor, &signature.key_id)
        .await
        .map_err(|e| format!("could not fetch public key: {e}"))?;
    match verification::verify(&signature, &request, Key::RsaPem(&pem)) {
        Ok(()) => Ok(()),
        Err(first_err) => {
            let refreshed = refresh_public_key(state, &key_actor, &signature.key_id)
                .await
                .map_err(|e| format!("could not refresh public key: {e}"))?;
            verification::verify(&signature, &request, Key::RsaPem(&refreshed))
                .map_err(|e| format!("{first_err}; after key refresh: {e}"))
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

    // The signing key must have the same origin as the actor.
    if actor_uri.is_empty() || !feder_core::origin::same_origin(&verification_method, actor_uri) {
        return Err(format!(
            "proof key origin does not match actor ({verification_method} vs {actor_uri})"
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

async fn fetch_public_key(
    state: &AppState,
    actor_url: &str,
    key_id: &str,
) -> anyhow::Result<String> {
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
    let pem = public_key_from_actor(&actor, key_id)?;
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

async fn refresh_public_key(
    state: &AppState,
    actor_url: &str,
    key_id: &str,
) -> anyhow::Result<String> {
    let actor = crate::federation::fetch::signed_get_json(state, actor_url).await?;
    let pem = public_key_from_actor(&actor, key_id)?;

    sqlx::query!(
        "UPDATE accounts SET public_key = $2, updated_at = now() WHERE uri = $1 AND domain IS NOT NULL",
        actor_url,
        pem,
    )
    .execute(&state.db)
    .await?;

    Ok(pem)
}

/// The key `key_id` names, if the fetched actor publishes it as its own:
/// the actor has to vouch for the key as well as the key ID for the actor, or
/// anyone who can put a key document on a server could sign as any actor
/// there.
fn public_key_from_actor(actor: &Value, key_id: &str) -> anyhow::Result<String> {
    feder_runtime::verification::published_key_pem(actor, key_id)
        .ok_or_else(|| anyhow::anyhow!("actor does not publish key {key_id}"))
}
