//! JSON-LD contexts ojak does not ship, loaded to check a Linked Data
//! Signature as Mastodon's document loader loads them
//! (`JsonLdHelper#load_jsonld_context`).
//!
//! Mastodon turns a signed activity into RDF with the contexts it preloads
//! and fetches any other, keeping what it fetched in `Rails.cache` for 30
//! days under `jsonld:context:<url>`. Eunha keeps it in Redis under the same
//! name, behind the instance's key prefix, and fetches with ojak's
//! (`ojak::contexts::fetch`): a GET asking for `application/ld+json`, taken
//! only as a `200` of that type up to a megabyte, through the instance's
//! guarded client, which refuses private addresses unless the operator
//! allowed them (`allowed_private_networks`). How many one activity may
//! cause to be loaded, and for how long, is ojak's `contexts::Limits`.
//!
//! Only the Linked Data Signature check asks, for a relayed activity whose
//! signer is not on a blocked server; reading an activity never fetches a
//! context (docs/design/protocol.md).

use crate::state::AppState;

/// How long a fetched context is kept: Mastodon's `expires_in: 30.days`.
const CACHE_SECONDS: u64 = ojak::contexts::CACHE_TTL.as_secs();

/// The cache key for `iri`, as Mastodon names it.
fn cache_key(iri: &str) -> String {
    format!("jsonld:context:{iri}")
}

/// The context document `iri` names: from the cache, or fetched and kept
/// there. A context that is refused is not kept, so it is asked for again
/// next time, as Mastodon's cache keeps only what its block returned.
pub async fn load(state: &AppState, iri: &str) -> Result<String, ojak::contexts::Error> {
    let key = state.redis_keys.key(cache_key(iri));
    let mut redis = state.redis.clone();
    let cached: Option<String> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .inspect_err(
            |error| tracing::warn!(%error, iri, "could not read the JSON-LD context cache"),
        )
        .ok()
        .flatten();
    if let Some(body) = cached {
        return Ok(body);
    }
    let body = ojak::contexts::fetch(state.fetcher.client(), iri).await?;
    let stored: redis::RedisResult<()> = redis::cmd("SET")
        .arg(&key)
        .arg(&body)
        .arg("EX")
        .arg(CACHE_SECONDS)
        .query_async(&mut redis)
        .await;
    if let Err(error) = stored {
        tracing::warn!(%error, iri, "could not write the JSON-LD context cache");
    }
    Ok(body)
}
