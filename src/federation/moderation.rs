//! Instance-level domain moderation (the admin `domain_blocks` list) applied to
//! the federation path: inbound activity acceptance and outbound delivery.
//!
//! Display-time filtering (timelines, search, profiles) lives in the Mastodon
//! API layer; this module is what actually stops federation traffic. A block on
//! `example.com` also covers its subdomains (`a.example.com`), matching
//! Mastodon. A domain keeps its port, as Mastodon's do: a block on
//! `example.com` does not cover `example.com:8080`, an account's domain on a
//! server at a port, and one on `example.com:8080` covers only that.

use crate::db::models::domain_severity;
use crate::state::AppState;

/// The effect of the admin domain block covering `domain`, if any.
#[derive(Debug, Clone, Copy, Default)]
pub struct DomainBlock {
    pub severity: i32,
    pub reject_media: bool,
    pub reject_reports: bool,
}

impl DomainBlock {
    /// True when the block defederates the domain (drop all traffic).
    pub fn is_suspend(&self) -> bool {
        self.severity == domain_severity::SUSPEND
    }
}

/// The domain `DomainBlock.rule_for` and `DomainAllow.rule_for` look up:
/// stripped, every `/` deleted, then Addressable's `normalized_host` — lower
/// case, in its ASCII form — with any port kept, since `host=` takes the
/// port in with the host. `None` for a blank domain or one Addressable
/// refuses, which rule nothing.
fn rule_domain(domain: &str) -> Option<String> {
    let domain = domain.trim().replace('/', "");
    if domain.is_empty() {
        return None;
    }
    ojak::origin::normalize_host(&domain, ojak::origin::Port::Keep)
}

/// `DomainBlock.rule_for`: the most specific admin domain block on `domain`
/// or one of its `domain_variants` (each suffix after a dot), or `None`.
/// The port stays on the last label, so `example.com:8080` is covered by a
/// block on itself or on `com:8080`, not by one on `example.com`.
pub async fn lookup(state: &AppState, domain: &str) -> Option<DomainBlock> {
    let domain = rule_domain(domain)?;
    let variants = crate::moderation::signup::domain_variants(&domain);
    let row = sqlx::query!(
        r#"SELECT severity, reject_media, reject_reports
           FROM domain_blocks
           WHERE domain = ANY($1)
           ORDER BY char_length(domain) DESC
           LIMIT 1"#,
        &variants,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()?;
    Some(DomainBlock {
        severity: row.severity.unwrap_or(domain_severity::SILENCE),
        reject_media: row.reject_media,
        reject_reports: row.reject_reports,
    })
}

/// `DomainControlHelper#domain_not_allowed?`: whether this instance refuses
/// to federate with `uri_or_domain` (a URI, or a bare domain). In limited
/// federation mode only a domain on the allow list federates, matched exactly
/// as `DomainAllow.allowed?` matches it; otherwise only a domain blocked at
/// suspend severity, itself or a parent, is refused. Blank is allowed.
///
/// A URI is read for its host alone (`Addressable::URI.parse(uri).host`),
/// without its port; a bare domain is passed on as it is, port and all, to
/// be normalised by `rule_for`.
pub async fn domain_not_allowed(state: &AppState, uri_or_domain: &str) -> bool {
    if uri_or_domain.trim().is_empty() {
        return false;
    }
    let domain = if uri_or_domain.contains("://") {
        ojak::origin::host_of(uri_or_domain)
    } else {
        Some(uri_or_domain.to_owned())
    };
    // No host is on no allow list, and under no block.
    let Some(domain) = domain else {
        return state.instance.limited_federation_mode;
    };
    if state.instance.limited_federation_mode {
        return !domain_allowed(state, &domain).await;
    }
    matches!(lookup(state, &domain).await, Some(b) if b.is_suspend())
}

/// `DomainAllow.allowed?`: whether `domain` itself, not a parent, is on the
/// allow list, normalised as `DomainAllow.rule_for` normalises it. A failed
/// lookup allows nothing.
async fn domain_allowed(state: &AppState, domain: &str) -> bool {
    let Some(domain) = rule_domain(domain) else {
        return false;
    };
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM domain_allows WHERE domain = $1) AS "e!""#,
        domain,
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(false)
}

/// True when activities attributed to `actor_uri` should be dropped on arrival
/// (the actor's domain is defederated at suspend severity).
pub async fn actor_is_suspended(state: &AppState, actor_uri: &str) -> bool {
    let Some(domain) = ojak::origin::host_of(actor_uri) else {
        return false;
    };
    matches!(lookup(state, &domain).await, Some(b) if b.is_suspend())
}

/// `MediaAttachment#skip_download`'s `DomainBlock.reject_media?(account.domain)`:
/// whether the remote account `account_id`'s media is stored without its file.
pub async fn account_media_rejected(state: &AppState, account_id: i64) -> bool {
    let domain = sqlx::query_scalar!("SELECT domain FROM accounts WHERE id = $1", account_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .flatten();
    match domain {
        Some(domain) => matches!(lookup(state, &domain).await, Some(b) if b.reject_media),
        None => false,
    }
}

/// All domains blocked at suspend severity, for bulk outbound delivery
/// filtering. Matched against inbox hosts with [`host_matches`].
pub async fn suspended_domains(state: &AppState) -> Vec<String> {
    sqlx::query_scalar!(
        "SELECT domain FROM domain_blocks WHERE domain <> '' AND severity = $1",
        domain_severity::SUSPEND,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
}

/// True if `host` equals or is a subdomain of any entry in `blocked`.
pub fn host_matches(host: &str, blocked: &[String]) -> bool {
    blocked
        .iter()
        .any(|parent| ojak::origin::host_within(host, parent))
}

/// True if the inbox URL's server is covered by any of the `blocked` domains,
/// as `DomainBlock.rule_for` covers an account's domain: the host with the
/// port it is reached at when that is not the scheme's own, so a block on
/// `example.com` covers `https://a.example.com/inbox` but not
/// `https://example.com:8443/inbox`, and one on `example.com:8443` covers
/// only that server. Upstream's delivery does not look at domain blocks; it
/// never has these inboxes to deliver to, because the block suspended their
/// accounts, matched by that same domain.
pub fn inbox_suspended(inbox_url: &str, blocked: &[String]) -> bool {
    inbox_domain(inbox_url).is_some_and(|domain| host_matches(&domain, blocked))
}

/// The domain an account at `inbox_url` would have: its host, lower case,
/// with its port when one other than the scheme's is given.
fn inbox_domain(inbox_url: &str) -> Option<String> {
    let url = url::Url::parse(inbox_url).ok()?;
    let host = url
        .host_str()
        .filter(|h| !h.is_empty())?
        .to_ascii_lowercase();
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked() -> Vec<String> {
        vec!["example.com".into(), "Evil.NET".into()]
    }

    #[test]
    fn host_matches_exact_and_subdomains() {
        let b = blocked();
        assert!(host_matches("example.com", &b));
        assert!(host_matches("a.example.com", &b));
        assert!(host_matches("deep.sub.example.com", &b));
        // case-insensitive on both sides
        assert!(host_matches("EXAMPLE.com", &b));
        assert!(host_matches("mail.evil.net", &b));
    }

    #[test]
    fn host_matches_rejects_non_subdomains() {
        let b = blocked();
        assert!(!host_matches("notexample.com", &b));
        assert!(!host_matches("example.com.attacker.test", &b));
        assert!(!host_matches("example.org", &b));
        assert!(!host_matches("fakeexample.com", &b));
    }

    /// `rule_for` normalises as Addressable's `host=` and `normalized_host`
    /// do, the port kept and every slash gone.
    #[test]
    fn rule_domain_keeps_the_port() {
        assert_eq!(
            rule_domain(" Example.COM:8080/").as_deref(),
            Some("example.com:8080")
        );
        assert_eq!(rule_domain("exa/mple.com").as_deref(), Some("example.com"));
        assert_eq!(
            rule_domain("bücher.example").as_deref(),
            Some("xn--bcher-kva.example")
        );
        assert_eq!(rule_domain(" / "), None);
        assert_eq!(
            crate::moderation::signup::domain_variants("a.example.com:8080"),
            ["a.example.com:8080", "example.com:8080", "com:8080"]
        );
    }

    #[test]
    fn inbox_suspended_matches_on_host() {
        let b = blocked();
        assert!(inbox_suspended("https://a.example.com/inbox", &b));
        assert!(!inbox_suspended("https://safe.test/inbox", &b));
        assert!(!inbox_suspended("garbage", &b));
    }

    /// The port is part of the domain, as it is of an account's and of the
    /// block's: a block on the host alone does not cover a server at a port,
    /// and one at a port covers that server and its subdomains there.
    #[test]
    fn inbox_suspended_keeps_the_port() {
        let b = vec!["example.com".to_owned(), "ported.test:8443".to_owned()];
        assert!(inbox_suspended("https://example.com/inbox", &b));
        // The scheme's own port is no port.
        assert!(inbox_suspended("https://example.com:443/inbox", &b));
        assert!(!inbox_suspended("https://example.com:8443/inbox", &b));
        assert!(inbox_suspended("https://ported.test:8443/inbox", &b));
        assert!(inbox_suspended("https://a.ported.test:8443/inbox", &b));
        assert!(!inbox_suspended("https://ported.test/inbox", &b));
        assert!(!inbox_suspended("https://ported.test:9443/inbox", &b));
    }
}
