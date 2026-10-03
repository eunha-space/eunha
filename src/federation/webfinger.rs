//! WebFinger (RFC 7033) lookup for ActivityPub actor discovery, through
//! ojak's fetcher, which asks an onion service over `http`, follows a
//! handle's one redirect and checks that it names the actor
//! (`ojak::webfinger`). What is Mastodon's policy rather than WebFinger's —
//! which usernames are valid, which domains are asked — stays here.

use std::sync::atomic::{AtomicBool, Ordering};

/// Whether WebFinger is asked over plain `http`, which only the test
/// binaries that run fake remote servers on `127.0.0.1` turn on.
static PLAIN_HTTP: AtomicBool = AtomicBool::new(false);

/// Ask every WebFinger query over plain `http`, for a test binary whose
/// remote servers cannot serve `https`: each instance's fetcher is built
/// with `Fetcher::with_plain_http_webfinger` from then on. Process-wide,
/// like the SSRF guard's allowlist it goes with.
pub fn use_plain_http_for_tests() {
    PLAIN_HTTP.store(true, Ordering::SeqCst);
}

/// Whether [`use_plain_http_for_tests`] was called.
pub fn plain_http() -> bool {
    PLAIN_HTTP.load(Ordering::SeqCst)
}

fn address(user: &str, domain: &str) -> anyhow::Result<ojak::webfinger::Address> {
    ojak::webfinger::Address::parse(&format!("{user}@{domain}"))
        .ok_or_else(|| anyhow::anyhow!("{user}@{domain} is not a handle"))
}

/// `ResolveAccountService#process_webfinger!`, through ojak: ask about
/// `user@domain`, following one redirect to a handle that names itself, and
/// return the canonical handle and the actor it names. A `410 Gone` is an
/// error whose [`ojak::webfinger::ResolveError::status`] says so.
pub async fn resolve_handle(
    fetcher: &ojak::fetch::Fetcher,
    user: &str,
    domain: &str,
) -> anyhow::Result<ojak::webfinger::Resolved> {
    Ok(fetcher.resolve(&address(user, domain)?).await?)
}

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
    let found = fetcher.webfinger(&address(user, domain)?).await?;
    found
        .actor(None)
        .map(|actor| actor.to_string())
        .ok_or_else(|| {
            anyhow::anyhow!("no ActivityPub self link in WebFinger response for {user}@{domain}")
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

/// `Account::USERNAME_ONLY_RE`: `[a-z0-9_]+([.-]+[a-z0-9_]+)*`, any case.
pub fn is_valid_username(username: &str) -> bool {
    let mut previous_separator = true;
    let mut seen = false;
    for byte in username.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' => previous_separator = false,
            b'.' | b'-' if seen && !previous_separator => previous_separator = true,
            b'.' | b'-' if seen => {}
            _ => return false,
        }
        seen = true;
    }
    seen && !previous_separator
}

/// `ProcessAccountService#check_webfinger!`: whether WebFinger agrees that
/// `username@domain` is the actor `uri`, following one redirect to the
/// handle the host says is canonical (ojak's `Fetcher::confirm`), and
/// whether that handle's username is one Mastodon takes. Returns the handle
/// it confirmed: the one asked about when the host agrees with it, without
/// regard to case, or the one it redirected to.
pub async fn confirm(
    fetcher: &ojak::fetch::Fetcher,
    username: &str,
    domain: &str,
    uri: &str,
) -> anyhow::Result<(String, String)> {
    let resolved = fetcher.confirm(&address(username, domain)?, uri).await?;
    let (confirmed_username, confirmed_domain) = (resolved.address.user(), resolved.address.host());
    if !is_valid_username(confirmed_username) {
        anyhow::bail!(
            "Unsupported username format in webfinger response for {confirmed_username}@{confirmed_domain}"
        );
    }
    if username.eq_ignore_ascii_case(confirmed_username)
        && domain.eq_ignore_ascii_case(confirmed_domain)
    {
        return Ok((username.to_owned(), domain.to_owned()));
    }
    Ok((confirmed_username.to_owned(), confirmed_domain.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usernames_are_read_as_mastodon_reads_them() {
        for valid in ["alice", "Alice_1", "a.b", "a-b.c", "a..b", "_"] {
            assert!(is_valid_username(valid), "{valid}");
        }
        for invalid in ["", ".a", "a.", "a b", "알리스", "a@b", "-a", "a-"] {
            assert!(!is_valid_username(invalid), "{invalid}");
        }
    }
}
