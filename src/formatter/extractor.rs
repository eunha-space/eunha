//! Mastodon's `Extractor`: twitter-text's entity extraction as Mastodon
//! amends it, in *config/initializers/twitter_regex.rb* and
//! *app/lib/extractor.rb*. Offsets are byte offsets into the text where
//! twitter-text counts codepoints; the entities they delimit are the same.
//! A URL's top-level domain is checked against twitter-text 3.1.0's lists
//! ([`super::tlds`]), so one under a newer domain stays text, as upstream.

use std::sync::LazyLock;

use fancy_regex::Regex;

/// `[[:word:]]` as Onigmo reads it on a Unicode string.
const WORD: &str = r"\p{Alphabetic}\p{M}\p{Nd}\p{Pc}";
/// `Tag::HASHTAG_SEPARATORS`.
const HASHTAG_SEPARATORS: &str = r"_\x{00B7}\x{30FB}\x{200C}";
/// `Extractor::MAX_DOMAIN_LENGTH`.
const MAX_DOMAIN_LENGTH: usize = 253;
/// twitter-text's `MAX_URL_LENGTH`.
const MAX_URL_LENGTH: usize = 4096;
/// twitter-text's `MAX_TCO_SLUG_LENGTH`.
const MAX_TCO_SLUG_LENGTH: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Url(String),
    Hashtag(String),
    /// `screen_name`: `user` or `user@domain`, as written.
    Mention(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
}

/// `Account::MENTION_RE`.
pub static MENTION_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?<![=/{WORD}])@((?i:[a-z0-9_]+(?:[.-]+[a-z0-9_]+)*)(?:@[{WORD}]+(?:[.-]+[{WORD}]+)*)?)"
    ))
    .expect("valid mention pattern")
});

/// `Tag::HASHTAG_RE`.
pub static HASHTAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    let first = format!(
        "[{WORD}_][{WORD}{HASHTAG_SEPARATORS}]*[\\p{{Alphabetic}}{HASHTAG_SEPARATORS}][{WORD}{HASHTAG_SEPARATORS}]*[{WORD}_]"
    );
    let last = format!("[{WORD}_]*[\\p{{Alphabetic}}][{WORD}_]*");
    Regex::new(&format!(r"(?<!\S)[#＃](({first})|({last}))")).expect("valid hashtag pattern")
});

/// twitter-text's `end_mention_match`.
static END_MENTION_MATCH: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"\A(?:[@＠]|[\x{C0}-\x{D6}\x{D8}-\x{F6}\x{F8}-\x{FF}\x{100}-\x{24F}\x{253}-\x{254}\x{256}-\x{257}\x{259}\x{25B}\x{263}\x{268}\x{26F}\x{272}\x{289}\x{28B}\x{2BB}\x{300}-\x{36F}\x{1E00}-\x{1EFF}]|://)",
    )
    .expect("valid pattern")
});

/// twitter-text's `valid_gTLD | valid_ccTLD | valid_punycode`: a top-level
/// domain on one of its lists, not followed by a letter, digit, `@`, `+` or
/// `-`, or a punycode one. Read case-insensitively, as twitter-text's
/// expressions are.
pub(crate) fn tld_pattern() -> String {
    let generic = super::tlds::GENERIC.join("|");
    let country = super::tlds::COUNTRY.join("|");
    format!(
        r"(?:(?:(?:{generic})(?=[^0-9a-z@+\-]|$))|(?:(?:{country})(?=[^0-9a-z@+\-]|$))|(?:xn--[0-9a-z]+))"
    )
}

/// The pieces twitter-text builds its URL expressions from.
struct UrlParts {
    before: String,
    domain: String,
    path: String,
    query: String,
    query_ending: String,
}

fn url_parts() -> UrlParts {
    // `valid_url_preceding_chars`.
    let before = r"(?:[^A-Z0-9@＠$#＃\x{FFFE}\x{FEFF}\x{FFFF}]|[\x{061C}\x{200E}\x{200F}\x{202A}-\x{202E}\x{2066}-\x{2069}]|^)".to_owned();
    // `DOMAIN_VALID_CHARS`.
    let dvc = r"[^\x00-\x2F\x3A-\x40\x5B-\x60\x7B-\x7F\x{85}\x{A0}\x{1680}\x{180E}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FFFE}\x{FEFF}\x{FFFF}]";
    let subdomain = format!(r"(?:(?:{dvc}(?:[_-]|{dvc})*)?{dvc}\.)");
    let domain_name = format!(r"(?:(?:{dvc}(?:-|{dvc})*)?{dvc}\.)");
    let tld = tld_pattern();
    let domain = format!("(?:{subdomain}*{domain_name}{tld})");
    let general = r"[^\s<>()?]";
    let balanced = format!(r"\((?:{general}+|(?:{general}*\({general}+\){general}*))\)");
    let ending = format!(r#"(?:[^\s()?!*"'「」<>;:=,.$%\[\]~&|]|{balanced})"#);
    let path = format!(r"(?:(?:{general}*(?:{balanced}{general}*)*{ending})|(?:{general}+/))");
    let uchars = r"\x{A0}-\x{D7FF}\x{F900}-\x{FDCF}\x{FDF0}-\x{FFEF}\x{10000}-\x{1FFFD}\x{20000}-\x{2FFFD}\x{30000}-\x{3FFFD}\x{40000}-\x{4FFFD}\x{50000}-\x{5FFFD}\x{60000}-\x{6FFFD}\x{70000}-\x{7FFFD}\x{80000}-\x{8FFFD}\x{90000}-\x{9FFFD}\x{A0000}-\x{AFFFD}\x{B0000}-\x{BFFFD}\x{C0000}-\x{CFFFD}\x{D0000}-\x{DFFFD}\x{E1000}-\x{EFFFD}\x{E000}-\x{F8FF}\x{F0000}-\x{FFFFD}\x{100000}-\x{10FFFD}";
    let query = format!(r"[a-z0-9!?*'();:&=+$/%#\[\]\-_.,~|@\^{uchars}]");
    let query_ending = format!(r"[a-z0-9_&=#/\-{uchars}]");
    UrlParts {
        before,
        domain,
        path,
        query,
        query_ending,
    }
}

/// `Twitter::TwitterText::Regex[:valid_url]`, as Mastodon redefines it.
static VALID_URL: LazyLock<Regex> = LazyLock::new(|| {
    let UrlParts {
        before,
        domain,
        path,
        query,
        query_ending,
    } = url_parts();
    Regex::new(&format!(
        r"(?im){before}(?P<url>(?P<protocol>(?:https?|dat|dweb|ipfs|ipns|ssb|gopher|gemini)://)?(?P<domain>{domain})(?::[0-9]+)?(?:/{path}*)?(?:\?{query}*{query_ending})?)"
    ))
    .expect("valid URL pattern")
});

/// `Twitter::TwitterText::Regex[:valid_extended_uri]`, Mastodon's addition.
static VALID_EXTENDED_URI: LazyLock<Regex> = LazyLock::new(|| {
    let UrlParts {
        before,
        domain,
        query,
        query_ending,
        ..
    } = url_parts();
    let unreserved = r"[a-z\p{Cyrillic}0-9\-._~]";
    let pct_encoded = r"(?:%[0-9a-f]{2})";
    let nodeid = format!(r"(?:{unreserved}|{pct_encoded}|[!$()*+,;=])");
    let resid = format!(r"(?:{unreserved}|{pct_encoded}|[!$&'()*+,;=])");
    Regex::new(&format!(
        r"(?im){before}(?P<url>(?:xmpp:(?://{nodeid}+@{domain}/)?(?:{nodeid}+@)?{domain}(?:/{resid}+)?(?:\?{query}*{query_ending})?)|(?:magnet:\?{query}*{query_ending}))"
    ))
    .expect("valid extended URI pattern")
});

/// twitter-text's `valid_tco_url`.
static VALID_TCO_URL: LazyLock<Regex> = LazyLock::new(|| {
    let UrlParts {
        query,
        query_ending,
        ..
    } = url_parts();
    Regex::new(&format!(
        r"(?i)^https?://t\.co/([a-z0-9]+)(?:\?{query}*{query_ending})?"
    ))
    .expect("valid t.co pattern")
});

/// Every match of `re` in `text`, the way Ruby's `String#scan` walks them.
fn scan<'t>(re: &Regex, text: &'t str) -> Vec<fancy_regex::Captures<'t>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos <= text.len() {
        let Ok(Some(captures)) = re.captures_from_pos(text, pos) else {
            break;
        };
        let whole = captures.get(0).expect("group 0");
        pos = if whole.end() > whole.start() {
            whole.end()
        } else {
            // An empty match; step over one character.
            match text[whole.end()..].chars().next() {
                Some(c) => whole.end() + c.len_utf8(),
                None => break,
            }
        };
        out.push(captures);
    }
    out
}

/// `Extractor.extract_entities_with_indices(text, extract_url_without_protocol: false)`.
pub fn extract_entities(text: &str) -> Vec<Entity> {
    let mut entities = extract_urls(text);
    entities.extend(extract_hashtags(text));
    entities.extend(extract_mentions(text));
    entities.extend(extract_extra_uris(text));
    remove_overlapping_entities(entities)
}

/// twitter-text's `remove_overlapping_entities`.
pub fn remove_overlapping_entities(mut entities: Vec<Entity>) -> Vec<Entity> {
    entities.sort_by_key(|e| e.start);
    let mut kept: Vec<Entity> = Vec::with_capacity(entities.len());
    for entity in entities {
        if kept.last().is_some_and(|prev| prev.end > entity.start) {
            continue;
        }
        kept.push(entity);
    }
    kept
}

/// `extract_urls_with_indices(text, extract_url_without_protocol: false)`.
pub fn extract_urls(text: &str) -> Vec<Entity> {
    if !text.contains(':') {
        return Vec::new();
    }
    let mut urls = Vec::new();
    for captures in scan(&VALID_URL, text) {
        let url = captures.name("url").expect("url group");
        // Without a protocol, nothing is extracted, but the match is still
        // consumed.
        if captures.name("protocol").is_none() {
            continue;
        }
        let domain = captures.name("domain").expect("domain group").as_str();
        let mut url_text = url.as_str();
        let mut end = url.end();
        if let Ok(Some(tco)) = VALID_TCO_URL.captures(url_text) {
            if tco
                .get(1)
                .is_some_and(|slug| slug.as_str().len() > MAX_TCO_SLUG_LENGTH)
            {
                continue;
            }
            let whole = tco.get(0).expect("group 0");
            url_text = &url_text[..whole.end()];
            end = url.start() + whole.end();
        }
        if !is_valid_domain(url_text.chars().count(), domain) {
            continue;
        }
        urls.push(Entity {
            start: url.start(),
            end,
            kind: Kind::Url(url_text.to_owned()),
        });
    }
    urls
}

/// twitter-text's `is_valid_domain`, for a URL that has a protocol:
/// `IDN::Idna.toASCII` has to accept the domain, and the URL, with the domain
/// in that form, must be no longer than `MAX_URL_LENGTH`.
fn is_valid_domain(url_length: usize, domain: &str) -> bool {
    let Ok(encoded) = idna::domain_to_ascii(domain) else {
        return false;
    };
    // libidn refuses a label longer than 63 octets.
    if encoded.split('.').any(|label| label.len() > 63) {
        return false;
    }
    let original = domain.chars().count();
    let updated = encoded.chars().count();
    let length = url_length + updated.saturating_sub(original);
    length <= MAX_URL_LENGTH
}

/// `Extractor.extract_hashtags_with_indices`.
pub fn extract_hashtags(text: &str) -> Vec<Entity> {
    if !text.contains(['#', '＃']) {
        return Vec::new();
    }
    static PROTOCOL_SUFFIX: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\A(.+)(https?)\z").expect("valid pattern"));
    let mut tags = Vec::new();
    for captures in scan(&HASHTAG_RE, text) {
        let whole = captures.get(0).expect("group 0");
        let name = captures.get(1).expect("name group");
        let mut hashtag = name.as_str();
        let mut end = name.end();
        if text[whole.end()..].starts_with("://") {
            if let Some(c) = PROTOCOL_SUFFIX.captures(hashtag) {
                let suffix = c.get(2).expect("group 2").as_str().len();
                hashtag = c.get(1).expect("group 1").as_str();
                end -= suffix;
            }
        }
        tags.push(Entity {
            start: whole.start(),
            end,
            kind: Kind::Hashtag(hashtag.to_owned()),
        });
    }
    tags
}

/// `Extractor.extract_mentions_or_lists_with_indices`.
pub fn extract_mentions(text: &str) -> Vec<Entity> {
    if !text.contains(['@', '＠']) {
        return Vec::new();
    }
    let mut mentions = Vec::new();
    for captures in scan(&MENTION_RE, text) {
        let whole = captures.get(0).expect("group 0");
        let screen_name = captures.get(1).expect("screen name group");
        if END_MENTION_MATCH.is_match(&text[whole.end()..]) {
            continue;
        }
        if let Some((_, domain)) = screen_name.as_str().split_once('@') {
            if domain.chars().count() > MAX_DOMAIN_LENGTH {
                continue;
            }
        }
        mentions.push(Entity {
            start: screen_name.start() - 1,
            end: screen_name.end(),
            kind: Kind::Mention(screen_name.as_str().to_owned()),
        });
    }
    mentions
}

/// `Extractor.extract_extra_uris_with_indices`: `xmpp:` and `magnet:` URIs.
pub fn extract_extra_uris(text: &str) -> Vec<Entity> {
    if !text.contains(':') {
        return Vec::new();
    }
    scan(&VALID_EXTENDED_URI, text)
        .into_iter()
        .map(|captures| {
            let url = captures.name("url").expect("url group");
            Entity {
                start: url.start(),
                end: url.end(),
                kind: Kind::Url(url.as_str().to_owned()),
            }
        })
        .collect()
}

/// `text.scan(Account::MENTION_RE)` as `ProcessMentionsService` and
/// `Account.from_text` read it: each mention's username and domain.
pub fn mention_handles(text: &str) -> Vec<(String, Option<String>)> {
    scan(&MENTION_RE, text)
        .into_iter()
        .map(|c| {
            let screen_name = c.get(1).expect("screen name group").as_str();
            match screen_name.split_once('@') {
                Some((username, domain)) => (username.to_owned(), Some(domain.to_owned())),
                None => (screen_name.to_owned(), None),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(text: &str) -> Vec<(String, usize, usize)> {
        extract_hashtags(text)
            .into_iter()
            .map(|e| match e.kind {
                Kind::Hashtag(h) => (h, e.start, e.end),
                _ => unreachable!(),
            })
            .collect()
    }

    // spec/lib/extractor_spec.rb
    #[test]
    fn mentions_need_an_at_sign() {
        assert!(extract_mentions("a string without at signs").is_empty());
    }

    #[test]
    fn mentions_ending_in_particular_characters_are_skipped() {
        assert!(extract_mentions("@screen_name@").is_empty());
    }

    #[test]
    fn mentions_come_with_indices() {
        assert_eq!(
            extract_mentions("@screen_name"),
            vec![Entity {
                start: 0,
                end: 12,
                kind: Kind::Mention("screen_name".into())
            }]
        );
    }

    #[test]
    fn hashtags_need_a_hash_sign() {
        assert!(extract_hashtags("a string without hash sign").is_empty());
    }

    #[test]
    fn hashtags_after_an_ascii_hash() {
        assert_eq!(tags("hello #world"), vec![("world".into(), 6, 12)]);
    }

    #[test]
    fn hashtags_after_a_full_width_hash() {
        // Byte offsets: the full-width hash is three bytes.
        assert_eq!(tags("hello ＃world"), vec![("world".into(), 6, 14)]);
    }

    #[test]
    fn hashtag_text_before_a_scheme_separator() {
        assert_eq!(tags("#hashtag://"), vec![("hashtag".into(), 0, 8)]);
        assert_eq!(tags("#hashtaghttp://"), vec![("hashtag".into(), 0, 8)]);
        assert_eq!(tags("#hashtaghttps://"), vec![("hashtag".into(), 0, 8)]);
    }

    #[test]
    fn cashtags_are_not_entities() {
        assert!(extract_entities("$cashtag").is_empty());
    }

    #[test]
    fn hashtags_need_whitespace_before_them() {
        assert!(tags("a#b").is_empty());
        assert_eq!(tags("#a_b c"), vec![("a_b".into(), 0, 4)]);
        assert_eq!(tags("#hashtagタグ"), vec![("hashtagタグ".into(), 0, 14)]);
        // Only digits is not a hashtag.
        assert!(tags("#123").is_empty());
    }

    #[test]
    fn mentions_follow_account_mention_re() {
        let names: Vec<String> = extract_mentions("hi @alice@example.com. @bob.smith x=@no a/@no")
            .into_iter()
            .map(|e| match e.kind {
                Kind::Mention(m) => m,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(names, vec!["alice@example.com", "bob.smith"]);
    }

    #[test]
    fn urls_need_a_protocol() {
        let urls: Vec<String> = extract_entities("see example.com and https://example.com/a.")
            .into_iter()
            .map(|e| match e.kind {
                Kind::Url(u) => u,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(urls, vec!["https://example.com/a"]);
    }

    #[test]
    fn a_top_level_domain_is_not_followed_by_a_digit() {
        assert!(extract_urls("https://example.com1").is_empty());
        assert_eq!(extract_urls("https://example.com-x.org").len(), 1);
    }

    /// twitter-text 3.1.0's lists, as `Twitter::TwitterText::Regex` joins
    /// them: a domain under a top-level domain it does not know is no link.
    #[test]
    fn top_level_domains_are_twitter_texts() {
        for linked in [
            "https://eunha.social/docs",
            "https://EXAMPLE.COM",
            "https://example.한국",
            "https://example.xn--3e0b707e",
        ] {
            assert_eq!(extract_urls(linked).len(), 1, "{linked}");
        }
        for text in ["https://example.zzz", "https://example.notatld/path"] {
            assert!(extract_urls(text).is_empty(), "{text}");
        }
    }
}
