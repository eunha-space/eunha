//! Outbound dereferencing of remote ActivityPub objects with HTTP Signatures.
//!
//! GETs are signed with the instance actor's key so that servers running in
//! authorized-fetch (secure) mode will serve the object. Servers that don't
//! require signatures simply ignore them.

use serde_json::Value;

use crate::state::AppState;

const AP_ACCEPT: &str = "application/activity+json, application/ld+json";

/// Issue a signed GET and hand back the response untouched, whatever its status
/// — [`crate::federation::fetch_resource`] needs to see a 404 or a `text/html`
/// answer rather than have it turned into an error.
pub async fn signed_get(
    state: &AppState,
    url: &str,
    accept: &str,
) -> anyhow::Result<reqwest::Response> {
    crate::federation::safe_fetch::validate_url(url)?;

    let key = crate::federation::instance_actor::signing_key(state).await?;
    let key_id = crate::federation::instance_actor::key_id(&state.instance.domain);

    let signed = feder_runtime::signature::sign_get_with_key(url, &key_id, &key)?;

    Ok(state
        .fetch
        .get(url)
        .header("Accept", accept)
        .header("Date", signed.date)
        .header("Signature", signed.signature)
        .send()
        .await?)
}

/// Fetch a remote ActivityPub object as JSON, signing the GET with the instance
/// actor's key.
pub async fn signed_get_json(state: &AppState, url: &str) -> anyhow::Result<Value> {
    let resp = signed_get(state, url, AP_ACCEPT)
        .await?
        .error_for_status()?;

    Ok(resp.json().await?)
}
