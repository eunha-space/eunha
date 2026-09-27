//! SSRF protection for outbound fetches of untrusted remote content.
//!
//! Remote ActivityPub objects, actor keys, and link-preview targets are all
//! attacker-influenced URLs. Without guarding, a hostile server could point us
//! at `http://169.254.169.254/…` (cloud metadata) or an internal service.
//!
//! Two layers close the holes:
//!   1. [`validate_url`] rejects non-HTTP(S) schemes and literal IP hosts in
//!      private/loopback/link-local/etc. ranges (reqwest does not run the DNS
//!      resolver for IP literals).
//!   2. [`PublicOnlyResolver`] filters DNS results to globally-routable
//!      addresses, so hostnames that resolve to private space are refused — at
//!      connect time, for the initial request *and* every redirect hop, which
//!      also defeats DNS-rebinding.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// True if `ip` is a globally-routable address we're willing to connect to.
/// Conservative: anything private, local, or special-use is rejected.
pub fn is_global_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_global_v4(v4),
        IpAddr::V6(v6) => is_global_v6(v6),
    }
}

fn is_global_v4(ip: Ipv4Addr) -> bool {
    // CGNAT 100.64.0.0/10 has no stable std predicate.
    let is_shared = ip.octets()[0] == 100 && (ip.octets()[1] & 0b1100_0000) == 0b0100_0000;
    // 192.0.0.0/24 (IETF protocol assignments) and 198.18.0.0/15 (benchmarking).
    let o = ip.octets();
    let is_protocol_assignment = o[0] == 192 && o[1] == 0 && o[2] == 0;
    let is_benchmarking = o[0] == 198 && (o[1] == 18 || o[1] == 19);

    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_unspecified()
        || ip.is_multicast()
        || is_shared
        || is_protocol_assignment
        || is_benchmarking)
}

fn is_global_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return false;
    }
    let seg = ip.segments();
    // Unique local addresses fc00::/7.
    let is_unique_local = (seg[0] & 0xfe00) == 0xfc00;
    // Link-local unicast fe80::/10.
    let is_link_local = (seg[0] & 0xffc0) == 0xfe80;
    // Documentation 2001:db8::/32.
    let is_documentation = seg[0] == 0x2001 && seg[1] == 0x0db8;
    // IPv4-mapped (::ffff:0:0/96) / IPv4-compatible: validate the embedded v4.
    if let Some(v4) = ip.to_ipv4() {
        return is_global_v4(v4);
    }
    !(is_unique_local || is_link_local || is_documentation)
}

/// Validate the initial request URL: HTTP(S) only, and reject literal IP hosts
/// that aren't globally routable. Hostnames are checked later by the resolver.
pub fn validate_url(url: &str) -> anyhow::Result<()> {
    let parsed = url::Url::parse(url).map_err(|e| anyhow::anyhow!("invalid URL {url:?}: {e}"))?;
    match parsed.scheme() {
        "https" | "http" => {}
        other => anyhow::bail!("refusing non-HTTP(S) URL scheme {other:?}"),
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("URL has no host: {url:?}"))?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_reachable(ip) {
            anyhow::bail!("refusing request to non-public IP {ip}");
        }
    }
    Ok(())
}

/// Networks an operator has declared reachable despite being private.
///
/// Mastodon has the same setting, `ALLOWED_PRIVATE_ADDRESSES`, and for the same
/// reason: an instance may legitimately need to reach a peer inside its own
/// network — split-horizon DNS, a reverse proxy on a LAN, a mesh network — and
/// without this it simply cannot federate there.
///
/// Empty by default. This weakens an SSRF protection exactly as far as it is
/// told to and no further, so it is set deliberately or not at all.
static ALLOWED_PRIVATE: std::sync::OnceLock<Vec<ipnet::IpNet>> = std::sync::OnceLock::new();

/// Declare the private networks this instance may reach. Call once at startup.
pub fn set_allowed_private_networks(nets: Vec<ipnet::IpNet>) {
    let _ = ALLOWED_PRIVATE.set(nets);
}

/// The private networks this process may reach: what the first call to
/// [`set_allowed_private_networks`] declared, and none before one.
pub fn allowed_private_networks() -> Vec<ipnet::IpNet> {
    ALLOWED_PRIVATE.get().cloned().unwrap_or_default()
}

fn is_allowed_private(ip: IpAddr) -> bool {
    ALLOWED_PRIVATE
        .get()
        .is_some_and(|nets| nets.iter().any(|net| net.contains(&ip)))
}

/// Whether an address may be connected to: globally routable, or inside a
/// network the operator has declared reachable.
fn is_reachable(ip: IpAddr) -> bool {
    is_global_ip(ip) || is_allowed_private(ip)
}

/// A reqwest DNS resolver that only yields reachable addresses, refusing the
/// lookup entirely when a name resolves solely into space it may not touch.
#[derive(Debug, Default)]
pub struct PublicOnlyResolver;

impl Resolve for PublicOnlyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            // Port 0: we only need address resolution, reqwest overrides the port.
            let resolved = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let public: Vec<SocketAddr> = resolved.filter(|addr| is_reachable(addr.ip())).collect();
            if public.is_empty() {
                let err: Box<dyn std::error::Error + Send + Sync> =
                    format!("{host} resolved only to non-public addresses").into();
                return Err(err);
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

/// A redirect policy that follows a bounded number of hops while rejecting any
/// hop to a non-HTTP(S) scheme or a literal non-public IP. Hostname hops are
/// still vetted by [`PublicOnlyResolver`] at connect time.
fn safe_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 4 {
            return attempt.error(anyhow::anyhow!("too many redirects"));
        }
        // Inspect the target before consuming `attempt`.
        let scheme = attempt.url().scheme().to_owned();
        let literal_ip = attempt
            .url()
            .host_str()
            .and_then(|h| h.parse::<IpAddr>().ok());
        if scheme != "https" && scheme != "http" {
            return attempt.error(anyhow::anyhow!("redirect to non-HTTP(S) scheme {scheme:?}"));
        }
        if let Some(ip) = literal_ip {
            if !is_global_ip(ip) {
                return attempt.error(anyhow::anyhow!("redirect to non-public IP {ip}"));
            }
        }
        attempt.follow()
    })
}

/// Build the dedicated HTTP client for fetching untrusted remote content.
pub fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(crate::version::USER_AGENT)
        .timeout(std::time::Duration::from_secs(30))
        .redirect(safe_redirect_policy())
        .dns_resolver(Arc::new(PublicOnlyResolver))
        .build()
        .expect("failed to build SSRF-guarded HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private address is refused unless the operator has named its network,
    /// and naming one network does not open the others.
    #[test]
    fn allowed_networks_open_only_what_they_name() {
        // Nothing declared: the default, and the answer for every private range.
        assert!(!is_reachable("10.0.0.5".parse().unwrap()));
        assert!(!is_reachable("192.168.1.1".parse().unwrap()));
        assert!(is_reachable("93.184.216.34".parse().unwrap()));

        set_allowed_private_networks(vec!["10.0.0.0/8".parse().unwrap()]);

        assert!(
            is_reachable("10.0.0.5".parse().unwrap()),
            "the named network"
        );
        assert!(
            !is_reachable("192.168.1.1".parse().unwrap()),
            "a private network that was not named stays refused"
        );
        assert!(
            !is_reachable("127.0.0.1".parse().unwrap()),
            "loopback is not covered by 10.0.0.0/8"
        );
        assert!(
            is_reachable("93.184.216.34".parse().unwrap()),
            "public is unaffected"
        );
    }

    #[test]
    fn rejects_private_and_special_v4() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254", // cloud metadata / link-local
            "100.64.0.1",      // CGNAT
            "0.0.0.0",
            "192.0.0.1",
            "198.18.0.1",
            "224.0.0.1", // multicast
        ] {
            assert!(!is_global_ip(ip.parse().unwrap()), "{ip} should be blocked");
        }
    }

    #[test]
    fn allows_public_v4() {
        for ip in ["1.1.1.1", "8.8.8.8", "93.184.216.34"] {
            assert!(is_global_ip(ip.parse().unwrap()), "{ip} should be allowed");
        }
    }

    #[test]
    fn rejects_private_and_special_v6() {
        for ip in [
            "::1",             // loopback
            "::",              // unspecified
            "fc00::1",         // unique local
            "fe80::1",         // link-local
            "2001:db8::1",     // documentation
            "::ffff:10.0.0.1", // v4-mapped private
            "::ffff:169.254.0.1",
            "ff02::1", // multicast
        ] {
            assert!(!is_global_ip(ip.parse().unwrap()), "{ip} should be blocked");
        }
    }

    #[test]
    fn allows_public_v6() {
        assert!(is_global_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[test]
    fn validate_url_rejects_scheme_and_literal_ips() {
        assert!(validate_url("https://example.com/users/foo").is_ok());
        assert!(validate_url("http://example.com").is_ok());
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("https://169.254.169.254/latest/meta-data").is_err());
        assert!(validate_url("http://127.0.0.1:8080/").is_err());
        assert!(validate_url("not a url").is_err());
    }
}
