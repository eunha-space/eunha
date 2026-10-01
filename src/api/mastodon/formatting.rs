use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;
use unicode_segmentation::UnicodeSegmentation;

use super::types::StatusMention;

/// Number of characters a URL counts as, regardless of its real length
/// (Mastodon `StatusLengthValidator::URL_PLACEHOLDER_CHARS`).
const URL_PLACEHOLDER_CHARS: usize = 23;

/// Countable length of a status for the 500-char limit, matching Mastodon's
/// `StatusLengthValidator`: the spoiler/CW text plus the body, where every URL
/// counts as 23 characters and every mention drops its `@domain` part, measured
/// in grapheme clusters (not codepoints).
pub fn countable_length(text: &str, spoiler_text: &str) -> usize {
    let combined = format!("{spoiler_text}{}", countable_text(text));
    combined.graphemes(true).count()
}

/// Rewrite `text` into the form Mastodon counts: URLs → a fixed placeholder,
/// mentions → `@username` with the domain stripped. Overlapping entities are
/// resolved earliest-first, mirroring `Extractor.remove_overlapping_entities`.
fn countable_text(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    struct Entity {
        start: usize,
        end: usize,
        replacement: String,
    }
    let mut entities: Vec<Entity> = Vec::new();

    for m in URL_RE.find_iter(text) {
        entities.push(Entity {
            start: m.start(),
            end: m.end(),
            replacement: "x".repeat(URL_PLACEHOLDER_CHARS),
        });
    }
    for caps in MENTION_RE.captures_iter(text) {
        // Group 2 is the username; the literal '@' sits one byte before it.
        let user = caps.get(2).unwrap();
        let at = user.start() - 1;
        let end = caps.get(3).map(|g| g.end()).unwrap_or_else(|| user.end());
        entities.push(Entity {
            start: at,
            end,
            replacement: format!("@{}", user.as_str()),
        });
    }

    entities.sort_by_key(|e| e.start);
    let mut result = String::with_capacity(text.len());
    let mut last = 0usize;
    for e in &entities {
        if e.start < last {
            continue; // overlaps an already-rewritten entity
        }
        result.push_str(&text[last..e.start]);
        result.push_str(&e.replacement);
        last = e.end;
    }
    result.push_str(&text[last..]);
    result
}

pub static HASHTAG_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(^|[\s,.:;!?\(\[\{/])#([a-zA-Z][a-zA-Z0-9_]*)").unwrap());

pub static MENTION_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(^|[\s,.:;!?\(\[\{/])@([a-zA-Z0-9_]+)(?:@([a-zA-Z0-9._:\-]+))?").unwrap()
});

pub static URL_RE: Lazy<Regex> = Lazy::new(|| Regex::new("https?://[^\\s<>&\"]+").unwrap());

fn html_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn html_escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn render_content(
    text: &str,
    domain: &str,
    mention_map: &HashMap<String, (String, String)>,
) -> String {
    if text.is_empty() {
        return String::new();
    }
    text.split("\n\n")
        .map(|para| {
            let linked = linkify_entities(para, domain, mention_map);
            format!("<p>{}</p>", linked.replace('\n', "<br />"))
        })
        .collect::<Vec<_>>()
        .join("")
}

fn linkify_entities(
    text: &str,
    domain: &str,
    mention_map: &HashMap<String, (String, String)>,
) -> String {
    struct Entity {
        start: usize,
        end: usize,
        html: String,
    }

    let mut entities: Vec<Entity> = Vec::new();

    for cap in HASHTAG_RE.captures_iter(text) {
        let full = cap.get(0).unwrap();
        let prefix_len = cap.get(1).unwrap().as_str().len();
        let tag_text = &cap[2];
        let tag_lower = tag_text.to_lowercase();
        let url = format!(
            "https://{}/tags/{}",
            domain,
            urlencoding::encode(&tag_lower)
        );
        entities.push(Entity {
            start: full.start() + prefix_len,
            end: full.end(),
            html: format!(
                r#"<a href="{}" class="mention hashtag" rel="tag">#<span>{}</span></a>"#,
                html_escape_attr(&url),
                html_escape_text(tag_text),
            ),
        });
    }

    for cap in MENTION_RE.captures_iter(text) {
        let full = cap.get(0).unwrap();
        let prefix_len = cap.get(1).unwrap().as_str().len();
        let username = cap[2].to_lowercase();
        let mention_domain = cap.get(3).map(|m| m.as_str().to_lowercase());
        let key = match &mention_domain {
            Some(d) => format!("{}@{}", username, d),
            None => username.clone(),
        };
        if let Some((url, display)) = mention_map.get(&key) {
            entities.push(Entity {
                start: full.start() + prefix_len,
                end: full.end(),
                html: format!(
                    r#"<span class="h-card" translate="no"><a href="{}" class="u-url mention">@<span>{}</span></a></span>"#,
                    html_escape_attr(url),
                    html_escape_text(display),
                ),
            });
        }
    }

    for m in URL_RE.find_iter(text) {
        let url = m.as_str();
        entities.push(Entity {
            start: m.start(),
            end: m.end(),
            html: format!(
                r#"<a href="{}" target="_blank" rel="nofollow noopener noreferrer">{}</a>"#,
                html_escape_attr(url),
                html_escape_text(url),
            ),
        });
    }

    entities.sort_by_key(|e| e.start);

    let mut result = String::with_capacity(text.len() * 2);
    let mut last_end = 0usize;
    for entity in &entities {
        if entity.start < last_end {
            continue;
        }
        result.push_str(&html_escape_text(&text[last_end..entity.start]));
        result.push_str(&entity.html);
        last_end = entity.end;
    }
    result.push_str(&html_escape_text(&text[last_end..]));
    result
}

/// Convert plain announcement text to HTML, wrapping paragraphs and linkifying URLs.
/// Mirrors Mastodon's `AnnouncementSerializer#content` which calls `linkify(object.text)`.
pub fn text_to_html(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    text.split("\n\n")
        .map(|para| {
            let linked = linkify_urls(para);
            format!("<p>{}</p>", linked.replace('\n', "<br />"))
        })
        .collect::<Vec<_>>()
        .join("")
}

pub fn format_field_value(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    linkify_urls(value)
}

fn linkify_urls(text: &str) -> String {
    struct Span {
        start: usize,
        end: usize,
        html: String,
    }
    let mut spans: Vec<Span> = Vec::new();
    for m in URL_RE.find_iter(text) {
        let url = m.as_str();
        spans.push(Span {
            start: m.start(),
            end: m.end(),
            html: format!(
                r#"<a href="{}" target="_blank" rel="nofollow noopener noreferrer">{}</a>"#,
                html_escape_attr(url),
                html_escape_text(url),
            ),
        });
    }
    let mut result = String::with_capacity(text.len() * 2);
    let mut last_end = 0usize;
    for span in &spans {
        result.push_str(&html_escape_text(&text[last_end..span.start]));
        result.push_str(&span.html);
        last_end = span.end;
    }
    result.push_str(&html_escape_text(&text[last_end..]));
    result
}

/// Build a mention lookup map from a `StatusMention` slice.
/// Keys are the lowercase acct handle (`user` or `user@domain`).
pub fn mention_map_from_api(
    mentions: &[StatusMention],
    local_domain: &str,
) -> HashMap<String, (String, String)> {
    let mut map = HashMap::new();
    for m in mentions {
        let key_short = m.username.to_lowercase();
        map.entry(key_short.clone())
            .or_insert_with(|| (m.url.clone(), m.acct.clone()));
        if m.acct.contains('@') {
            map.entry(m.acct.to_lowercase())
                .or_insert_with(|| (m.url.clone(), m.acct.clone()));
        } else if !local_domain.is_empty() {
            // A local mention's acct is bare (`bob`), but the source text may use
            // the fully-qualified `@bob@this.instance` form; map that key too so
            // it still renders as a link.
            map.entry(format!("{}@{}", key_short, local_domain.to_lowercase()))
                .or_insert_with(|| (m.url.clone(), m.acct.clone()));
        }
    }
    map
}

/// `PlainTextFormatter`, as `FormattingHelper.extract_status_plain_text` uses it
/// for a remote post: line breaks and paragraph breaks kept, tags dropped,
/// entities decoded.
pub fn html_to_plain_text(html: &str) -> String {
    static BR: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"(?i)<br\s*/?>").expect("valid regex"));
    static PARAGRAPH: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?i)</p>\s*<p[^>]*>").expect("valid regex")
    });
    let html = BR.replace_all(html, "\n");
    let html = PARAGRAPH.replace_all(&html, "\n\n");
    scraper::Html::parse_fragment(&html)
        .root_element()
        .text()
        .collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::countable_length;

    #[test]
    fn html_to_plain_text_keeps_breaks() {
        assert_eq!(
            super::html_to_plain_text("<p>a &amp; b<br>c</p><p>d</p>"),
            "a & b\nc\n\nd"
        );
    }

    #[test]
    fn plain_text_counts_graphemes() {
        assert_eq!(countable_length("hello", ""), 5);
    }

    #[test]
    fn spoiler_text_is_included() {
        // 3 (spoiler) + 5 (body) = 8
        assert_eq!(countable_length("hello", "cw:"), 8);
    }

    #[test]
    fn url_counts_as_23_regardless_of_length() {
        let url = "https://example.com/a/very/long/path/that/is/way/over/23/characters";
        assert!(url.len() > 23);
        assert_eq!(countable_length(url, ""), 23);
        // "see " (4) + url (23) = 27
        assert_eq!(countable_length(&format!("see {url}"), ""), 27);
    }

    #[test]
    fn mention_drops_domain() {
        // "@alice" (6) + " hi" (3) = 9, the "@remote.example.org" is not counted.
        assert_eq!(countable_length("@alice@remote.example.org hi", ""), 9);
        // A local mention (no domain) is unchanged: "@bob" (4) + " hi" (3) = 7.
        assert_eq!(countable_length("@bob hi", ""), 7);
    }
}
