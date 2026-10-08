//! rel="me" profile link verification, mirroring Mastodon's `VerifyLinkService`
//! and `VerifyAccountLinksWorker`.
//!
//! When a local account saves profile metadata fields, any field whose value is
//! a plain `https` URL is a candidate for verification. We fetch that URL and
//! look for an `<a rel="me">` / `<link rel="me">` element pointing back at the
//! account's profile URL. If found, the field is stamped with `verified_at`,
//! which the API surfaces so clients can render the green "verified link" badge.
//!
//! Verification runs asynchronously after `update_credentials`, exactly like
//! Mastodon enqueues `VerifyAccountLinksWorker`. Already-verified fields are left
//! untouched; a field only loses its badge when its value changes (handled at
//! save time by preserving `verified_at` only for unchanged values).
//!
//! A remote account's fields are verified too, some minutes after
//! `ProcessAccountService` stores them, against the account's `url`: a value
//! that is nothing but a link to itself is the candidate there. Its server
//! sends the fields without `verified_at`, so each refresh verifies afresh.

use std::time::Duration;

use scraper::{Html, Selector};
use serde_json::Value;

use crate::state::AppState;

/// Mastodon `Account::Field#verifiable?`: the value must be a plain `https` URL
/// with a host, no userinfo, and no IDN host. (Mastodon also requires a
/// normalized path; the `url` crate normalizes on parse, so a round-trippable
/// ASCII URL satisfies that.)
pub fn is_verifiable(value: &str) -> bool {
    let Ok(u) = url::Url::parse(value) else {
        return false;
    };
    if u.scheme() != "https" {
        return false;
    }
    if !u.username().is_empty() || u.password().is_some() {
        return false;
    }
    let Some(host) = u.host_str() else {
        return false;
    };
    // Reject IDN/punycode hosts — Mastodon skips these (normalized_host != host).
    if !host.is_ascii() || host.starts_with("xn--") || host.contains(".xn--") {
        return false;
    }
    true
}

/// The `href` of every `<a>`/`<link>` element that carries `rel="me"`
/// (rel is a space-separated token list, matched case-insensitively), in
/// document order, `None` for one without: Mastodon's
/// `(//a|//link)[@rel][nokogiri:link_rel_include(@rel, "me")]`.
fn rel_me_hrefs(html: &str) -> Vec<Option<String>> {
    let doc = Html::parse_document(html);
    // unwrap: static selector, always valid.
    let sel = Selector::parse("a[rel], link[rel]").unwrap();
    doc.select(&sel)
        .filter_map(|el| {
            let rel = el.value().attr("rel")?;
            let is_me = rel.split_whitespace().any(|t| t.eq_ignore_ascii_case("me"));
            is_me.then(|| el.value().attr("href").map(str::to_string))
        })
        .collect()
}

/// `Request#truncated_body`'s limit: the first megabyte of a page.
const BODY_LIMIT: usize = 1024 * 1024;

/// `VerifyLinkService#perform_request!`: the page at `url`, as far as
/// `truncated_body` reads it, when it answers exactly `200`, whatever its
/// content type. Redirects are followed, as `Request` follows them.
async fn fetch_page(http: &ojak::client::Client, url: &str) -> Option<String> {
    // Through the SSRF-guarded client, as preview cards are fetched.
    let url = url::Url::parse(url).ok()?;
    let mut resp = http
        .request(reqwest::Method::GET, &url)
        .ok()?
        .header("Accept", "text/html")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .ok()?;
    if resp.status().as_u16() != 200 {
        return None;
    }
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        body.extend_from_slice(&chunk);
        if body.len() >= BODY_LIMIT {
            body.truncate(BODY_LIMIT);
            break;
        }
    }
    Some(String::from_utf8_lossy(&body).into_owned())
}

/// `VerifyLinkService#link_redirects_back?`: whether `test_url` redirects
/// to `link_back`, by the `Location` it answers with, unfollowed.
///
/// Mastodon asks with `HEAD`; ojak's client follows no redirect only for a
/// `GET`, so this asks with that (see the `link-verification-redirect-get`
/// divergence).
async fn redirects_back(http: &ojak::client::Client, test_url: &str, link_back: &str) -> bool {
    if test_url.trim().is_empty() {
        return false;
    }
    let Ok(url) = url::Url::parse(test_url) else {
        return false;
    };
    let Ok(resp) = http
        .get_direct(&url, reqwest::header::HeaderMap::new())
        .await
    else {
        return false;
    };
    resp.headers
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        == Some(link_back)
}

/// Fetch `url` and return whether it links back to `link_back` via
/// `rel="me"`: `VerifyLinkService#link_back_present?`. A `rel="me"` link
/// that matches, case aside, is enough; when there are such links and none
/// matches, the first may still redirect back.
async fn links_back(http: &ojak::client::Client, url: &str, link_back: &str) -> bool {
    let Some(body) = fetch_page(http, url).await else {
        return false;
    };
    if body.trim().is_empty() {
        return false;
    }
    let links = rel_me_hrefs(&body);
    let link_back_lc = link_back.to_lowercase();
    if links
        .iter()
        .flatten()
        .any(|href| href.to_lowercase() == link_back_lc)
    {
        true
    } else if let Some(first) = links.first() {
        redirects_back(http, first.as_deref().unwrap_or(""), link_back).await
    } else {
        false
    }
}

/// `VerifyAccountLinksWorker.perform_async(account_id)`: verify the
/// unverified `rel="me"` links on a local account's profile fields, stamping
/// `verified_at` on success.
pub async fn verify(state: &AppState, account_id: i64) {
    crate::jobs::push(state, VerifyAccountLinksWorker { account_id }).await;
}

/// The longest `VerifyAccountLinksWorker` waits after a remote account is
/// stored (`ProcessAccountService::VERIFY_DELAY`).
const VERIFY_DELAY: Duration = Duration::from_secs(10 * 60);

/// `VerifyAccountLinksWorker.perform_in(rand(VERIFY_DELAY), id)`, for a
/// remote account `ProcessAccountService` just stored.
pub async fn verify_later(state: &AppState, account_id: i64) {
    let delay = Duration::from_secs(rand::random_range(0..VERIFY_DELAY.as_secs()));
    crate::jobs::push_in(state, delay, VerifyAccountLinksWorker { account_id }).await;
}

/// `VerifyAccountLinksWorker`, once at a time per account.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct VerifyAccountLinksWorker {
    pub account_id: i64,
}

impl crate::jobs::Job for VerifyAccountLinksWorker {
    const KIND: &'static str = "VerifyAccountLinksWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .no_retry()
        .lock(crate::jobs::Lock::UntilExecuted(Duration::from_secs(3600)));

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        verify_account_links(state, self.account_id).await
    }
}

/// `Account::Field#value_for_verification` for a remote account: the URL of
/// a value that is nothing but a link to itself, `<a href="X">X</a>`.
fn remote_value_for_verification(value: &str) -> Option<String> {
    let fragment = Html::parse_fragment(value);
    let root = fragment.root_element();
    let mut children = root.children();
    let only = children.next()?;
    if children.next().is_some() {
        return None;
    }
    let element = scraper::ElementRef::wrap(only)?;
    if element.value().name() != "a" {
        return None;
    }
    let href = element.value().attr("href")?;
    (href == element.text().collect::<String>()).then(|| href.to_owned())
}

/// `Account::Field#requires_verification?` for one of a remote account's
/// stored fields.
fn remote_field_requires_verification(field: &Value) -> bool {
    let verified = field.get("verified_at").is_some_and(|v| !v.is_null());
    !verified
        && field
            .get("value")
            .and_then(Value::as_str)
            .and_then(remote_value_for_verification)
            .is_some_and(|url| is_verifiable(&url))
}

/// `@account.fields.any?(&:requires_verification?)`, for a remote account.
pub fn any_requires_verification_remote(fields: &Value) -> bool {
    fields
        .as_array()
        .is_some_and(|fields| fields.iter().any(remote_field_requires_verification))
}

async fn verify_account_links(state: &AppState, account_id: i64) -> anyhow::Result<()> {
    let row = sqlx::query!(
        r#"SELECT username, domain, url, fields FROM accounts WHERE id = $1"#,
        account_id,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let remote = row.domain.is_some();
    let Some(mut fields) = row.fields.and_then(|v| v.as_array().cloned()) else {
        return Ok(());
    };

    // `ActivityPub::TagManager#url_for`: a remote account's own `url`, when
    // it is an http(s) one.
    let link_back = if remote {
        match row
            .url
            .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
        {
            Some(url) => url,
            None => return Ok(()),
        }
    } else {
        format!("https://{}/@{}", state.urls.local_domain, row.username)
    };

    let mut changed = false;
    for field in &mut fields {
        let already_verified = field.get("verified_at").is_some_and(|v| !v.is_null());
        let Some(value) = field.get("value").and_then(|v| v.as_str()) else {
            continue;
        };
        let value = if remote {
            match remote_value_for_verification(value) {
                Some(url) => url,
                None => continue,
            }
        } else {
            value.to_owned()
        };
        if already_verified || !is_verifiable(&value) {
            continue;
        }
        if links_back(&state.fetch, &value, &link_back).await {
            let now = crate::api::mastodon::convert::mastodon_date(chrono::Utc::now().naive_utc());
            if let Some(obj) = field.as_object_mut() {
                obj.insert("verified_at".into(), Value::String(now));
                changed = true;
            }
        }
    }

    if changed {
        sqlx::query!(
            "UPDATE accounts SET fields = $1, updated_at = now() WHERE id = $2",
            Value::Array(fields),
            account_id,
        )
        .execute(&state.db)
        .await?;
        // `account.save! if account.changed?`.
        crate::moderation::webhooks::account_updated(state, account_id).await;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page server on the loopback, and a client allowed to reach it.
    async fn page_server() -> (String, ojak::client::Client) {
        use axum::{http::header, routing::get, Router};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let back = "https://social.example/@alice";
        let short = format!("{base}/short");
        let app = Router::new()
            // A page answering 200 with no HTML content type.
            .route(
                "/octet",
                get(move || async move {
                    (
                        [(header::CONTENT_TYPE, "application/octet-stream")],
                        format!(r#"<a rel="me" href="{back}">me</a>"#),
                    )
                }),
            )
            // A page answering anything but exactly 200.
            .route(
                "/created",
                get(move || async move {
                    (
                        axum::http::StatusCode::CREATED,
                        format!(r#"<a rel="me" href="{back}">me</a>"#),
                    )
                }),
            )
            // A page whose first `rel="me"` link redirects back.
            .route(
                "/shortened",
                get(move || {
                    let short = short.clone();
                    async move {
                        format!(
                            r#"<a rel="me" href="{short}">me</a><a rel="me" href="https://else.example">x</a>"#
                        )
                    }
                }),
            )
            .route(
                "/short",
                get(move || async move {
                    (
                        axum::http::StatusCode::MOVED_PERMANENTLY,
                        [(header::LOCATION, back)],
                    )
                }),
            )
            // A page whose first `rel="me"` link redirects elsewhere.
            .route(
                "/elsewhere",
                get(|| async {
                    r#"<a rel="me" href="https://else.example/@alice">me</a>"#
                }),
            );
        crate::tenants::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = ojak::client::Client::new(ojak::client::ClientConfig {
            allow_private: vec!["127.0.0.0/8".parse().unwrap()],
            ..Default::default()
        })
        .unwrap();
        (base, client)
    }

    /// `VerifyLinkService`: exactly a 200, whatever its content type, and
    /// `link_redirects_back?` for the first `rel="me"` link when none
    /// matches.
    #[tokio::test]
    async fn links_back_as_verify_link_service() {
        let (base, client) = page_server().await;
        let back = "https://social.example/@alice";
        assert!(links_back(&client, &format!("{base}/octet"), back).await);
        assert!(!links_back(&client, &format!("{base}/created"), back).await);
        assert!(links_back(&client, &format!("{base}/shortened"), back).await);
        assert!(!links_back(&client, &format!("{base}/elsewhere"), back).await);
    }

    #[test]
    fn verifiable_accepts_plain_https_urls() {
        assert!(is_verifiable("https://example.com"));
        assert!(is_verifiable("https://example.com/~me"));
        assert!(is_verifiable("https://sub.example.com/path"));
    }

    #[test]
    fn verifiable_rejects_non_candidates() {
        assert!(!is_verifiable("http://example.com")); // not https
        assert!(!is_verifiable("ftp://example.com"));
        assert!(!is_verifiable("https://user:pass@example.com")); // userinfo
        assert!(!is_verifiable("https://user@example.com"));
        assert!(!is_verifiable("https://xn--80ak6aa92e.com")); // IDN/punycode
        assert!(!is_verifiable("not a url"));
        assert!(!is_verifiable("mailto:me@example.com"));
        assert!(!is_verifiable("")); // blank
    }

    #[test]
    fn rel_me_hrefs_finds_anchor_and_link_elements() {
        let html = r#"
            <html><head>
              <link rel="me" href="https://social.example/@alice">
            </head><body>
              <a rel="me" href="https://other.example/@alice">me</a>
              <a rel="nofollow" href="https://ignore.example">no</a>
              <a href="https://norel.example">no rel</a>
            </body></html>
        "#;
        let hrefs = rel_me_hrefs(html);
        assert_eq!(
            hrefs,
            vec![
                Some("https://social.example/@alice".to_string()),
                Some("https://other.example/@alice".to_string()),
            ],
        );
    }

    #[test]
    fn rel_me_hrefs_matches_multi_token_and_case_insensitively() {
        let html = r#"<a rel="Me nofollow" href="https://a.example">a</a>
                      <a rel="noopener ME" href="https://b.example">b</a>"#;
        let hrefs = rel_me_hrefs(html);
        assert_eq!(
            hrefs,
            vec![
                Some("https://a.example".to_string()),
                Some("https://b.example".to_string())
            ],
        );
    }

    #[test]
    fn rel_me_hrefs_ignores_rel_values_that_merely_contain_me() {
        // "meta" contains the substring "me" but is not the token "me".
        let html = r#"<a rel="meta" href="https://a.example">a</a>"#;
        assert!(rel_me_hrefs(html).is_empty());
    }

    #[test]
    fn remote_values_are_verified_only_when_they_are_a_bare_link() {
        assert_eq!(
            remote_value_for_verification(
                r#"<a href="https://a.example/me" rel="me">https://a.example/me</a>"#
            ),
            Some("https://a.example/me".into())
        );
        assert_eq!(
            remote_value_for_verification(
                r#"<a href="https://a.example/me"><span>a.example/me</span></a>"#
            ),
            None
        );
        assert_eq!(
            remote_value_for_verification("see https://a.example/me"),
            None
        );
    }
}
