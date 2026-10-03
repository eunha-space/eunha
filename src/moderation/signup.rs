//! The moderation checks a sign-up passes: `UserEmailValidator`,
//! `EmailMxValidator`, `UnreservedUsernameValidator`, and the IP, email
//! domain and username blocks that put a sign-up in the approval queue
//! (`User#requires_approval?`).

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::state::AppState;

static SKIP_MX_CHECK: AtomicBool = AtomicBool::new(false);

/// `User.skip_mx_check?`, which Mastodon answers yes to in development and
/// test: no DNS lookups for sign-up emails.
pub fn skip_mx_check() {
    SKIP_MX_CHECK.store(true, Ordering::Relaxed);
}

/// Whether [`skip_mx_check`] was called.
pub fn mx_check_skipped() -> bool {
    SKIP_MX_CHECK.load(Ordering::Relaxed)
}

/// `DomainMaterializable.domain_variants`: the domain and each parent.
pub fn domain_variants(domain: &str) -> Vec<String> {
    let labels: Vec<&str> = domain.split('.').collect();
    (0..labels.len()).map(|i| labels[i..].join(".")).collect()
}

/// The domain part of an email, normalized (`TagManager#normalize_domain`).
pub fn email_domain(email: &str) -> Option<String> {
    let (_, domain) = email.split_once('@')?;
    let domain = domain.trim().to_lowercase();
    if domain.is_empty() || domain.contains("..") {
        return None;
    }
    url::Url::parse(&format!("https://{domain}/"))
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
}

/// `EmailDomainBlock::Matcher#match?`: any of `domains`, or a parent of one,
/// is blocked with `allow_with_approval` as given; a domain that cannot be
/// read counts as blocked. Each matching block counts the attempt from
/// `attempt_ip` in its history.
pub async fn email_domain_blocked(
    state: &AppState,
    domains: &[String],
    allow_with_approval: bool,
    attempt_ip: Option<IpAddr>,
) -> bool {
    let mut variants = vec![];
    for domain in domains {
        let Some(normalized) = (if domain.contains('@') {
            email_domain(domain)
        } else {
            email_domain(&format!("x@{domain}"))
        }) else {
            return !allow_with_approval;
        };
        variants.extend(domain_variants(&normalized));
    }
    variants.sort();
    variants.dedup();
    let blocks: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM email_domain_blocks WHERE domain = ANY($1) AND allow_with_approval = $2",
        &variants,
        allow_with_approval,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    if let Some(ip) = attempt_ip {
        for id in &blocks {
            super::history::add(state, "email_domain_blocks", *id, &ip.to_string()).await;
        }
    }
    !blocks.is_empty()
}

/// `CanonicalEmail.canonicalize_email`: lowercased, the local part without
/// dots or a `+tag`.
pub fn canonicalize_email(email: &str) -> String {
    let email = email.to_lowercase();
    match email.split_once('@') {
        Some((local, domain)) => {
            let local: String = local.chars().filter(|c| *c != '.').collect();
            let local = local.split('+').next().unwrap_or("").to_owned();
            format!("{local}@{domain}")
        }
        None => {
            let local: String = email.chars().filter(|c| *c != '.').collect();
            local.split('+').next().unwrap_or("").to_owned()
        }
    }
}

/// `CanonicalEmailBlock.block?`.
pub async fn canonical_email_blocked(state: &AppState, email: &str) -> bool {
    use sha2::{Digest, Sha256};
    let hash = hex::encode(Sha256::digest(canonicalize_email(email).as_bytes()));
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM canonical_email_blocks WHERE canonical_email_hash = $1) AS "e!""#,
        hash,
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(false)
}

/// `UsernameBlock`'s `normalized_username`: lowercased, with digits read as
/// the letters they pass for.
pub fn normalize_username(username: &str) -> String {
    username
        .to_lowercase()
        .chars()
        .map(|c| match c {
            '1' => 'i',
            '2' => 'z',
            '3' => 'e',
            '4' => 'a',
            '5' => 's',
            '7' => 't',
            '8' => 'b',
            '9' => 'g',
            '0' => 'o',
            c => c,
        })
        .collect()
}

/// `UsernameBlock.matches?(username, allow_with_approval:)`: an exact block
/// equal to it, or a partial one it contains, once both are normalized. The
/// partial match is Arel's `matches`, which is `ILIKE` on PostgreSQL, with the
/// block's own `%` and `_` left as wildcards as upstream leaves them.
pub async fn username_blocked(state: &AppState, username: &str, allow_with_approval: bool) -> bool {
    let normalized = normalize_username(username);
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM username_blocks
             WHERE allow_with_approval = $2
               AND ((exact AND normalized_username = $1)
                    OR (NOT exact AND $1 ILIKE '%' || normalized_username || '%'))
           ) AS "e!""#,
        normalized,
        allow_with_approval,
    )
    .fetch_one(&state.db)
    .await
    .unwrap_or(false)
}

/// What `EmailMxValidator` found for a domain: the addresses it and its mail
/// exchangers resolve to, and the exchangers' names.
pub struct Mx {
    pub ips: Vec<IpAddr>,
    pub records: Vec<String>,
}

/// `EmailMxValidator#resolve_mx`.
pub async fn resolve_mx(domain: &str) -> Mx {
    use hickory_resolver::TokioResolver;
    let Ok(builder) = TokioResolver::builder_tokio() else {
        return Mx {
            ips: vec![],
            records: vec![],
        };
    };
    let resolver = builder.build();
    let mut records: Vec<String> = match resolver.mx_lookup(domain).await {
        Ok(mx) => mx
            .iter()
            .map(|r| r.exchange().to_utf8().trim_end_matches('.').to_owned())
            .collect(),
        Err(_) => vec![],
    };
    // `next if records == ['']`: a null MX says the domain takes no mail.
    if records.len() == 1 && records[0].is_empty() {
        return Mx {
            ips: vec![],
            records,
        };
    }
    let mut ips = vec![];
    let mut hosts = vec![domain.to_owned()];
    hosts.append(&mut records.clone());
    hosts.dedup();
    for host in hosts {
        if let Ok(found) = resolver.lookup_ip(host.as_str()).await {
            ips.extend(found.iter());
        }
    }
    records.dedup();
    Mx { ips, records }
}

/// Why a sign-up is refused, as the validation error Mastodon would give.
#[derive(Debug)]
pub enum Refusal {
    UsernameReserved,
    EmailBlocked,
    EmailTaken,
    EmailUnreachable,
    EmailInvalid,
}

impl Refusal {
    pub fn message(&self) -> &'static str {
        match self {
            Refusal::UsernameReserved => "Validation failed: Username is reserved",
            Refusal::EmailBlocked => {
                "Validation failed: Email is using a disallowed e-mail provider"
            }
            Refusal::EmailTaken => "Validation failed: Email has already been taken",
            Refusal::EmailUnreachable => "Validation failed: Email does not seem to exist",
            Refusal::EmailInvalid => "Validation failed: Email is invalid",
        }
    }
}

/// The outcome of a sign-up's checks.
pub struct Checked {
    /// `User#requires_approval?`: an IP, email domain or username block that
    /// lets the sign-up through only into the approval queue.
    pub requires_approval: bool,
}

/// `EmailMxValidator`: the address's domain resolves to a mail server, and
/// neither it nor its mail servers are blocked. Returns the MX hosts.
async fn email_mx(
    state: &AppState,
    domain: &str,
    sign_up_ip: Option<IpAddr>,
) -> Result<Vec<String>, Refusal> {
    if SKIP_MX_CHECK.load(Ordering::Relaxed) {
        return Ok(vec![]);
    }
    let mx = resolve_mx(domain).await;
    if mx.ips.is_empty() {
        return Err(Refusal::EmailUnreachable);
    }
    let mut domains = vec![domain.to_owned()];
    domains.extend(mx.records.iter().cloned());
    if email_domain_blocked(state, &domains, false, sign_up_ip).await {
        return Err(Refusal::EmailBlocked);
    }
    Ok(mx.records)
}

/// `UserEmailValidator`: the address's domain is not blocked, nor the address
/// itself (`CanonicalEmailBlock`).
async fn user_email(
    state: &AppState,
    email: &str,
    domain: &str,
    sign_up_ip: Option<IpAddr>,
) -> Result<(), Refusal> {
    if email_domain_blocked(state, &[domain.to_owned()], false, sign_up_ip).await {
        return Err(Refusal::EmailBlocked);
    }
    if canonical_email_blocked(state, email).await {
        return Err(Refusal::EmailTaken);
    }
    Ok(())
}

/// The email validations a user made outside the sign-up flow still meets —
/// `tootctl accounts create`'s, which bypasses the registration checks:
/// `EmailMxValidator` always, and `UserEmailValidator` while the user is
/// unconfirmed.
pub async fn check_email(state: &AppState, email: &str, confirmed: bool) -> Result<(), Refusal> {
    let domain = email_domain(email).ok_or(Refusal::EmailInvalid)?;
    email_mx(state, &domain, None).await?;
    if !confirmed {
        user_email(state, email, &domain, None).await?;
    }
    Ok(())
}

/// The sign-up validations that are about moderation. `valid_invitation`
/// skips the email provider checks, as `UserEmailValidator` does.
pub async fn check(
    state: &AppState,
    username: &str,
    email: &str,
    sign_up_ip: Option<IpAddr>,
    valid_invitation: bool,
) -> Result<Checked, Refusal> {
    // `UnreservedUsernameValidator`
    if username_blocked(state, username, false).await {
        return Err(Refusal::UsernameReserved);
    }
    let domain = email_domain(email).ok_or(Refusal::EmailInvalid)?;
    let mx_records = email_mx(state, &domain, sign_up_ip).await?;
    if !valid_invitation {
        user_email(state, email, &domain, sign_up_ip).await?;
    }

    // `requires_approval?`
    let mut approval_domains = mx_records;
    approval_domains.push(domain);
    let requires_approval = crate::remote_ip::sign_up_requires_approval(state, sign_up_ip).await
        || email_domain_blocked(state, &approval_domains, true, sign_up_ip).await
        || username_blocked(state, username, true).await;
    Ok(Checked { requires_approval })
}

/// `User#requires_approval?` alone, asked again when a confirmed sign-up
/// becomes an account (the attempt was counted when it was made).
pub async fn requires_approval(
    state: &AppState,
    username: &str,
    email: &str,
    sign_up_ip: Option<IpAddr>,
) -> bool {
    let Some(domain) = email_domain(email) else {
        return true;
    };
    let mut domains = if SKIP_MX_CHECK.load(Ordering::Relaxed) {
        vec![]
    } else {
        resolve_mx(&domain).await.records
    };
    domains.push(domain);
    crate::remote_ip::sign_up_requires_approval(state, sign_up_ip).await
        || email_domain_blocked(state, &domains, true, None).await
        || username_blocked(state, username, true).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_email() {
        assert_eq!(
            canonicalize_email("Jo.Hn+spam@Example.com"),
            "john@example.com"
        );
    }

    #[test]
    fn normalized_username() {
        assert_eq!(normalize_username("Adm1n"), "admin");
    }

    #[test]
    fn variants() {
        assert_eq!(
            domain_variants("mail.example.com"),
            vec!["mail.example.com", "example.com", "com"]
        );
    }
}
