//! JSON-LD contexts ojak does not ship, loaded to check a Linked Data
//! Signature as Mastodon's document loader loads them
//! (`JsonLdHelper#load_jsonld_context`).
//!
//! Mastodon turns a signed activity into RDF with the contexts it preloads
//! and fetches any other, keeping what it fetched in `Rails.cache` for 30
//! days under `jsonld:context:<url>`. Eunha keeps it in Redis under the same
//! name, behind the instance's key prefix (`AppState::federation_kv`), with
//! ojak's `contexts::fetch_cached`: a GET asking for `application/ld+json`,
//! taken only as a `200` of that type up to a megabyte, through the
//! instance's guarded client, which refuses private addresses unless the
//! operator allowed them (`allowed_private_networks`). How many one activity
//! may cause to be loaded, and for how long, is ojak's `contexts::Limits`.
//!
//! Only the Linked Data Signature check asks, for a relayed activity whose
//! signer is not on a blocked server; reading an activity never fetches a
//! context (docs/design/protocol.md).

use crate::state::AppState;

/// The context document `iri` names: from the cache, or fetched and kept
/// there for Mastodon's `expires_in: 30.days`. A context that is refused is
/// not kept, so it is asked for again next time, as Mastodon's cache keeps
/// only what its block returned; a cache that cannot be reached is a miss,
/// as `Rails.cache` takes it.
pub async fn load(state: &AppState, iri: &str) -> Result<String, ojak::contexts::Error> {
    ojak::contexts::fetch_cached(
        state.federation_kv.as_ref(),
        state.fetcher.client(),
        iri,
        ojak::contexts::CACHE_TTL,
        |error| tracing::warn!(%error, iri, "the JSON-LD context cache failed"),
    )
    .await
}
