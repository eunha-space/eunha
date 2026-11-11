//! Which addresses eunha refuses to fetch, beyond what Mastodon refuses.

use url::Url;

fn refused(url: &str) -> bool {
    ojak::client::validate_url(&Url::parse(url).unwrap(), &[]).is_err()
}

/// Mastodon judges an IPv4-compatible address by the IPv4 address inside it
/// (`IPAddr#native`), so `::8.8.8.8` is fetched; ojak refuses the whole
/// deprecated `::/96` range.
#[test]
fn ipv4_compatible_addresses_are_refused() {
    assert!(refused("http://[::808:808]/"));
    assert!(refused("http://[::a00:1]/"));
}

/// Mastodon's list has no entry for the deprecated site-local `fec0::/10`, and
/// Ruby's `IPAddr#private?` covers only `fc00::/7`; ojak refuses it.
#[test]
fn site_local_addresses_are_refused() {
    assert!(refused("http://[fec0::1]/"));
}

/// Both servers fetch an ordinary public address.
#[test]
fn public_addresses_are_fetched() {
    assert!(!refused("http://[2606:4700::1111]/"));
    assert!(!refused("http://8.8.8.8/"));
}
