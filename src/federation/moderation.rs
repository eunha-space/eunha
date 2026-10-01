//! Instance-level domain moderation (the admin `domain_blocks` list) applied to
//! the federation path: inbound activity acceptance and outbound delivery.
//!
//! Display-time filtering (timelines, search, profiles) lives in the Mastodon
//! API layer; this module is what actually stops federation traffic. A block on
//! `example.com` also covers its subdomains (`a.example.com`), matching
//! Mastodon.

use crate::db::models::domain_severity;
use crate::state::AppState;

/// The host of an ActivityPub id/URI, lowercased.
pub fn domain_of(uri: &str) -> Option<String> {
    url::Url::parse(uri)
        .ok()?
        .host_str()
        .map(|h| h.to_lowercase())
}

/// The effect of the admin domain block covering `domain`, if any.
#[derive(Debug, Clone, Copy, Default)]
pub struct DomainBlock {
    pub severity: i32,
    pub reject_media: bool,
}

impl DomainBlock {
    /// True when the block defederates the domain (drop all traffic).
    pub fn is_suspend(&self) -> bool {
        self.severity == domain_severity::SUSPEND
    }
}

/// `DomainBlock.rule_for`: the most specific admin domain block covering
/// `domain` (the domain itself or any parent domain), or `None`.
pub async fn lookup(state: &AppState, domain: &str) -> Option<DomainBlock> {
    let domain = domain.to_lowercase();
    let row = sqlx::query!(
        r#"SELECT severity, reject_media
           FROM domain_blocks
           WHERE domain <> '' AND ($1 = domain OR $1 LIKE '%.' || domain)
           ORDER BY char_length(domain) DESC
           LIMIT 1"#,
        domain,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()?;
    Some(DomainBlock {
        severity: row.severity.unwrap_or(domain_severity::SILENCE),
        reject_media: row.reject_media,
    })
}

/// True when activities attributed to `actor_uri` should be dropped on arrival
/// (the actor's domain is defederated at suspend severity).
pub async fn actor_is_suspended(state: &AppState, actor_uri: &str) -> bool {
    let Some(domain) = domain_of(actor_uri) else {
        return false;
    };
    matches!(lookup(state, &domain).await, Some(b) if b.is_suspend())
}

/// True when remote media from `actor_uri`'s domain should not be stored
/// (`reject_media`, or a full suspend which implies it).
pub async fn actor_media_rejected(state: &AppState, actor_uri: &str) -> bool {
    let Some(domain) = domain_of(actor_uri) else {
        return false;
    };
    matches!(lookup(state, &domain).await, Some(b) if b.reject_media || b.is_suspend())
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
    let host = host.to_lowercase();
    blocked.iter().any(|b| {
        let b = b.to_lowercase();
        host == b || host.ends_with(&format!(".{b}"))
    })
}

/// True if the inbox URL's host is covered by any of the `blocked` domains.
pub fn inbox_suspended(inbox_url: &str, blocked: &[String]) -> bool {
    url::Url::parse(inbox_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .map(|h| host_matches(&h, blocked))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked() -> Vec<String> {
        vec!["example.com".into(), "Evil.NET".into()]
    }

    #[test]
    fn domain_of_extracts_lowercased_host() {
        assert_eq!(
            domain_of("https://Mastodon.Social/users/foo"),
            Some("mastodon.social".into())
        );
        assert_eq!(domain_of("not a url"), None);
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

    #[test]
    fn inbox_suspended_matches_on_host() {
        let b = blocked();
        assert!(inbox_suspended("https://a.example.com/inbox", &b));
        assert!(!inbox_suspended("https://safe.test/inbox", &b));
        assert!(!inbox_suspended("garbage", &b));
    }
}
