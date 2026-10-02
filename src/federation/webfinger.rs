//! WebFinger (RFC 7033) lookup for ActivityPub actor discovery, through
//! ojak's fetcher.

/// Resolve a fediverse handle to an ActivityPub actor URL.
///
/// Asks `domain`'s WebFinger endpoint about `acct:{user}@{domain}`, with the
/// resource encoded, and returns the actor its first ActivityPub `self` link
/// names.
pub async fn resolve(
    fetcher: &ojak::fetch::Fetcher,
    user: &str,
    domain: &str,
) -> anyhow::Result<String> {
    let address = ojak::webfinger::Address::parse(&format!("{user}@{domain}"))
        .ok_or_else(|| anyhow::anyhow!("{user}@{domain} is not a handle"))?;
    let found = fetcher.webfinger(&address).await?;
    found
        .actor(None)
        .map(|actor| actor.to_string())
        .ok_or_else(|| {
            anyhow::anyhow!("no ActivityPub self link in WebFinger response for {address}")
        })
}

/// [`resolve`], as `ResolveAccountService` does it: a handle on a domain this
/// instance does not federate with (`domain_not_allowed?`) is not looked up.
pub async fn resolve_allowed(
    state: &crate::state::AppState,
    user: &str,
    domain: &str,
) -> anyhow::Result<String> {
    if super::moderation::domain_not_allowed(state, domain).await {
        anyhow::bail!("{domain} is not a domain this instance federates with");
    }
    resolve(&state.fetcher, user, domain).await
}
