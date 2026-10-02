//! WebFinger (RFC 7033) lookup for ActivityPub actor discovery, through
//! ojak's fetcher.

use std::sync::atomic::{AtomicBool, Ordering};

/// Whether WebFinger is asked over plain `http`, which only the test
/// binaries that run fake remote servers on `127.0.0.1` turn on.
static PLAIN_HTTP: AtomicBool = AtomicBool::new(false);

/// Ask every WebFinger query over plain `http`, for a test binary whose
/// remote servers cannot serve `https`. Process-wide, like the SSRF guard's
/// allowlist it goes with.
pub fn use_plain_http_for_tests() {
    PLAIN_HTTP.store(true, Ordering::SeqCst);
}

/// `Webfinger#standard_url`: `https`, except for an onion service, which is
/// asked over `http` as Mastodon asks it.
fn query_url(address: &ojak::webfinger::Address) -> url::Url {
    let mut url = address.webfinger_url();
    let host = address.host();
    let onion = host
        .split(':')
        .next()
        .is_some_and(|name| name.ends_with(".onion"));
    if onion || PLAIN_HTTP.load(Ordering::SeqCst) {
        // A scheme change between two special schemes always succeeds.
        let _ = url.set_scheme("http");
    }
    url
}

/// What a host's WebFinger endpoint says of `user@domain`.
pub async fn lookup(
    fetcher: &ojak::fetch::Fetcher,
    user: &str,
    domain: &str,
) -> anyhow::Result<ojak::webfinger::Found> {
    let address = ojak::webfinger::Address::parse(&format!("{user}@{domain}"))
        .ok_or_else(|| anyhow::anyhow!("{user}@{domain} is not a handle"))?;
    Ok(fetcher.webfinger_at(&query_url(&address)).await?)
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
    let found = lookup(fetcher, user, domain).await?;
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

/// `ProcessAccountService#split_acct`: an `acct:` URI or a bare handle, as
/// its user and host.
pub fn split_acct(acct: &str) -> (String, String) {
    let acct = acct.strip_prefix("acct:").unwrap_or(acct);
    let mut parts = acct.split('@');
    (
        parts.next().unwrap_or_default().to_owned(),
        parts.next().unwrap_or_default().to_owned(),
    )
}

/// `ProcessAccountService#check_webfinger!`: whether WebFinger agrees that
/// `username@domain` is the actor `uri`, following one redirect to the
/// handle the host says is canonical. Returns the handle it confirmed.
pub async fn confirm(
    fetcher: &ojak::fetch::Fetcher,
    username: &str,
    domain: &str,
    uri: &str,
) -> anyhow::Result<(String, String)> {
    let self_link = |found: &ojak::webfinger::Found| {
        found
            .actors
            .first()
            .map(|named| named.id.to_string())
            .unwrap_or_default()
    };
    let subject = |found: &ojak::webfinger::Found, asked: &str| {
        found
            .subject
            .clone()
            .filter(|subject| !subject.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Missing subject in response for {asked}"))
    };
    let asked = format!("{username}@{domain}");
    let found = lookup(fetcher, username, domain).await?;
    let (confirmed_username, confirmed_domain) = split_acct(&subject(&found, &asked)?);
    if !is_valid_username(&confirmed_username) {
        anyhow::bail!("Unsupported username format in webfinger response for {asked}");
    }
    if username.eq_ignore_ascii_case(&confirmed_username)
        && domain.eq_ignore_ascii_case(&confirmed_domain)
    {
        if self_link(&found) != uri {
            anyhow::bail!("Webfinger response for {asked} does not loop back to {uri}");
        }
        return Ok((username.to_owned(), domain.to_owned()));
    }

    let redirected = format!("{confirmed_username}@{confirmed_domain}");
    let found = lookup(fetcher, &confirmed_username, &confirmed_domain).await?;
    let (username, domain) = split_acct(&subject(&found, &redirected)?);
    if !confirmed_username.eq_ignore_ascii_case(&username)
        || !confirmed_domain.eq_ignore_ascii_case(&domain)
    {
        anyhow::bail!(
            "Too many webfinger redirects for URI {uri} (stopped at {username}@{domain})"
        );
    }
    if self_link(&found) != uri {
        anyhow::bail!("Webfinger response for {username}@{domain} does not loop back to {uri}");
    }
    if !is_valid_username(&username) {
        anyhow::bail!("Unsupported username format in webfinger response for {username}@{domain}");
    }
    Ok((username, domain))
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

    #[test]
    fn an_acct_is_split_like_ruby_splits_it() {
        assert_eq!(
            split_acct("acct:alice@example.com"),
            ("alice".into(), "example.com".into())
        );
        assert_eq!(split_acct("alice"), ("alice".into(), String::new()));
        assert_eq!(
            split_acct("a@b@c"),
            ("a".into(), "b".into()),
            "only the first two parts count"
        );
    }
}
