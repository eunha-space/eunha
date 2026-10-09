//! `TagManager#normalize_domain`: a domain as Mastodon stores and compares
//! it, wherever one is typed in or taken from a handle.

use ojak::origin::{normalize_host, Port};

/// A domain `Addressable::URI#host=` refuses (`Addressable::URI::
/// InvalidURIError`): one with a space, `/`, `@` or the like in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidDomain;

impl std::fmt::Display for InvalidDomain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invalid character in host")
    }
}

impl std::error::Error for InvalidDomain {}

/// `TagManager#normalize_domain`: stripped, one trailing slash removed, then
/// Addressable's normalized host — lower case, percent-decoded, in its ASCII
/// form, a single trailing dot dropped — with any port kept as it was
/// written. A blank domain is `""`, as Addressable normalizes it; what
/// Addressable refuses is an error, which each caller handles as Mastodon
/// does there.
pub fn normalize_domain(domain: &str) -> Result<String, InvalidDomain> {
    let domain = domain.trim();
    let domain = domain.strip_suffix('/').unwrap_or(domain);
    if domain.is_empty() {
        return Ok(String::new());
    }
    normalize_host(domain, Port::Keep).ok_or(InvalidDomain)
}

/// `DomainValidator#compliant?`: Addressable's normalized host shorter than
/// 256 characters, every dot-separated label one to 63 letters, digits or
/// hyphens.
pub fn compliant_domain(domain: &str) -> bool {
    // `Addressable::URI#host=` refuses a `:`, where a URL would read the rest
    // as a port and keep only what came before it.
    if domain.contains(':') {
        return false;
    }
    let Ok(url) = url::Url::parse(&format!("https://{domain}/")) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    host.len() < 256
        && host.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compliant_as_domain_validator_is() {
        assert!(compliant_domain("blocked.example"));
        assert!(compliant_domain("xn--bcher-kva.example"));
        assert!(!compliant_domain(""));
        assert!(!compliant_domain("a..example"));
        assert!(!compliant_domain("a_b.example"));
        assert!(!compliant_domain("a.example:443"));
    }

    #[test]
    fn normalizes_as_tag_manager_does() {
        assert_eq!(normalize_domain(" Example.COM/ ").unwrap(), "example.com");
        assert_eq!(
            normalize_domain("bücher.example").unwrap(),
            "xn--bcher-kva.example"
        );
        assert_eq!(
            normalize_domain("127.0.0.1:3000").unwrap(),
            "127.0.0.1:3000"
        );
        assert_eq!(normalize_domain("a.example:443").unwrap(), "a.example:443");
        assert_eq!(normalize_domain("  ").unwrap(), "");
        // Only one slash is taken off; the next is in the host.
        assert_eq!(normalize_domain("a.example//"), Err(InvalidDomain));
        assert_eq!(normalize_domain("a b.example"), Err(InvalidDomain));
    }
}
