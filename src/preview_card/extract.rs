//! `LinkDetailsExtractor`: what a page says about itself, read the way
//! Mastodon reads it — JSON-LD first, then OpenGraph and Twitter tags, then
//! plain HTML.

use std::sync::LazyLock;

use regex::Regex;
use scraper::{ElementRef, Html, Selector};
use serde_json::Value;
use url::Url;

/// Card types, as `PreviewCard`'s `type` enum stores them.
pub const TYPE_LINK: i32 = 0;
pub const TYPE_PHOTO: i32 = 1;
pub const TYPE_VIDEO: i32 = 2;
pub const TYPE_RICH: i32 = 3;

/// `PreviewCard`'s `link_type` enum.
pub const LINK_TYPE_UNKNOWN: i32 = 0;
pub const LINK_TYPE_ARTICLE: i32 = 1;

/// `LinkDetailsExtractor#to_preview_card_attributes`, with the canonical URL
/// and the `fediverse:creator` handle the service reads separately.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LinkDetails {
    pub title: String,
    pub description: String,
    pub image: Option<String>,
    pub image_description: String,
    pub card_type: i32,
    pub link_type: i32,
    pub width: i32,
    pub height: i32,
    pub html: String,
    pub provider_name: String,
    pub provider_url: String,
    pub author_name: String,
    pub author_url: String,
    pub embed_url: String,
    pub language: Option<String>,
    pub published_at: Option<chrono::NaiveDateTime>,
    pub canonical_url: String,
    pub author_account: Option<String>,
}

/// Read `html`, fetched from `url` (the last URL in the redirect chain).
pub fn extract(url: &Url, html: &str) -> LinkDetails {
    Page::new(url, html).details()
}

struct Meta {
    property: Option<String>,
    name: Option<String>,
    content: Option<String>,
}

struct Page<'a> {
    url: &'a Url,
    document: Html,
    metas: Vec<Meta>,
    structured: Option<StructuredData>,
}

impl<'a> Page<'a> {
    fn new(url: &'a Url, html: &str) -> Self {
        static META: LazyLock<Selector> = LazyLock::new(|| Selector::parse("meta").unwrap());
        let document = Html::parse_document(html);
        let metas = document
            .select(&META)
            .map(|m| Meta {
                property: m.value().attr("property").map(str::to_owned),
                name: m.value().attr("name").map(str::to_owned),
                content: m.value().attr("content").map(str::to_owned),
            })
            .collect();
        let structured = structured_data(&document);
        Page {
            url,
            document,
            metas,
            structured,
        }
    }

    fn details(&self) -> LinkDetails {
        let sd = self.structured.as_ref();
        let player_url = self.valid_url_or_nil(self.opengraph_tag("twitter:player"), false);
        let width = self.opengraph_tag("twitter:player:width");
        let height = self.opengraph_tag("twitter:player:height");

        let title = sd
            .and_then(StructuredData::headline)
            .or_else(|| self.opengraph_tag("og:title"))
            .or_else(|| self.head_title());
        let description = sd
            .and_then(StructuredData::description)
            .or_else(|| self.opengraph_tag("og:description"))
            .or_else(|| self.meta_tag("description"));
        let published_at = sd
            .and_then(StructuredData::date_published)
            .or_else(|| self.opengraph_tag("article:published_time"))
            .filter(|s| !is_blank(s))
            .and_then(|s| parse_time(&s));
        let provider_name = sd
            .and_then(StructuredData::publisher_name)
            .or_else(|| self.opengraph_tag("og:site_name"));
        let author_name = sd
            .and_then(StructuredData::author_name)
            .or_else(|| self.opengraph_tag("og:author"))
            .or_else(|| self.opengraph_tag("og:author:username"));
        let language = sd
            .and_then(StructuredData::language)
            .or_else(|| self.opengraph_tag("og:locale"))
            .or_else(|| {
                self.document
                    .root_element()
                    .value()
                    .attr("lang")
                    .map(str::to_owned)
            });
        let link_type = if sd
            .and_then(|sd| sd.json.get("@type"))
            .and_then(Value::as_str)
            == Some("NewsArticle")
            || self.opengraph_tag("og:type").as_deref() == Some("article")
        {
            LINK_TYPE_ARTICLE
        } else {
            LINK_TYPE_UNKNOWN
        };
        let canonical_url = self
            .valid_url_or_nil(
                self.link_tag("canonical")
                    .or_else(|| self.opengraph_tag("og:url")),
                true,
            )
            .unwrap_or_else(|| self.url.to_string());

        LinkDetails {
            title: ruby_strip(&decode_entities(title.as_deref().unwrap_or(""))).to_owned(),
            description: decode_entities(description.as_deref().unwrap_or("")),
            image: self.valid_url_or_nil(self.opengraph_tag("og:image"), false),
            image_description: self.opengraph_tag("og:image:alt").unwrap_or_default(),
            card_type: if player_url.is_some() {
                TYPE_VIDEO
            } else {
                TYPE_LINK
            },
            link_type,
            html: player_url
                .as_deref()
                .map(|src| iframe(src, width.as_deref(), height.as_deref()))
                .unwrap_or_default(),
            width: width.as_deref().map(ruby_to_i).unwrap_or(0),
            height: height.as_deref().map(ruby_to_i).unwrap_or(0),
            provider_name: decode_entities(provider_name.as_deref().unwrap_or("")),
            provider_url: self
                .valid_url_or_nil(host_to_url(self.opengraph_tag("og:site")), false)
                .unwrap_or_default(),
            author_name: decode_entities(author_name.as_deref().unwrap_or("")),
            author_url: sd.and_then(StructuredData::author_url).unwrap_or_default(),
            embed_url: self
                .valid_url_or_nil(self.opengraph_tag("twitter:player:stream"), false)
                .unwrap_or_default(),
            language: valid_locale_or_nil(language.as_deref()),
            published_at,
            canonical_url,
            author_account: self.opengraph_tag("fediverse:creator"),
        }
    }

    /// `//meta[casecmp(@property, name) or casecmp(@name, name)]/@content`:
    /// the first such tag anywhere in the document, even one without content.
    fn opengraph_tag(&self, name: &str) -> Option<String> {
        self.metas
            .iter()
            .find(|m| {
                m.property
                    .as_deref()
                    .is_some_and(|p| p.eq_ignore_ascii_case(name))
                    || m.name
                        .as_deref()
                        .is_some_and(|n| n.eq_ignore_ascii_case(name))
            })
            .and_then(|m| m.content.clone())
    }

    fn meta_tag(&self, name: &str) -> Option<String> {
        self.metas
            .iter()
            .find(|m| {
                m.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
            })
            .and_then(|m| m.content.clone())
    }

    /// `//link[link_rel_include(@rel, name)]/@href`.
    fn link_tag(&self, name: &str) -> Option<String> {
        static LINK: LazyLock<Selector> = LazyLock::new(|| Selector::parse("link").unwrap());
        let name = name.to_lowercase();
        self.document
            .select(&LINK)
            .find(|l| {
                l.value().attr("rel").is_some_and(|rel| {
                    rel.to_lowercase()
                        .split([' ', '\t', '\n', '\x0C', '\r'])
                        .any(|token| token == name)
                })
            })
            .and_then(|l| l.value().attr("href").map(str::to_owned))
    }

    /// `head.at_xpath('title')`: a `<title>` directly in the head.
    fn head_title(&self) -> Option<String> {
        static HEAD: LazyLock<Selector> = LazyLock::new(|| Selector::parse("head").unwrap());
        let head = self.document.select(&HEAD).next()?;
        head.children()
            .filter_map(ElementRef::wrap)
            .find(|e| e.value().name() == "title")
            .map(|t| t.text().collect())
    }

    /// `valid_url_or_nil`: `str` resolved against the page, if it is an
    /// HTTP(S) URL with a host — and, for `same_origin_only`, that host.
    fn valid_url_or_nil(&self, s: Option<String>, same_origin_only: bool) -> Option<String> {
        let s = s?;
        if is_blank(&s) || s == "null" || s == "undefined" {
            return None;
        }
        let url = self.url.join(&s).ok()?;
        let host = url.host_str().filter(|h| !h.is_empty())?;
        if !matches!(url.scheme(), "http" | "https")
            || (same_origin_only && Some(host) != self.url.host_str())
        {
            return None;
        }
        Some(url.to_string())
    }
}

fn host_to_url(s: Option<String>) -> Option<String> {
    let s = s.filter(|s| !is_blank(s))?;
    static SCHEME: LazyLock<Regex> = LazyLock::new(|| Regex::new("^https?://").unwrap());
    Some(if SCHEME.is_match(&s) {
        s
    } else {
        format!("http://{s}")
    })
}

/// `content_tag(:iframe, nil, src:, width:, height:, allowfullscreen: 'true',
/// allowtransparency: 'true', scrolling: 'no', frameborder: '0')`. Rails
/// omits nil attributes and writes `allowfullscreen`, a boolean attribute, as
/// its own name.
fn iframe(src: &str, width: Option<&str>, height: Option<&str>) -> String {
    let mut out = format!(r#"<iframe src="{}""#, escape_attr(src));
    if let Some(width) = width {
        out.push_str(&format!(r#" width="{}""#, escape_attr(width)));
    }
    if let Some(height) = height {
        out.push_str(&format!(r#" height="{}""#, escape_attr(height)));
    }
    out.push_str(
        r#" allowfullscreen="allowfullscreen" allowtransparency="true" scrolling="no" frameborder="0"></iframe>"#,
    );
    out
}

/// `ERB::Util.html_escape`.
fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Ruby's `String#blank?`: empty or whitespace only.
pub(crate) fn is_blank(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

/// Ruby's `String#strip`.
fn ruby_strip(s: &str) -> &str {
    s.trim_start_matches(['\t', '\n', '\x0B', '\x0C', '\r', ' '])
        .trim_end_matches(['\0', '\t', '\n', '\x0B', '\x0C', '\r', ' '])
}

/// What ActiveModel's integer cast makes of a string: Ruby's `String#to_i`,
/// the leading digits (and sign), or 0.
pub(crate) fn ruby_to_i(s: &str) -> i32 {
    let s = s.trim_start();
    let (sign, digits) = match s.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    let digits: String = digits
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .filter(|c| *c != '_')
        .collect();
    digits
        .parse::<i64>()
        .map(|n| (sign * n).clamp(i32::MIN as i64, i32::MAX as i64) as i32)
        .unwrap_or(0)
}

/// `LanguagesHelper#valid_locale_or_nil`.
pub(crate) fn valid_locale_or_nil(s: Option<&str>) -> Option<String> {
    let s = s.filter(|s| !is_blank(s))?;
    if crate::languages::valid_locale(Some(s)) {
        return Some(s.to_owned());
    }
    let code = s.split(['_', '-']).next().unwrap_or_default();
    crate::languages::valid_locale(Some(code)).then(|| code.to_owned())
}

/// A datetime column's cast of a string: ISO 8601 in its common shapes, or
/// RFC 2822. Anything else is no time at all.
pub(crate) fn parse_time(s: &str) -> Option<chrono::NaiveDateTime> {
    let s = s.trim();
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(t.naive_utc());
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%m-%d %H:%M:%S%.f%z",
        "%Y-%m-%dT%H:%M%z",
    ] {
        if let Ok(t) = chrono::DateTime::parse_from_str(s, format) {
            return Some(t.naive_utc());
        }
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(t) = chrono::NaiveDateTime::parse_from_str(s, format) {
            return Some(t);
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return d.and_hms_opt(0, 0, 0);
    }
    chrono::DateTime::parse_from_rfc2822(s)
        .ok()
        .map(|t| t.naive_utc())
}

// ── HTML entities ──────────────────────────────────────────────────────────

/// `HTMLEntities.new(:expanded).decode`: named entities (here from the HTML
/// standard's table, a superset of the gem's) and numeric ones, each only
/// with its closing semicolon. An unknown name is left as written.
pub(crate) fn decode_entities(s: &str) -> String {
    static ENTITY: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)&(?:((?:b\.)?[a-z][a-z0-9]{1,31})|#([0-9]{1,7})|#x([0-9a-f]{1,6}));")
            .unwrap()
    });
    if !s.contains('&') {
        return s.to_owned();
    }
    ENTITY
        .replace_all(s, |caps: &regex::Captures| {
            let whole = caps.get(0).unwrap().as_str();
            if let Some(name) = caps.get(1) {
                return htmlize::ENTITIES
                    .get(format!("&{};", name.as_str()).as_bytes())
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .unwrap_or(whole)
                    .to_owned();
            }
            let code = match (caps.get(2), caps.get(3)) {
                (Some(dec), _) => dec.as_str().parse::<u32>().ok(),
                (_, Some(hex)) => u32::from_str_radix(hex.as_str(), 16).ok(),
                _ => None,
            };
            code.and_then(char::from_u32)
                .map(String::from)
                .unwrap_or_else(|| whole.to_owned())
        })
        .into_owned()
}

// ── JSON-LD ────────────────────────────────────────────────────────────────

/// Some publications wrap their JSON-LD in commented-out CDATA blocks, which
/// have to go before the JSON can be parsed.
static CDATA_JUNK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^\s*((/\*\s*<!\[CDATA\[\s*\*/)|(//\s*<!\[CDATA\[)|(/\*\s*\]\]>\s*\*/)|(//\s*\]\]>))\s*$",
    )
    .unwrap()
});

/// `LinkDetailsExtractor::StructuredData`: the first object, in the first
/// `<script type="application/ld+json">` that parses, typed `NewsArticle` or
/// `WebPage`.
struct StructuredData {
    json: serde_json::Map<String, Value>,
}

fn structured_data(document: &Html) -> Option<StructuredData> {
    static SCRIPT: LazyLock<Selector> = LazyLock::new(|| Selector::parse("script").unwrap());
    document
        .select(&SCRIPT)
        .filter(|s| s.value().attr("type") == Some("application/ld+json"))
        .find_map(|script| {
            let content: String = script.text().collect();
            let json_ld = CDATA_JUNK.replace_all(&content, "");
            if is_blank(&json_ld) {
                return None;
            }
            let parsed: Value = serde_json::from_str(&decode_entities(&json_ld)).ok()?;
            let roots = match parsed {
                Value::Array(items) => items,
                other => vec![other],
            };
            roots.into_iter().find_map(|obj| match obj {
                Value::Object(map)
                    if matches!(
                        map.get("@type").and_then(Value::as_str),
                        Some("NewsArticle" | "WebPage")
                    ) =>
                {
                    Some(StructuredData { json: map })
                }
                _ => None,
            })
        })
}

impl StructuredData {
    fn headline(&self) -> Option<String> {
        ruby_string(text_or_language_tagged_string(self.json.get("headline")))
    }

    fn description(&self) -> Option<String> {
        ruby_string(text_or_language_tagged_string(self.json.get("description")))
    }

    fn language(&self) -> Option<String> {
        let mut lang = self.json.get("inLanguage");
        if let Some(Value::Array(items)) = lang {
            lang = items.first();
        }
        match lang {
            Some(Value::Object(map)) => map
                .get("alternateName")
                .filter(|v| truthy(v))
                .or_else(|| map.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        }
    }

    fn date_published(&self) -> Option<String> {
        ruby_string(self.json.get("datePublished"))
    }

    fn author_name(&self) -> Option<String> {
        match index(first_of_hash(self.json.get("author")), "name") {
            Some(Value::Array(names)) => Some(
                names
                    .iter()
                    .map(|n| ruby_string(Some(n)).unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            other => ruby_string(other.as_ref()),
        }
    }

    fn author_url(&self) -> Option<String> {
        ruby_string(index(first_of_hash(self.json.get("author")), "url").as_ref())
    }

    fn publisher_name(&self) -> Option<String> {
        ruby_string(index(first_of_hash(self.json.get("publisher")), "name").as_ref())
    }
}

/// `first_of_hash`: an array's first object, or the value itself.
fn first_of_hash(value: Option<&Value>) -> Option<&Value> {
    fn flat_first(items: &[Value]) -> Option<&Value> {
        items.iter().find_map(|item| match item {
            Value::Object(_) => Some(item),
            Value::Array(nested) => flat_first(nested),
            _ => None,
        })
    }
    match value {
        Some(Value::Array(items)) => flat_first(items),
        other => other,
    }
}

/// `value[key]` in Ruby: a hash's entry, or — for a string — the key itself
/// when the string contains it.
fn index(value: Option<&Value>, key: &str) -> Option<Value> {
    match value? {
        Value::Object(map) => map.get(key).cloned(),
        Value::String(s) if s.contains(key) => Some(Value::String(key.to_owned())),
        _ => None,
    }
}

fn text_or_language_tagged_string(value: Option<&Value>) -> Option<&Value> {
    match value {
        Some(Value::Object(map))
            if map.get("@value").is_some_and(truthy)
                && map.get("@language").is_some_and(truthy) =>
        {
            map.get("@value")
        }
        other => other,
    }
}

fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
}

/// A JSON value as Ruby would hand it on: nothing for `nil` and `false`, and
/// otherwise its text.
fn ruby_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null | Value::Bool(false) => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

// ── Character sets ─────────────────────────────────────────────────────────

/// Turn a page's bytes into text. Mastodon asks ICU's detector first and the
/// `Content-Type` charset second, taking the first that decodes the bytes
/// cleanly; this takes, in order, a byte-order mark, UTF-8 when the bytes are
/// UTF-8, the header's charset, a `<meta charset>` in the first kilobyte, and
/// then Mozilla's detector.
pub fn decode_html(bytes: &[u8], header_charset: Option<&str>) -> String {
    if let Some((encoding, _)) = encoding_rs::Encoding::for_bom(bytes) {
        return encoding.decode_with_bom_removal(bytes).0.into_owned();
    }
    if is_utf8(bytes) {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let declared = header_charset
        .and_then(|c| encoding_rs::Encoding::for_label(c.trim().as_bytes()))
        .into_iter()
        .chain(meta_charset(bytes));
    for encoding in declared {
        let (text, had_errors) = encoding.decode_without_bom_handling(bytes);
        if !had_errors {
            return text.into_owned();
        }
    }
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    detector.feed(bytes, true);
    let encoding = detector.guess(None, chardetng::Utf8Detection::Allow);
    encoding.decode_without_bom_handling(bytes).0.into_owned()
}

/// Valid UTF-8, allowing for a character the body limit cut in half.
fn is_utf8(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && bytes.len() - e.valid_up_to() < 4,
    }
}

fn meta_charset(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    static CHARSET: LazyLock<regex::bytes::Regex> = LazyLock::new(|| {
        regex::bytes::Regex::new(r#"(?i)<meta[^>]+charset\s*=\s*["']?\s*([a-z0-9_\-:.]+)"#).unwrap()
    });
    let head = &bytes[..bytes.len().min(1024)];
    let label = CHARSET.captures(head)?.get(1)?.as_bytes();
    encoding_rs::Encoding::for_label(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(url: &str, html: &str) -> LinkDetails {
        extract(&Url::parse(url).unwrap(), html)
    }

    #[test]
    fn opengraph_tags_fill_the_card() {
        let d = page(
            "https://news.example/a/1",
            r#"<!doctype html><html lang="en-US"><head>
               <title>Fallback</title>
               <meta property="og:title" content="Tom &amp;amp; Jerry">
               <meta property="OG:DESCRIPTION" content="A cat &amp; a mouse">
               <meta property="og:image" content="/img/cover.jpg">
               <meta property="og:image:alt" content="Both of them">
               <meta property="og:type" content="article">
               <meta property="og:site_name" content="News">
               <meta name="og:site" content="news.example">
               <meta property="article:published_time" content="2024-05-06T07:08:09Z">
               <meta property="og:author" content="Ann">
               <meta name="fediverse:creator" content="@ann@social.example">
               </head><body></body></html>"#,
        );
        // Entities are decoded twice over: once by the parser, once more by
        // HTMLEntities, as in Mastodon.
        assert_eq!(d.title, "Tom & Jerry");
        assert_eq!(d.description, "A cat & a mouse");
        assert_eq!(
            d.image.as_deref(),
            Some("https://news.example/img/cover.jpg"),
            "a relative og:image resolves against the page"
        );
        assert_eq!(d.image_description, "Both of them");
        assert_eq!(d.card_type, TYPE_LINK);
        assert_eq!(d.link_type, LINK_TYPE_ARTICLE);
        assert_eq!(d.provider_name, "News");
        assert_eq!(d.provider_url, "http://news.example/");
        assert_eq!(d.author_name, "Ann");
        assert_eq!(d.language.as_deref(), Some("en"), "the region is dropped");
        assert_eq!(
            d.published_at,
            chrono::NaiveDate::from_ymd_opt(2024, 5, 6)
                .unwrap()
                .and_hms_opt(7, 8, 9)
        );
        assert_eq!(d.canonical_url, "https://news.example/a/1");
        assert_eq!(d.author_account.as_deref(), Some("@ann@social.example"));
        assert_eq!(d.html, "");
        assert_eq!((d.width, d.height), (0, 0));
    }

    #[test]
    fn the_title_tag_is_the_last_resort() {
        let d = page(
            "https://example.com/",
            "<html><head><title>\n  Plain page  \n</title></head><body><p>x</p></body></html>",
        );
        assert_eq!(d.title, "Plain page");
        assert_eq!(d.description, "");
        assert_eq!(d.link_type, LINK_TYPE_UNKNOWN);
        assert_eq!(d.language, None);
        assert_eq!(d.image, None);
    }

    #[test]
    fn an_empty_og_title_still_wins_over_the_title_tag() {
        let d = page(
            "https://example.com/",
            r#"<html><head><title>Tag</title><meta property="og:title" content=""></head></html>"#,
        );
        assert_eq!(d.title, "", "Ruby's || treats an empty string as present");
    }

    #[test]
    fn json_ld_news_article_wrapped_in_cdata_junk() {
        let d = page(
            "https://paper.example/story",
            r#"<html lang="fr"><head>
               <meta property="og:title" content="OG title">
               <script type="application/ld+json">{ not json at all</script>
               <script type="application/ld+json">
               //<![CDATA[
               [{"@type": "Organization", "name": "Nope"},
                {"@type": "NewsArticle",
                 "headline": {"@value": "Ledger &amp; Lines", "@language": "en"},
                 "description": "From the paper",
                 "inLanguage": {"alternateName": "de-DE"},
                 "datePublished": "2023-01-02",
                 "author": [[{"name": ["Ann", "Bob"], "url": "https://paper.example/ann"}]],
                 "publisher": {"name": "The Paper"}}]
               //]]>
               </script>
               </head></html>"#,
        );
        assert_eq!(d.title, "Ledger & Lines");
        assert_eq!(d.description, "From the paper");
        assert_eq!(d.language.as_deref(), Some("de"));
        assert_eq!(d.link_type, LINK_TYPE_ARTICLE);
        assert_eq!(d.author_name, "Ann, Bob");
        assert_eq!(d.author_url, "https://paper.example/ann");
        assert_eq!(d.provider_name, "The Paper");
        assert_eq!(
            d.published_at,
            chrono::NaiveDate::from_ymd_opt(2023, 1, 2)
                .unwrap()
                .and_hms_opt(0, 0, 0)
        );
    }

    #[test]
    fn twitter_player_makes_a_video_with_an_iframe() {
        let d = page(
            "https://video.example/watch?v=1",
            r#"<html><head>
               <meta name="twitter:player" content="https://video.example/embed/1?a=1&amp;b=2">
               <meta name="twitter:player:width" content="640">
               <meta name="twitter:player:height" content="360px">
               <meta name="twitter:player:stream" content="/stream/1.mp4">
               </head></html>"#,
        );
        assert_eq!(d.card_type, TYPE_VIDEO);
        assert_eq!((d.width, d.height), (640, 360));
        assert_eq!(
            d.html,
            r#"<iframe src="https://video.example/embed/1?a=1&amp;b=2" width="640" height="360px" allowfullscreen="allowfullscreen" allowtransparency="true" scrolling="no" frameborder="0"></iframe>"#
        );
        assert_eq!(d.embed_url, "https://video.example/stream/1.mp4");
    }

    #[test]
    fn canonical_url_must_stay_on_the_same_host() {
        let cross = page(
            "https://a.example/post",
            r#"<html><head><link rel="canonical" href="https://b.example/post">
               <meta property="og:url" content="https://a.example/og"></head></html>"#,
        );
        assert_eq!(
            cross.canonical_url, "https://a.example/post",
            "a cross-origin canonical is refused, without falling back to og:url"
        );
        let same = page(
            "https://a.example/post?utm=1",
            r#"<html><head><link rel="Alternate CANONICAL" href="/post"></head></html>"#,
        );
        assert_eq!(same.canonical_url, "https://a.example/post");
        let og = page(
            "https://a.example/post?utm=1",
            r#"<html><head><meta property="og:url" content="https://a.example/clean"></head></html>"#,
        );
        assert_eq!(og.canonical_url, "https://a.example/clean");
    }

    #[test]
    fn junk_urls_are_refused() {
        for junk in [
            "null",
            "undefined",
            "  ",
            "javascript:alert(1)",
            "mailto:a@b",
        ] {
            let d = page(
                "https://a.example/",
                &format!(
                    r#"<html><head><meta property="og:image" content="{junk}"></head></html>"#
                ),
            );
            assert_eq!(d.image, None, "{junk}");
        }
    }

    #[test]
    fn languages_are_mastodons_or_nothing() {
        assert_eq!(valid_locale_or_nil(Some("ja_JP")).as_deref(), Some("ja"));
        assert_eq!(valid_locale_or_nil(Some("ko")).as_deref(), Some("ko"));
        assert_eq!(valid_locale_or_nil(Some("xx-YY")), None);
        assert_eq!(valid_locale_or_nil(Some("")), None);
    }

    #[test]
    fn entities_decode_like_htmlentities() {
        assert_eq!(decode_entities("a &amp; b &lt;c&gt;"), "a & b <c>");
        assert_eq!(decode_entities("&#233;&#xe9;&eacute;"), "ééé");
        assert_eq!(
            decode_entities("&amp without semicolon"),
            "&amp without semicolon"
        );
        assert_eq!(decode_entities("&nosuchthing;"), "&nosuchthing;");
    }

    #[test]
    fn charsets() {
        let (sjis, _, _) =
            encoding_rs::SHIFT_JIS.encode("<html><head><title>日本語</title></head></html>");
        assert_eq!(
            decode_html(&sjis, Some("Shift_JIS")),
            "<html><head><title>日本語</title></head></html>"
        );
        let meta = [
            b"<html><head><meta charset=\"euc-kr\"><title>".as_slice(),
            &encoding_rs::EUC_KR.encode("한국어").0,
            b"</title></head></html>",
        ]
        .concat();
        assert!(decode_html(&meta, None).contains("한국어"));
        // A body cut off in the middle of a character is still UTF-8.
        let cut = "제목".as_bytes();
        assert_eq!(
            decode_html(&cut[..cut.len() - 1], Some("iso-8859-1")),
            "제\u{FFFD}"
        );
    }

    #[test]
    fn to_i_reads_leading_digits() {
        assert_eq!(ruby_to_i("640"), 640);
        assert_eq!(ruby_to_i("360px"), 360);
        assert_eq!(ruby_to_i("abc"), 0);
        assert_eq!(ruby_to_i("100%"), 100);
    }
}
