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

#[cfg(test)]
mod tests {
    use super::*;

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
