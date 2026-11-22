//! Posts that open with a mention.
//!
//! A post that replies to nothing but starts with `@someone` is addressed to
//! that account, as X reads it: the home and list feeds show it only where a
//! reply to `@someone` would go, and the public timelines leave it out. Mastodon
//! decides this by `in_reply_to_id` alone, and shows such a post to everyone.
//!
//! The account is recorded in `eunha.leading_mentions` when a post is written,
//! received or edited, after its mentions are, so that the timeline queries
//! can read it; [`addressed_to`] is the rule the feeds apply. Replies, and
//! posts that open by mentioning their own author, get no row.
//!
//! Mastodon: none

use crate::formatter::extractor::{extract_mentions, Kind};
use crate::state::AppState;

/// The handle a local post's source text opens with, after any whitespace.
pub fn opening_handle(text: &str) -> Option<(String, Option<String>)> {
    let start = text.len() - text.trim_start().len();
    let first = extract_mentions(text).into_iter().next()?;
    if first.start != start {
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

/// The `href` of the link a remote post's HTML opens with, when nothing but
/// whitespace and paragraph or span wrappers comes before it.
pub fn opening_link(html: &str) -> Option<String> {
    let document = scraper::Html::parse_fragment(html);
    for node in document.root_element().descendants() {
        match node.value() {
            scraper::Node::Text(text) if !text.trim().is_empty() => return None,
            scraper::Node::Element(element) => match element.name() {
                "a" => return element.attr("href").map(str::to_owned),
                "html" | "body" | "p" | "span" | "div" => {}
                _ => return None,
            },
            _ => {}
        }
    }
    None
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

/// Records whom `status_id` opens by mentioning, or that it opens with no
/// one, from its text and its active mentions.
pub async fn record(state: &AppState, status_id: i64) -> sqlx::Result<()> {
    let Some(status) = sqlx::query!(
        r#"SELECT s.text, s.account_id, s.reply, a.domain IS NULL AS "local!"
           FROM statuses s JOIN accounts a ON a.id = s.account_id
           WHERE s.id = $1"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Ok(());
    };
    let mentioned = sqlx::query!(
        r#"SELECT a.id, a.username, a.domain, a.url, a.uri, a.id_scheme
           FROM mentions m JOIN accounts a ON a.id = m.account_id
           WHERE m.status_id = $1 AND NOT m.silent"#,
        status_id,
    )
    .fetch_all(&state.db)
    .await?;

    let instance = &state.instance;
    let addressee = if status.reply {
        None
    } else if status.local {
        opening_handle(&status.text).and_then(|(username, domain)| {
            let domain = domain.filter(|d| !instance.is_local_domain(d));
            mentioned.iter().find(|a| {
                a.username.eq_ignore_ascii_case(&username)
                    && match (&domain, &a.domain) {
                        (None, None) => true,
                        (Some(d), Some(ad)) => d.eq_ignore_ascii_case(ad),
                        _ => false,
                    }
            })
        })
    } else {
        opening_link(&status.text).and_then(|href| {
            mentioned.iter().find(|a| {
                let urls: Vec<String> = match a.domain {
                    None => [instance.web_domain(), instance.local_domain.as_str()]
                        .into_iter()
                        .flat_map(|domain| {
                            [
                                format!("https://{domain}/@{}", a.username),
                                crate::federation::tag::account_uri(
                                    domain,
                                    a.id,
                                    a.id_scheme,
                                    &a.username,
                                ),
                            ]
                        })
                        .collect(),
                    Some(_) => a.url.iter().chain(&a.uri).cloned().collect(),
                };
                urls.iter().any(|url| url == &href)
            })
        })
    }
    .map(|a| a.id)
    .filter(|&id| id != status.account_id);

    match addressee {
        Some(account_id) => {
            sqlx::query!(
                "INSERT INTO eunha.leading_mentions (status_id, account_id) VALUES ($1, $2)
                 ON CONFLICT (status_id) DO UPDATE SET account_id = EXCLUDED.account_id",
                status_id,
                account_id,
            )
            .execute(&state.db)
            .await?;
        }
        None => {
            sqlx::query!(
                "DELETE FROM eunha.leading_mentions WHERE status_id = $1",
                status_id
            )
            .execute(&state.db)
            .await?;
        }
    }
    Ok(())
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
    }

    #[test]
    fn a_mention_later_in_the_post_is_no_opening_handle() {
        assert_eq!(opening_handle("hi @bob"), None);
        assert_eq!(opening_handle(".@bob hi"), None);
        assert_eq!(opening_handle("no mentions"), None);
    }

    #[test]
    fn a_remote_post_opening_with_a_link_has_an_opening_link() {
        let html = r#"<p><span class="h-card" translate="no"><a href="https://x.example/@hellel" class="u-url mention">@<span>hellel</span></a></span> hello</p>"#;
        assert_eq!(opening_link(html), Some("https://x.example/@hellel".into()));
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
