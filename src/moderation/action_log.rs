//! `Admin::ActionLog`, written by `AccountableConcern#log_action`.

use sqlx::PgExecutor;

use crate::error::AppResult;

/// What a log entry is about: the polymorphic target and the three things
/// `Admin::ActionLog` copies from it before saving (`to_log_human_identifier`,
/// `to_log_route_param`, `to_log_permalink`), so the entry still reads once the
/// target is gone.
#[derive(Debug, Clone)]
pub struct Target {
    /// The Rails class name, as `target_type` stores it.
    pub kind: &'static str,
    pub id: i64,
    pub human_identifier: Option<String>,
    pub route_param: Option<String>,
    pub permalink: Option<String>,
}

impl Target {
    fn new(kind: &'static str, id: i64, human_identifier: impl Into<String>) -> Self {
        Self {
            kind,
            id,
            human_identifier: Some(human_identifier.into()),
            route_param: None,
            permalink: None,
        }
    }

    /// `Account#to_log_human_identifier` is its `acct`.
    pub fn account(id: i64, acct: impl Into<String>) -> Self {
        Self::new("Account", id, acct)
    }

    /// A `User` reads as its account's `acct`, and routes by its account id.
    pub fn user(user_id: i64, account_id: i64, acct: impl Into<String>) -> Self {
        Self {
            route_param: Some(account_id.to_string()),
            ..Self::new("User", user_id, acct)
        }
    }

    pub fn report(id: i64) -> Self {
        Self::new("Report", id, id.to_string())
    }

    /// `AccountWarning` reads as its target account's `acct`.
    pub fn account_warning(id: i64, target_acct: impl Into<String>) -> Self {
        Self::new("AccountWarning", id, target_acct)
    }

    /// A `Status` reads as its author's `acct` and links to its URI.
    pub fn status(id: i64, author_acct: impl Into<String>, uri: impl Into<String>) -> Self {
        Self {
            permalink: Some(uri.into()),
            ..Self::new("Status", id, author_acct)
        }
    }

    /// A `Collection` reads as its owner's `acct` and links to its URI.
    pub fn collection(id: i64, owner_acct: impl Into<String>, uri: Option<String>) -> Self {
        Self {
            permalink: uri,
            ..Self::new("Collection", id, owner_acct)
        }
    }

    pub fn domain_block(id: i64, domain: impl Into<String>) -> Self {
        Self::new("DomainBlock", id, domain)
    }

    pub fn domain_allow(id: i64, domain: impl Into<String>) -> Self {
        Self::new("DomainAllow", id, domain)
    }

    pub fn email_domain_block(id: i64, domain: impl Into<String>) -> Self {
        Self::new("EmailDomainBlock", id, domain)
    }

    /// `IpBlock#to_log_human_identifier` is `to_cidr`.
    pub fn ip_block(id: i64, cidr: impl Into<String>) -> Self {
        Self::new("IpBlock", id, cidr)
    }

    pub fn canonical_email_block(id: i64, hash: impl Into<String>) -> Self {
        Self::new("CanonicalEmailBlock", id, hash)
    }

    pub fn custom_emoji(id: i64, shortcode: impl Into<String>) -> Self {
        Self::new("CustomEmoji", id, shortcode)
    }

    /// `Tag#to_log_human_identifier` is `formatted_name`, the `#`-prefixed
    /// display name.
    pub fn tag(id: i64, formatted_name: impl Into<String>) -> Self {
        Self::new("Tag", id, formatted_name)
    }

    pub fn user_role(id: i64, name: impl Into<String>) -> Self {
        Self::new("UserRole", id, name)
    }

    /// `UsernameBlock#to_log_human_identifier` is its `username`.
    pub fn username_block(id: i64, username: impl Into<String>) -> Self {
        Self::new("UsernameBlock", id, username)
    }

    /// `Appeal` reads as its account's `acct`, and routes by its strike.
    pub fn appeal(id: i64, acct: impl Into<String>, strike_id: i64) -> Self {
        Self {
            route_param: Some(strike_id.to_string()),
            ..Self::new("Appeal", id, acct)
        }
    }

    /// `Announcement#to_log_human_identifier` is its `text`.
    pub fn announcement(id: i64, text: impl Into<String>) -> Self {
        Self::new("Announcement", id, text)
    }

    /// `Relay#to_log_human_identifier` is its `inbox_url`.
    pub fn relay(id: i64, inbox_url: impl Into<String>) -> Self {
        Self::new("Relay", id, inbox_url)
    }

    /// `UnavailableDomain#to_log_human_identifier` is its `domain`.
    pub fn unavailable_domain(id: i64, domain: impl Into<String>) -> Self {
        Self::new("UnavailableDomain", id, domain)
    }

    /// An `Instance` reads as its domain, which is also its primary key; the
    /// integer `target_id` takes that as Rails casts a domain to an integer,
    /// zero.
    pub fn instance(domain: impl Into<String>) -> Self {
        Self::new("Instance", 0, domain)
    }

    pub fn rule(id: i64) -> Self {
        Self {
            kind: "Rule",
            id,
            human_identifier: None,
            route_param: None,
            permalink: None,
        }
    }

    /// `PreviewCard`, `PreviewCardProvider` and `Instance` have no
    /// `to_log_human_identifier`.
    pub fn bare(kind: &'static str, id: i64) -> Self {
        Self {
            kind,
            id,
            human_identifier: None,
            route_param: None,
            permalink: None,
        }
    }
}

/// `log_action(action, target)` by `account_id`.
pub async fn log<'e>(
    db: impl PgExecutor<'e>,
    account_id: i64,
    action: &str,
    target: &Target,
) -> AppResult<()> {
    log_with_changes(db, account_id, action, target, None).await
}

/// `log_action` for a target whose `LOG_ATTRIBUTES` changes are recorded too
/// (`recorded_changes`, used for tags' usable/trendable/listable).
pub async fn log_with_changes<'e>(
    db: impl PgExecutor<'e>,
    account_id: i64,
    action: &str,
    target: &Target,
    recorded_changes: Option<serde_json::Value>,
) -> AppResult<()> {
    sqlx::query!(
        r#"INSERT INTO admin_action_logs
             (account_id, action, target_type, target_id, human_identifier,
              route_param, permalink, recorded_changes, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now(), now())"#,
        account_id,
        action,
        target.kind,
        target.id,
        target.human_identifier,
        target.route_param,
        target.permalink,
        recorded_changes,
    )
    .execute(db)
    .await?;
    Ok(())
}
