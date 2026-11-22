//! Posts that open with a mention.
//!
//! A post that replies to nothing but starts with `@someone` is addressed to
//! that account, as X reads it: the home and list feeds show it only where a
//! reply to `@someone` would go, and the public timelines leave it out. Mastodon
//! decides this by `in_reply_to_id` alone, and shows such a post to everyone.
//!
//! Nothing is recorded: whom a post opens by mentioning is read from its text
//! and its active mentions when a feed takes it ([`leading_mentions`]), and the
//! public timelines ask `eunha.opens_with_mention` for the same opening. Both
//! only look at the start of the text, so a long post costs no more than a
//! short one. A post that opens by mentioning its own author is no one's.
//!
//! Mastodon: none

use std::collections::HashMap;
use std::sync::LazyLock;

use regex::Regex;
use sqlx::PgPool;

use crate::formatter::extractor::{extract_mentions, Kind};

/// How much of a local post's text a handle can take up: a username of 30
/// characters, a domain of 253, the `@`s and some whitespace.
const HANDLE_PREFIX: usize = 320;

/// The handle a local post's source text opens with, after any whitespace.
pub fn opening_handle(text: &str) -> Option<(String, Option<String>)> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with(['@', '＠']) {
        return None;
    }
    let mut end = trimmed.len().min(HANDLE_PREFIX);
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let first = extract_mentions(&trimmed[..end]).into_iter().next()?;
    if first.start != 0 {
        return None;
    }
    let Kind::Mention(screen_name) = first.kind else {
        return None;
    };
    Some(match screen_name.split_once('@') {
        Some((username, domain)) => (username.to_owned(), Some(domain.to_owned())),
        None => (screen_name, None),
    })
}

/// The `href` of the link a remote post's HTML opens with, inside at most a
/// paragraph and a span, as `eunha.opens_with_mention` reads it.
pub fn opening_link(html: &str) -> Option<&str> {
    static OPENING_LINK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"^\s*(?:<p[^>]*>\s*)?(?:<span[^>]*>\s*)?<a\s[^>]*href="([^"]+)""#)
            .expect("valid pattern")
    });
    OPENING_LINK
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}

/// Whom a status in a feed is addressed to: the account it replies to, or,
/// when it replies to nothing, the one it opens by mentioning.
pub fn addressed_to(
    reply: bool,
    in_reply_to_account_id: Option<i64>,
    leading_mention: Option<i64>,
) -> Option<i64> {
    if reply {
        in_reply_to_account_id
    } else {
        leading_mention
    }
}

/// A status as [`leading_mentions`] reads it.
pub struct Opening<'a> {
    pub id: i64,
    pub account_id: i64,
    pub reply: bool,
    /// `Status#local?`.
    pub local: bool,
    pub text: &'a str,
}

/// An account a status mentions, as matched against its opening.
struct Mentioned {
    id: i64,
    username: String,
    domain: Option<String>,
    url: Option<String>,
    uri: String,
}

/// Whom each of `statuses` that replies to nothing opens by mentioning, among
/// its active mentions, other than its author; by status id.
pub async fn leading_mentions(db: &PgPool, statuses: &[Opening<'_>]) -> HashMap<i64, i64> {
    enum Open<'a> {
        Handle(String, Option<String>),
        Link(&'a str),
    }
    let opening: Vec<(&Opening, Open)> = statuses
        .iter()
        .filter(|s| !s.reply)
        .filter_map(|s| {
            let open = if s.local {
                opening_handle(s.text).map(|(u, d)| Open::Handle(u, d))
            } else {
                opening_link(s.text).map(Open::Link)
            };
            open.map(|o| (s, o))
        })
        .collect();
    if opening.is_empty() {
        return HashMap::new();
    }
    let ids: Vec<i64> = opening.iter().map(|(s, _)| s.id).collect();
    let rows = match sqlx::query!(
        r#"SELECT m.status_id, a.id, a.username, a.domain, a.url, a.uri
           FROM mentions m JOIN accounts a ON a.id = m.account_id
           WHERE m.status_id = ANY($1) AND NOT m.silent"#,
        &ids,
    )
    .fetch_all(db)
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "could not read whom statuses open by mentioning");
            return HashMap::new();
        }
    };
    let mut mentioned: HashMap<i64, Vec<Mentioned>> = HashMap::new();
    for r in rows {
        mentioned.entry(r.status_id).or_default().push(Mentioned {
            id: r.id,
            username: r.username,
            domain: r.domain,
            url: r.url,
            uri: r.uri.unwrap_or_default(),
        });
    }

    let mut found = HashMap::new();
    for (status, open) in opening {
        let Some(accounts) = mentioned.get(&status.id) else {
            continue;
        };
        let account = match open {
            Open::Handle(username, domain) => {
                let named = |a: &&Mentioned| a.username.eq_ignore_ascii_case(&username);
                // `@bob@this.instance` names the local `bob`, whose domain
                // is empty; only the status's own mentions are looked at.
                accounts
                    .iter()
                    .filter(named)
                    .find(|a| match (&domain, &a.domain) {
                        (None, None) => true,
                        (Some(d), Some(ad)) => d.eq_ignore_ascii_case(ad),
                        _ => false,
                    })
                    .or_else(|| {
                        domain.as_ref().and_then(|_| {
                            accounts.iter().filter(named).find(|a| a.domain.is_none())
                        })
                    })
            }
            Open::Link(href) => accounts.iter().find(|a| match a.domain {
                Some(_) => a.url.as_deref() == Some(href) || a.uri == href,
                // A local account's links are this instance's:
                // `/@username`, `/users/username` or `/ap/users/id`.
                None => {
                    href.ends_with(&format!("/@{}", a.username))
                        || href.ends_with(&format!("/users/{}", a.username))
                        || href.ends_with(&format!("/ap/users/{}", a.id))
                }
            }),
        };
        if let Some(account) = account.filter(|a| a.id != status.account_id) {
            found.insert(status.id, account.id);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_post_opening_with_a_mention_has_an_opening_handle() {
        assert_eq!(
            opening_handle("@hellel@xxx.example hello"),
            Some(("hellel".into(), Some("xxx.example".into())))
        );
        assert_eq!(opening_handle("  @bob hi"), Some(("bob".into(), None)));
        assert_eq!(opening_handle("@bob"), Some(("bob".into(), None)));
    }

    #[test]
    fn a_mention_later_in_the_post_is_no_opening_handle() {
        assert_eq!(opening_handle("hi @bob"), None);
        assert_eq!(opening_handle(".@bob hi"), None);
        assert_eq!(opening_handle("no mentions"), None);
    }

    #[test]
    fn only_the_start_of_a_long_post_is_read() {
        let long = format!("@bob {}", "é".repeat(400_000));
        assert_eq!(opening_handle(&long), Some(("bob".into(), None)));
        let long = format!("@{}", "é".repeat(400_000));
        assert_eq!(opening_handle(&long), None);
    }

    #[test]
    fn a_remote_post_opening_with_a_link_has_an_opening_link() {
        let html = r#"<p><span class="h-card" translate="no"><a href="https://x.example/@hellel" class="u-url mention">@<span>hellel</span></a></span> hello</p>"#;
        assert_eq!(opening_link(html), Some("https://x.example/@hellel"));
        let html = r#"<p><a class="u-url mention" href="https://x.example/users/a">@a</a> hi</p>"#;
        assert_eq!(opening_link(html), Some("https://x.example/users/a"));
    }

    #[test]
    fn text_before_the_link_means_no_opening_link() {
        let html =
            r#"<p>hi <a href="https://x.example/@hellel" class="u-url mention">@hellel</a></p>"#;
        assert_eq!(opening_link(html), None);
        assert_eq!(opening_link("<p>plain</p>"), None);
    }

    #[test]
    fn a_reply_is_addressed_to_whom_it_replies_to() {
        assert_eq!(addressed_to(true, Some(1), None), Some(1));
        assert_eq!(addressed_to(true, Some(1), Some(2)), Some(1));
        assert_eq!(addressed_to(false, None, Some(2)), Some(2));
        assert_eq!(addressed_to(false, None, None), None);
    }
}
