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

use std::sync::atomic::{AtomicU64, Ordering};
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

/// Extract the `href`s of every `<a>`/`<link>` element that carries `rel="me"`
/// (rel is a space-separated token list, matched case-insensitively).
fn rel_me_hrefs(html: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    // unwrap: static selector, always valid.
    let sel = Selector::parse("a[rel], link[rel]").unwrap();
    doc.select(&sel)
        .filter_map(|el| {
            let rel = el.value().attr("rel")?;
            let is_me = rel.split_whitespace().any(|t| t.eq_ignore_ascii_case("me"));
            if is_me {
                el.value().attr("href").map(str::to_string)
            } else {
                None
            }
        })
        .collect()
}

/// Fetch `url` and return whether it links back to `link_back` via `rel="me"`.
/// Mirrors `VerifyLinkService#link_back_present?` (minus the redirect-follow
/// fallback used by a handful of services).
async fn links_back(http: &reqwest::Client, url: &str, link_back: &str) -> bool {
    // Reuse the SSRF-guarded client and validate the target up front, matching
    // how preview cards are fetched.
    if crate::federation::safe_fetch::validate_url(url).is_err() {
        return false;
    }
    let Ok(resp) = http
        .get(url)
        .header("Accept", "text/html")
        .timeout(Duration::from_secs(5))
        .send()
        .await
    else {
        return false;
    };
    if !resp.status().is_success() {
        return false;
    }
    let is_html = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|ct| ct.contains("text/html"));
    if !is_html {
        return false;
    }
    let Ok(body) = resp.text().await else {
        return false;
    };
    let link_back_lc = link_back.to_lowercase();
    rel_me_hrefs(&body)
        .iter()
        .any(|href| href.to_lowercase() == link_back_lc)
}

/// Spawn a background task that verifies the unverified `rel="me"` links on a
/// local account's profile fields, stamping `verified_at` on success.
pub fn spawn(state: &AppState, account_id: i64) {
    let state = state.clone();
    crate::tenants::spawn(async move {
        if let Err(e) = verify_account_links(&state, account_id).await {
            tracing::warn!(error = %e, account_id, "link verification failed");
        }
    });
}

/// The longest `VerifyAccountLinksWorker` waits after a remote account is
/// stored (`ProcessAccountService::VERIFY_DELAY`), in seconds.
static VERIFY_DELAY_SECS: AtomicU64 = AtomicU64::new(10 * 60);

/// Verify remote accounts' links at once, for tests.
pub fn verify_without_delay_for_tests() {
    VERIFY_DELAY_SECS.store(0, Ordering::SeqCst);
}

/// `VerifyAccountLinksWorker.perform_in(rand(VERIFY_DELAY), id)`, for a
/// remote account `ProcessAccountService` just stored. Once at a time per
/// account (`lock: :until_executed, lock_ttl: 1.hour`).
pub fn spawn_later(state: &AppState, account_id: i64) {
    let state = state.clone();
    crate::tenants::spawn(async move {
        let Some(lock) = crate::redis_lock::try_acquire(
            &state,
            &format!("verify_account_links:{account_id}"),
            60 * 60 * 1000,
        )
        .await
        else {
            return;
        };
        let most = VERIFY_DELAY_SECS.load(Ordering::SeqCst);
        if most > 0 {
            tokio::time::sleep(Duration::from_secs(rand::random_range(0..most))).await;
        }
        if let Err(e) = verify_account_links(&state, account_id).await {
            tracing::warn!(error = %e, account_id, "link verification failed");
        }
        drop(lock);
    });
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
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                "https://social.example/@alice".to_string(),
                "https://other.example/@alice".to_string(),
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
                "https://a.example".to_string(),
                "https://b.example".to_string()
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
