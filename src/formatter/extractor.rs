//! Mastodon's `Extractor`, *app/lib/extractor.rb*. URLs are found as UTS #58
//! says, by the uts58 gem Mastodon uses: this file is a port of its
//! `Uts58::Extractor`, for links with a scheme. A host counts when the Public
//! Suffix List knows its top-level domain. Offsets are byte offsets into the
//! text where Ruby counts codepoints; the entities they delimit are the same.
//!
//! Mastodon: app/lib/extractor.rb

use std::sync::LazyLock;

use fancy_regex::Regex;

use super::uts58_tables::{LinkTerm, LINK_TERM, OPENERS};

/// `[[:word:]]` as Onigmo reads it on a Unicode string.
const WORD: &str = r"\p{Alphabetic}\p{M}\p{Nd}\p{Pc}";
/// `Tag::HASHTAG_SEPARATORS`.
const HASHTAG_SEPARATORS: &str = r"_\x{00B7}\x{30FB}\x{200C}";
/// `Extractor::MAX_DOMAIN_LENGTH`.
const MAX_DOMAIN_LENGTH: usize = 253;
/// `Extractor::UTS58`'s `max_length`, in codepoints of the text.
const MAX_URL_LENGTH: usize = 4096;
/// `Account::HOST_LABEL_CHAR`, inside its brackets: `[[:word:]]` and the
/// letters IDNA allows that it leaves out.
const HOST_LABEL_CHAR: &str = "\\p{Alphabetic}\\p{M}\\p{Nd}\\p{Pc}ßς۽۾་〇";

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
        r"(?<![=/{WORD}])@((?i:[a-z0-9_]+(?:[.-]+[a-z0-9_]+)*)(?:@[{HOST_LABEL_CHAR}]+(?:[.-]+[{HOST_LABEL_CHAR}]+)*)?)"
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

/// `Tag::HASHTAG_NAME_RE`: a whole name as a hashtag may have it.
pub static HASHTAG_NAME_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    let first = format!(
        "[{WORD}_][{WORD}{HASHTAG_SEPARATORS}]*[\\p{{Alphabetic}}{HASHTAG_SEPARATORS}][{WORD}{HASHTAG_SEPARATORS}]*[{WORD}_]"
    );
    let last = format!("[{WORD}_]*[\\p{{Alphabetic}}][{WORD}_]*");
    regex::Regex::new(&format!(r"(?i)\A(?:(?:{first})|(?:{last}))\z"))
        .expect("valid hashtag name pattern")
});

/// `Extractor::END_MENTION_MATCH`.
static END_MENTION_MATCH: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"\A(?:[@＠]|[\x{C0}-\x{D6}\x{D8}-\x{F6}\x{F8}-\x{FF}\x{100}-\x{24F}\x{253}-\x{254}\x{256}-\x{257}\x{259}\x{25B}\x{263}\x{268}\x{26F}\x{272}\x{289}\x{28B}\x{2BB}\x{300}-\x{36F}\x{1E00}-\x{1EFF}]|://)",
    )
    .expect("valid pattern")
});

/// Where a link may start: `Uts58::Extractor`'s scan. A UTS #58 link has no
/// userinfo, so a host right after an `@` is no link.
static LINK_TRIGGER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?<![-\p{L}\p{M}\p{Nd}./@])(?=[\p{L}\p{M}\p{Nd}][-\p{L}\p{N}\p{M}ßς۽۾་〇]*[.:。])")
        .expect("valid pattern")
});

/// The scheme, after any letters of the scripts written without spaces:
/// `テストhttp://example.com` is a link from its `h`.
static LINK_SCHEME: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)\A([\p{Han}\p{Hiragana}\p{Katakana}\p{Hangul}\p{Thai}\p{Lao}\p{Khmer}\p{Myanmar}]*?)(https?://)",
    )
    .expect("valid pattern")
});

/// What might be a hostname or an IDN.
static LINK_HOST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"\A(?:[-\p{L}\p{N}\p{M}ßς۽۾་〇]+[.。]){1,4}[-\p{L}\p{N}\p{M}]+(?![-\p{L}\p{N}\p{M}])",
    )
    .expect("valid pattern")
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
    entities.extend(extract_xmpp_uris(text));
    entities.extend(extract_magnet_uris(text));
    // A `<` is reserved by RFC 3986, and has been used for XSS.
    entities.retain(|entity| !matches!(&entity.kind, Kind::Url(url) if url.contains('<')));
    remove_overlapping_entities(entities)
}

/// `Uts58::Extractor#remove_overlapping_entities`.
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

/// `Uts58::Extractor#extract_urls_with_indices(text,
/// extract_url_without_protocol: false)`, with `max_length` set.
pub fn extract_urls(text: &str) -> Vec<Entity> {
    if !text.contains(':') {
        return Vec::new();
    }
    let mut urls = Vec::new();
    for trigger in scan(&LINK_TRIGGER, text) {
        let trigger = trigger.get(0).expect("group 0").start();
        if let Some(url) = link_at(text, trigger) {
            urls.push(url);
        }
    }
    urls
}

/// The link starting where the trigger fired, if there is one.
fn link_at(text: &str, trigger: usize) -> Option<Entity> {
    let after = &text[trigger..];
    let scheme = LINK_SCHEME.captures(after)?;
    let start = trigger + scheme.get(1)?.end();
    let protocol = scheme.get(2)?.as_str();
    let from_host = &after[scheme.get(0)?.end()..];

    let host = LINK_HOST.find(from_host).ok()??;
    if host.as_str().chars().count() >= 254 {
        return None;
    }
    let host_raw = host.as_str().replace('。', ".");
    // A label may not start or end with a hyphen.
    if host_raw
        .split('.')
        .any(|label| label.starts_with('-') || label.ends_with('-'))
    {
        return None;
    }
    let (hostname, result) = idna::domain_to_unicode(&host_raw);
    result.ok()?;
    if !public_suffix_allows(&hostname) {
        return None;
    }

    let after_host = &from_host[host.end()..];
    let mut rest = after_host;
    // "example.com." keeps its trailing dot only when a path, query or
    // fragment follows; at the end of a sentence it is prose.
    if let Some(dotless) = rest.strip_prefix('.') {
        if dotless.starts_with(['/', '?', '#']) {
            rest = dotless;
        }
    }
    if let Some(digits) = rest.strip_prefix(':') {
        let length = digits.bytes().take_while(u8::is_ascii_digit).count();
        if length > 0 {
            // A port is 1..65535.
            let port = digits[..length].parse::<u32>().ok()?;
            if !(1..=65535).contains(&port) {
                return None;
            }
            rest = &digits[length..];
        }
    }
    while rest.starts_with('/') {
        rest = skip_component(rest, &['#', '/', '?'], &[], None);
    }
    if rest.starts_with('?') {
        rest = skip_component(rest, &['#'], &['=', '&'], None);
    }
    if rest.starts_with('#') {
        rest = skip_component(rest, &[], &[], Some(&[',', '=', '&']));
    }

    let end = text.len() - rest.len();
    if text[start..end].chars().count() > MAX_URL_LENGTH {
        return None;
    }
    let tail = &after_host[..after_host.len() - rest.len()];
    Some(Entity {
        start,
        end,
        kind: Kind::Url(format!("{protocol}{hostname}{tail}")),
    })
}

/// `PublicSuffix.parse(hostname, ignore_private: true, default_rule: nil)`:
/// the top-level domain has to be on the list, and there has to be something
/// in front of the suffix. A private entry counts as the ICANN one above it,
/// so a name that is only a private suffix is an ordinary domain.
fn public_suffix_allows(hostname: &str) -> bool {
    let Ok(ascii) = idna::domain_to_ascii(hostname) else {
        return false;
    };
    let Some(suffix) = psl::suffix(ascii.as_bytes()) else {
        return false;
    };
    if !suffix.is_known() {
        return false;
    }
    suffix.typ() == Some(psl::Type::Private) || suffix.as_bytes().len() < ascii.len()
}

/// `Link_Term` for a code point; `None` is `Hard`, UTS #58's default.
fn link_term(c: char) -> Option<LinkTerm> {
    let c = u32::from(c);
    let index = LINK_TERM
        .binary_search_by(|&(start, end, _)| {
            if c < start {
                std::cmp::Ordering::Greater
            } else if c > end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .ok()?;
    Some(LINK_TERM[index].2)
}

fn opener_of(closer: char) -> Option<u32> {
    let closer = u32::from(closer);
    let index = OPENERS.binary_search_by_key(&closer, |&(c, _)| c).ok()?;
    Some(OPENERS[index].1)
}

/// `Uts58::Extractor#skip_component`: what is left after the path segment,
/// query or fragment `string` starts with. Brackets are paired per part, which
/// `separators` end; a `:~:` starts a fragment directive, with its own.
fn skip_component<'t>(
    string: &'t str,
    closers: &[char],
    separators: &[char],
    directive_separators: Option<&[char]>,
) -> &'t str {
    let chars: Vec<(usize, char)> = string.char_indices().collect();
    let mut openers: Vec<u32> = Vec::new();
    let mut separators = separators;
    // The first character is the lead-in.
    let mut i = 1;
    while i < chars.len() {
        let (at, c) = chars[i];
        if closers.contains(&c) {
            return &string[at..];
        }
        if let Some(directive) = directive_separators {
            if c == ':'
                && chars.get(i + 1).map(|&(_, c)| c) == Some('~')
                && chars.get(i + 2).map(|&(_, c)| c) == Some(':')
            {
                openers.clear();
                separators = directive;
                i += 3;
                continue;
            }
        }
        if separators.contains(&c) {
            openers.clear();
            i += 1;
            continue;
        }
        match link_term(c) {
            Some(LinkTerm::Include) => {}
            Some(LinkTerm::Soft) => {
                // A soft terminator ends the link only before a hard one.
                let next = chars[i..]
                    .iter()
                    .map(|&(_, c)| link_term(c))
                    .find(|term| *term != Some(LinkTerm::Soft));
                if matches!(next, None | Some(None)) {
                    return &string[at..];
                }
            }
            Some(LinkTerm::Close) => {
                if opener_of(c).is_some() && opener_of(c) == openers.last().copied() {
                    openers.pop();
                } else {
                    return &string[at..];
                }
            }
            Some(LinkTerm::Open) => openers.push(u32::from(c)),
            None => return &string[at..],
        }
        i += 1;
    }
    ""
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

/// `Extractor::TRAILING_URI_PUNCT`.
const TRAILING_URI_PUNCT: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '}', '\'', '"', '`'];

/// A URI of a scheme found by a `pattern` that takes anything up to
/// whitespace, less trailing punctuation, when `keep` accepts it.
fn liberal_uris(text: &str, pattern: &Regex, keep: impl Fn(&str) -> bool) -> Vec<Entity> {
    scan(pattern, text)
        .into_iter()
        .filter_map(|captures| {
            let found = captures.get(0)?;
            let url = found.as_str().trim_end_matches(TRAILING_URI_PUNCT);
            keep(url).then(|| Entity {
                start: found.start(),
                end: found.start() + url.len(),
                kind: Kind::Url(url.to_owned()),
            })
        })
        .collect()
}

/// `Extractor.extract_xmpp_uris_with_indices`: an `@` or `.` has to follow
/// the scheme. Ruby's `\S` is everything but ASCII whitespace.
pub fn extract_xmpp_uris(text: &str) -> Vec<Entity> {
    static XMPP: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?<![\p{L}\p{N}_])xmpp:[^ \t\r\n\x0C\x0B]+").expect("valid pattern")
    });
    if !text.contains("xmpp:") {
        return Vec::new();
    }
    liberal_uris(text, &XMPP, |url| {
        url["xmpp:".len().min(url.len())..].contains(['.', '@'])
    })
}

/// `Extractor.extract_magnet_uris_with_indices`: a magnet URI is meaningless
/// without its `xt=`.
pub fn extract_magnet_uris(text: &str) -> Vec<Entity> {
    static MAGNET: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?<![\p{L}\p{N}_])magnet:\?[^ \t\r\n\x0C\x0B]+").expect("valid pattern")
    });
    if !text.contains("magnet:?") {
        return Vec::new();
    }
    liberal_uris(text, &MAGNET, |url| {
        url.contains("?xt=") || url.contains("&xt=")
    })
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

    /// The Public Suffix List decides, in either form of a name.
    #[test]
    fn top_level_domains_are_the_public_suffix_lists() {
        for linked in [
            "https://eunha.social/docs",
            "https://EXAMPLE.COM",
            "https://example.한국",
            "https://example.xn--3e0b707e",
            "https://универсальное-принятие-тест.москва",
            "https://どこでもつかえる.みんな",
        ] {
            assert_eq!(extract_urls(linked).len(), 1, "{linked}");
        }
        for text in [
            "https://example.zzz",
            "https://example.notatld/path",
            "https://co.uk",
        ] {
            assert!(extract_urls(text).is_empty(), "{text}");
        }
    }

    fn urls(text: &str) -> Vec<(String, usize, usize)> {
        extract_entities(text)
            .into_iter()
            .filter_map(|e| match e.kind {
                Kind::Url(u) => Some((u, e.start, e.end)),
                _ => None,
            })
            .collect()
    }

    fn url_of(text: &str) -> String {
        urls(text)
            .into_iter()
            .next()
            .map_or(String::new(), |(u, _, _)| u)
    }

    /// `spec/lib/extractor_spec.rb` and `spec/lib/text_formatter_spec.rb`.
    #[test]
    fn urls_end_where_uts58_says() {
        assert_eq!(
            url_of("https://example.com/foo= Content"),
            "https://example.com/foo="
        );
        assert_eq!(
            url_of("https://example.com/foo."),
            "https://example.com/foo"
        );
        assert_eq!(
            url_of("(https://example.com/a(b))"),
            "https://example.com/a(b)"
        );
        assert_eq!(url_of("https://example.com."), "https://example.com");
        assert_eq!(url_of("https://example.com./a"), "https://example.com./a");
        assert_eq!(
            url_of("https://example.com:8443/a"),
            "https://example.com:8443/a"
        );
        assert_eq!(
            url_of("https://example.com/a?b=c&d=(e)#f"),
            "https://example.com/a?b=c&d=(e)#f"
        );
        assert_eq!(url_of("テストhttps://example.com"), "https://example.com");
        assert_eq!(urls("テストhttps://example.com")[0].1, "テスト".len(),);
        assert_eq!(
            url_of("https://xn--3e0b707e.xn--3e0b707e"),
            "https://한국.한국"
        );
        assert_eq!(url_of("https://example.com:0"), "");
        assert_eq!(url_of("https://example.com:65536"), "");
        assert_eq!(url_of("https://-foo.example.com"), "");
    }

    #[test]
    fn only_http_and_https_are_links() {
        assert_eq!(url_of("gemini://example.com"), "");
        assert_eq!(url_of("dat://example.com"), "");
        assert_eq!(url_of("ipfs://example.com"), "");
    }

    #[test]
    fn a_url_is_kept_to_its_length() {
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LENGTH));
        assert_eq!(url_of(&long), "");
    }

    #[test]
    fn a_url_with_an_angle_bracket_is_no_link() {
        assert!(!url_of("https://example.com/<del>a").contains('<'));
        assert_eq!(url_of("xmpp:a@example.com<script>"), "");
    }

    #[test]
    fn a_mention_is_not_taken_for_a_url() {
        assert!(urls("@bob@example.com").is_empty());
        assert_eq!(
            url_of("https://example.com/@bob@example.org"),
            "https://example.com/@bob@example.org"
        );
    }

    #[test]
    fn a_mention_host_may_have_idna_letters() {
        assert_eq!(
            mention_handles("@alice@fußball.example"),
            vec![("alice".to_owned(), Some("fußball.example".to_owned()))]
        );
        assert_eq!(
            mention_handles("@alice@ས་གནས.example"),
            vec![("alice".to_owned(), Some("ས་གནས.example".to_owned()))]
        );
    }

    #[test]
    fn xmpp_uris() {
        assert!(extract_xmpp_uris("no xmpp here").is_empty());
        for (text, url) in [
            ("xmpp:user@example.com", "xmpp:user@example.com"),
            ("xmpp:grå@grå.org/grønn/blå", "xmpp:grå@grå.org/grønn/blå"),
            (
                "please join xmpp:muc@example.com?join right now",
                "xmpp:muc@example.com?join",
            ),
            ("see xmpp:user@example.com.", "xmpp:user@example.com"),
            ("(xmpp:user@example.com),", "xmpp:user@example.com"),
        ] {
            assert_eq!(url_of(text), url, "{text}");
        }
        for text in [
            "xmpp:foo bar",
            "plain xmpp: nothing here",
            "fooxmpp:user@example.com",
        ] {
            assert!(extract_xmpp_uris(text).is_empty(), "{text}");
        }
    }

    #[test]
    fn magnet_uris() {
        assert!(extract_magnet_uris("no magnet here").is_empty());
        assert_eq!(url_of("magnet:?xt=urn:btih:abc"), "magnet:?xt=urn:btih:abc");
        assert_eq!(
            url_of("see magnet:?dn=a&xt=urn:btih:abc."),
            "magnet:?dn=a&xt=urn:btih:abc"
        );
        for text in [
            "magnet:?as=http%3A%2F%2Fexample.com%2Ffile",
            "plain magnet: nothing here",
        ] {
            assert!(extract_magnet_uris(text).is_empty(), "{text}");
        }
    }
}
