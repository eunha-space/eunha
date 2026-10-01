//! The client's address, as Rails' `ActionDispatch::RemoteIp` works it out,
//! and Mastodon's `IpBlock` checks on it.
//!
//! Proxies are trusted when they are loopback or private addresses, as Rails'
//! defaults have it, or listed in `TRUSTED_PROXY_IP` (comma-separated
//! addresses or CIDR ranges), as Mastodon's configuration adds.

use std::net::{IpAddr, SocketAddr};
use std::sync::OnceLock;

use axum::{
    extract::{ConnectInfo, Request},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};

use crate::state::AppState;

/// The client address of a request, when one could be told.
#[derive(Debug, Clone, Copy)]
pub struct ClientIp(pub Option<IpAddr>);

/// An address range: an address and how many leading bits of it count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Option<Self> {
        let (addr, prefix) = match s.trim().split_once('/') {
            Some((a, p)) => (a.parse::<IpAddr>().ok()?, Some(p.parse::<u8>().ok()?)),
            None => (s.trim().parse::<IpAddr>().ok()?, None),
        };
        let max = if addr.is_ipv4() { 32 } else { 128 };
        Some(Self {
            addr,
            prefix: prefix.unwrap_or(max).min(max),
        })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match (self.addr, ip) {
            (IpAddr::V4(_), IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
                Some(v4) => IpAddr::V4(v4),
                None => return false,
            },
            _ => ip,
        };
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// `ActionDispatch::RemoteIp::TRUSTED_PROXIES`, and `TRUSTED_PROXY_IP`.
fn trusted_proxies() -> &'static [Cidr] {
    static TRUSTED: OnceLock<Vec<Cidr>> = OnceLock::new();
    TRUSTED.get_or_init(|| {
        let mut trusted: Vec<Cidr> = [
            "127.0.0.0/8",
            "::1/128",
            "fc00::/7",
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "169.254.0.0/16",
            "fe80::/10",
        ]
        .iter()
        .filter_map(|c| Cidr::parse(c))
        .collect();
        if let Ok(extra) = std::env::var("TRUSTED_PROXY_IP") {
            trusted.extend(extra.split(',').filter_map(Cidr::parse));
        }
        trusted
    })
}

fn trusted(ip: IpAddr) -> bool {
    trusted_proxies().iter().any(|c| c.contains(ip))
}

/// `ActionDispatch::RemoteIp::GetIp#calculate_ip`: of the peer and the
/// addresses `X-Forwarded-For` lists, the nearest one that is not a trusted
/// proxy; or, if all are, the farthest.
pub fn calculate(peer: Option<IpAddr>, forwarded_for: Option<&str>) -> Option<IpAddr> {
    let forwarded: Vec<IpAddr> = forwarded_for
        .map(|v| {
            v.split(',')
                .filter_map(|s| s.trim().parse::<IpAddr>().ok())
                .collect()
        })
        .unwrap_or_default();
    // Nearest first: the peer, then the forwarded chain from its right end.
    let chain: Vec<IpAddr> = peer
        .into_iter()
        .chain(forwarded.iter().rev().copied())
        .collect();
    chain
        .iter()
        .copied()
        .find(|ip| !trusted(*ip))
        .or_else(|| chain.last().copied())
}

/// Put the [`ClientIp`] on every request.
pub async fn layer(mut req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    let forwarded = req
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok());
    let ip = calculate(peer, forwarded);
    req.extensions_mut().insert(ClientIp(ip));
    next.run(req).await
}

/// `IpBlock` severities, read with their expiry.
async fn blocked_with(state: &AppState, ip: IpAddr, severity: i32) -> bool {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM ip_blocks
             WHERE severity = $2 AND $1::inet <<= ip
               AND (expires_at IS NULL OR expires_at > now())
           ) AS "e!""#,
        ip.to_string() as _,
        severity,
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(false)
}

/// `IpBlock.severity_sign_up_block.containing(ip).exists?`.
pub async fn sign_up_blocked(state: &AppState, ip: Option<IpAddr>) -> bool {
    match ip {
        Some(ip) => blocked_with(state, ip, crate::db::models::ip_severity::SIGN_UP_BLOCK).await,
        None => false,
    }
}

/// `IpBlock.severity_sign_up_requires_approval.containing(ip).exists?`.
pub async fn sign_up_requires_approval(state: &AppState, ip: Option<IpAddr>) -> bool {
    match ip {
        Some(ip) => {
            blocked_with(
                state,
                ip,
                crate::db::models::ip_severity::SIGN_UP_REQUIRES_APPROVAL,
            )
            .await
        }
        None => false,
    }
}

/// `IpBlock.blocked?`: the `no_access` ranges, kept for a few seconds per
/// instance as Mastodon keeps them in its cache, so the check costs nothing
/// on most requests.
async fn no_access(state: &AppState, ip: IpAddr) -> bool {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    type Cache = Mutex<HashMap<String, (Instant, std::sync::Arc<Vec<Cidr>>)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    const FRESH: Duration = Duration::from_secs(10);

    let cache = CACHE.get_or_init(Default::default);
    let key = state.instance.domain.clone();
    let cached = cache.lock().ok().and_then(|c| {
        c.get(&key)
            .filter(|(at, _)| at.elapsed() < FRESH)
            .map(|(_, v)| v.clone())
    });
    let ranges = match cached {
        Some(ranges) => ranges,
        None => {
            let rows = sqlx::query!(
                r#"SELECT host(ip) AS "addr!", masklen(ip) AS "prefix!" FROM ip_blocks
                   WHERE severity = $1 AND (expires_at IS NULL OR expires_at > now())"#,
                crate::db::models::ip_severity::NO_ACCESS,
            )
            .fetch_all(&state.db)
            .await
            .unwrap_or_default();
            let ranges = std::sync::Arc::new(
                rows.into_iter()
                    .filter_map(|r| Cidr::parse(&format!("{}/{}", r.addr, r.prefix)))
                    .collect::<Vec<_>>(),
            );
            if let Ok(mut c) = cache.lock() {
                c.insert(key, (Instant::now(), ranges.clone()));
            }
            ranges
        }
    };
    ranges.iter().any(|c| c.contains(ip))
}

/// Rack::Attack's `deny from blocklist`: an address under a `no_access` block
/// gets a plain 403 for everything.
pub async fn deny_blocked(req: Request, next: Next) -> Response {
    let ip = req.extensions().get::<ClientIp>().and_then(|c| c.0);
    let state = req.extensions().get::<AppState>().cloned();
    if let (Some(ip), Some(state)) = (ip, state) {
        if no_access(&state, ip).await {
            return (StatusCode::FORBIDDEN, "Forbidden\n").into_response();
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn cidr_contains() {
        let net = Cidr::parse("192.0.2.0/24").unwrap();
        assert!(net.contains(ip("192.0.2.9")));
        assert!(!net.contains(ip("192.0.3.9")));
        assert!(net.contains(ip("::ffff:192.0.2.9")));
        let host = Cidr::parse("2001:db8::1").unwrap();
        assert!(host.contains(ip("2001:db8::1")));
        assert!(!host.contains(ip("2001:db8::2")));
    }

    #[test]
    fn remote_ip_skips_trusted_proxies() {
        // Behind a local proxy, the client is the nearest untrusted address.
        assert_eq!(
            calculate(Some(ip("127.0.0.1")), Some("203.0.113.5, 10.0.0.2")),
            Some(ip("203.0.113.5"))
        );
        // A forged header beyond an untrusted peer is ignored.
        assert_eq!(
            calculate(Some(ip("198.51.100.7")), Some("203.0.113.5")),
            Some(ip("198.51.100.7"))
        );
        // All trusted: the farthest.
        assert_eq!(
            calculate(Some(ip("127.0.0.1")), Some("10.0.0.3")),
            Some(ip("10.0.0.3"))
        );
        assert_eq!(calculate(None, None), None);
    }
}
