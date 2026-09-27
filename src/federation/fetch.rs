//! Outbound dereferencing of remote ActivityPub objects with HTTP Signatures,
//! through feder's fetcher.
//!
//! GETs are signed with the instance actor's key so that servers running in
//! authorized-fetch (secure) mode will serve the object. Servers that don't
//! require signatures simply ignore them. The fetcher follows redirects
//! itself, checking each hop and signing it again for where it goes, and
//! retries a refused signature once in the other scheme.

use serde_json::Value;

use crate::state::AppState;

/// The instance actor's signing key, as feder takes it.
pub(crate) async fn instance_key(state: &AppState) -> anyhow::Result<feder::delivery::SenderKey> {
    Ok(feder::delivery::SenderKey {
        key_id: crate::federation::instance_actor::key_id(&state.instance.domain),
        private_key: crate::federation::instance_actor::signing_key(state).await?,
    })
}

fn parse(url: &str) -> anyhow::Result<url::Url> {
    url::Url::parse(url).map_err(|e| anyhow::anyhow!("invalid URL {url:?}: {e}"))
}

/// Issue a signed GET and hand back the response untouched, whatever its status
/// — [`crate::federation::fetch_resource`] needs to see a 404 or a `text/html`
/// answer rather than have it turned into an error. The response's `url` is
/// where it was finally served from, after any redirects.
pub async fn signed_get(
    state: &AppState,
    url: &str,
    accept: &str,
) -> anyhow::Result<feder::client::Response> {
    let url = parse(url)?;
    let key = instance_key(state).await?;
    Ok(state.fetcher.get(&url, accept, Some(&key)).await?)
}

/// Fetch a remote ActivityPub object as JSON, signing the GET with the instance
/// actor's key.
///
/// The document is trusted only as feder establishes it: served as
/// ActivityPub, with an `id` on the origin it was finally served from. One
/// that names an `id` on another origin is refused, as Mastodon refuses it;
/// a caller that has to follow such an `id` asks for it by that `id`.
pub async fn signed_get_json(state: &AppState, url: &str) -> anyhow::Result<Value> {
    let key = instance_key(state).await?;
    // A portable id names a key, not a host: feder asks the gateways it
    // hints at, and keeps a copy only when its proof by the key holds.
    if let Some(portable) = feder_core::portable::ApUri::parse(url) {
        return Ok(state
            .fetcher
            .portable(&portable, &[], Some(&key))
            .await?
            .json);
    }
    let url = parse(url)?;
    Ok(state.fetcher.document(&url, Some(&key)).await?.json)
}
