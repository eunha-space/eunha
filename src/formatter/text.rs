//! Mastodon's `TextFormatter` (*app/lib/text_formatter.rb*): plain text from
//! a local account turned into HTML, its URLs, hashtags and mentions linked.

use std::sync::LazyLock;

use super::extractor::{self, Kind};
use super::html::html_escape as h;

/// `TextFormatter::URL_PREFIX_REGEX`.
static URL_PREFIX_REGEX: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"\A(?:https?://(?:www\.)?|xmpp:)").expect("valid pattern"));

/// An account a mention can link to: what `TextFormatter.link_to_mention`
/// reads off one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionTarget {
    pub username: String,
    /// `None` for a local account.
    pub domain: Option<String>,
    /// `ActivityPub::TagManager#url_for`: `/@username` on this instance for a
    /// local account, the account's `url` for a remote one.
    pub url: String,
}

impl MentionTarget {
    /// `Account#pretty_acct`.
    fn pretty_acct(&self) -> String {
        match &self.domain {
            None => self.username.clone(),
            Some(domain) => {
                let (unicode, _) = idna::domain_to_unicode(domain);
                format!("{}@{unicode}", self.username)
            }
        }
    }
}

/// `TextFormatter`'s options.
#[derive(Debug, Clone)]
pub struct Options<'a> {
    /// `Rails.configuration.x.local_domain`, which hashtag links and
    /// `TagManager#local_domain?` read.
    pub local_domain: &'a str,
    pub multiline: bool,
    pub with_domains: bool,
    pub with_rel_me: bool,
    /// `preloaded_accounts`: the status's author and the accounts it
    /// mentions. When there are none, mentions are looked up as
    /// `EntityCache#mention` would, among [`Options::lookup`].
    pub preloaded_accounts: &'a [MentionTarget],
    /// The accounts `EntityCache#mention` would find for the text's mentions,
    /// fetched beforehand (see [`mention_lookups`]).
    pub lookup: &'a [MentionTarget],
    /// `quoted_status`'s `url_for || uri_for`.
    pub quoted_status_url: Option<&'a str>,
}

impl<'a> Options<'a> {
    /// `TextFormatter::DEFAULT_OPTIONS`.
    pub fn new(local_domain: &'a str) -> Self {
        Self {
            local_domain,
            multiline: true,
            with_domains: false,
            with_rel_me: false,
            preloaded_accounts: &[],
            lookup: &[],
            quoted_status_url: None,
        }
    }
}

/// Rails' `String#blank?`.
pub(crate) fn blank(text: &str) -> bool {
    text.chars().all(char::is_whitespace)
}

/// `TextFormatter.new(text, options).to_s`.
pub fn format(text: &str, options: &Options<'_>) -> String {
    if blank(text) {
        return add_quote_fallback(String::new(), options.quoted_status_url);
    }
    let html = rewrite(text, options);
    let html = if options.multiline {
        simple_format(&html).replace('\n', "")
    } else {
        html
    };
    add_quote_fallback(html, options.quoted_status_url)
}

/// `TextFormatter#rewrite`.
fn rewrite(text: &str, options: &Options<'_>) -> String {
    let mut result = String::with_capacity(text.len() * 2);
    let mut last = 0;
    for entity in extractor::extract_entities(text) {
        result.push_str(&h(&text[last..entity.start]));
        match &entity.kind {
            Kind::Url(url) => result.push_str(&shortened_link(url, options.with_rel_me)),
            Kind::Hashtag(hashtag) => result.push_str(&link_to_hashtag(hashtag, options)),
            Kind::Mention(screen_name) => result.push_str(&link_to_entity(screen_name, options)),
        }
        last = entity.end;
    }
    result.push_str(&h(&text[last..]));
    result
}

/// `TextFormatter.shortened_link`.
pub fn shortened_link(url: &str, rel_me: bool) -> String {
    let rel = if rel_me {
        "nofollow noopener me"
    } else {
        "nofollow noopener"
    };
    let prefix = URL_PREFIX_REGEX.find(url).map_or("", |m| m.as_str());
    let rest: Vec<char> = url[prefix.len()..].chars().collect();
    let mut display: String = rest.iter().take(30).collect();
    let mut suffix: Option<String> = (rest.len() >= 30).then(|| rest[30..].iter().collect());
    let mut cutoff = rest.len() > 30;
    if suffix.as_ref().is_some_and(|s| s.chars().count() == 1) {
        display.push_str(&suffix.take().unwrap_or_default());
        cutoff = false;
    }
    format!(
        r#"<a href="{}" target="_blank" rel="{rel}" translate="no"><span class="invisible">{}</span><span class="{}">{}</span><span class="invisible">{}</span></a>"#,
        h(url),
        h(prefix),
        if cutoff { "ellipsis" } else { "" },
        h(&display),
        h(suffix.as_deref().unwrap_or_default()),
    )
}

/// Rails' `escape_segment`, as `tag_url` writes a hashtag into its path.
fn escape_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `tag_url(hashtag)`.
pub fn tag_url(local_domain: &str, hashtag: &str) -> String {
    format!("https://{local_domain}/tags/{}", escape_segment(hashtag))
}

/// `TextFormatter#link_to_hashtag`.
fn link_to_hashtag(hashtag: &str, options: &Options<'_>) -> String {
    format!(
        r#"<a href="{}" class="mention hashtag" rel="tag">#<span>{}</span></a>"#,
        h(&tag_url(options.local_domain, hashtag)),
        h(hashtag),
    )
}

/// `TagManager#local_domain?`.
fn local_domain(domain: &str, options: &Options<'_>) -> bool {
    let domain = domain.trim();
    let domain = domain.strip_suffix('/').unwrap_or(domain);
    domain.eq_ignore_ascii_case(options.local_domain)
}

/// A mention's username and domain as `TextFormatter#link_to_mention`
/// splits them, the domain gone when it is this instance's.
fn split_screen_name<'s>(
    screen_name: &'s str,
    options: &Options<'_>,
) -> (&'s str, Option<&'s str>) {
    match screen_name.split_once('@') {
        Some((username, domain)) if !local_domain(domain, options) => (username, Some(domain)),
        Some((username, _)) => (username, None),
        None => (screen_name, None),
    }
}

/// The username and domain of every mention in `text` that `TextFormatter`
/// would look up with `EntityCache#mention`: fetch these accounts and pass
/// them as [`Options::lookup`].
pub fn mention_lookups(text: &str, local_domain: &str) -> Vec<(String, Option<String>)> {
    let options = Options::new(local_domain);
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    for entity in extractor::extract_entities(text) {
        if let Kind::Mention(screen_name) = entity.kind {
            let (username, domain) = split_screen_name(&screen_name, &options);
            let handle = (username.to_owned(), domain.map(str::to_owned));
            if !out.contains(&handle) {
                out.push(handle);
            }
        }
    }
    out
}

/// `TextFormatter#link_to_mention`.
fn link_to_entity(screen_name: &str, options: &Options<'_>) -> String {
    let (username, domain) = split_screen_name(screen_name, options);
    let same_domain = |other: &MentionTarget| match (&other.domain, domain) {
        (None, None) => true,
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    };
    let mut account = None;
    let mut same_username_hits = 0;
    if !options.preloaded_accounts.is_empty() {
        for other in options.preloaded_accounts {
            let same_username = other.username.eq_ignore_ascii_case(username);
            if same_username && !same_domain(other) {
                same_username_hits += 1;
            } else if same_username {
                account = Some(other);
            }
        }
    } else {
        // `Account.with_username(username).with_domain(domain)`.
        account = options.lookup.iter().find(|other| {
            other.username.to_lowercase() == username.to_lowercase()
                && match (&other.domain, domain) {
                    (None, None) => true,
                    (Some(a), Some(b)) => a.to_lowercase() == b.to_lowercase(),
                    _ => false,
                }
        });
    }
    match account {
        None => format!("@{}", h(screen_name)),
        Some(account) => link_to_mention(account, same_username_hits > 0 || options.with_domains),
    }
}

/// `TextFormatter.link_to_mention`.
pub fn link_to_mention(account: &MentionTarget, with_domain: bool) -> String {
    let display = if with_domain {
        account.pretty_acct()
    } else {
        account.username.clone()
    };
    format!(
        r#"<span class="h-card" translate="no"><a href="{}" class="u-url mention">@<span>{}</span></a></span>"#,
        h(&account.url),
        h(&display),
    )
}

/// Rails' `simple_format(html, {}, sanitize: false)`.
fn simple_format(text: &str) -> String {
    static PARAGRAPHS: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\n\n+").expect("valid pattern"));
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut paragraphs: Vec<&str> = PARAGRAPHS.split(&text).collect();
    while paragraphs.last().is_some_and(|p| p.is_empty()) {
        paragraphs.pop();
    }
    if paragraphs.is_empty() {
        return "<p></p>".to_owned();
    }
    paragraphs
        .iter()
        .map(|paragraph| {
            // `gsub(/([^\n]\n)(?=[^\n])/, '\1<br />')`.
            let chars: Vec<char> = paragraph.chars().collect();
            let mut out = String::with_capacity(paragraph.len() + 16);
            for (i, c) in chars.iter().enumerate() {
                out.push(*c);
                if *c == '\n'
                    && i > 0
                    && chars[i - 1] != '\n'
                    && chars.get(i + 1).is_some_and(|n| *n != '\n')
                {
                    out.push_str("<br />");
                }
            }
            format!("<p>{out}</p>")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Rails' `String#squish`.
fn squish(text: &str) -> String {
    let collapsed = text.split(char::is_whitespace).filter(|s| !s.is_empty());
    let mut out = String::with_capacity(text.len());
    for (i, part) in collapsed.enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(part);
    }
    out.trim_matches('\0').to_owned()
}

/// `TextFormatter#add_quote_fallback`.
pub fn add_quote_fallback(html: String, quoted_status_url: Option<&str>) -> String {
    let Some(url) = quoted_status_url else {
        return html;
    };
    if blank(url) || html.contains(url) {
        return html;
    }
    squish(&format!(
        r#"<p class="quote-inline">RE: {}</p>{html}"#,
        shortened_link(url, false)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOMAIN: &str = "cb6e6126.ngrok.io";

    fn to_s(text: &str) -> String {
        format(text, &Options::new(DOMAIN))
    }

    // spec/lib/text_formatter_spec.rb
    #[test]
    fn paragraphizes_the_text() {
        assert_eq!(to_s("text"), "<p>text</p>");
    }

    #[test]
    fn removes_line_feeds() {
        assert!(!to_s("line\nfeed").contains('\n'));
        assert_eq!(to_s("line\nfeed"), "<p>line<br />feed</p>");
        assert_eq!(to_s("a\n\n\nb\r\nc"), "<p>a</p><p>b<br />c</p>");
    }

    #[test]
    fn creates_a_mention_link() {
        let alice = [MentionTarget {
            username: "alice".into(),
            domain: None,
            url: format!("https://{DOMAIN}/@alice"),
        }];
        let options = Options {
            preloaded_accounts: &alice,
            ..Options::new(DOMAIN)
        };
        assert!(format("@alice", &options).contains(&format!(
            r#"<a href="https://{DOMAIN}/@alice" class="u-url mention">@<span>alice</span></a>"#
        )));
    }

    #[test]
    fn does_not_create_a_mention_link_for_unknown_accounts() {
        assert_eq!(to_s("@alice"), "<p>@alice</p>");
    }

    fn includes(text: &str, needle: &str) {
        let html = to_s(text);
        assert!(
            html.contains(needle),
            "{html:?} does not include {needle:?}"
        );
    }

    #[test]
    fn matches_full_urls() {
        includes(
            "https://hackernoon.com/the-power-to-build-communities-a-response-to-mark-zuckerberg-3f2cac9148a4",
            r#"href="https://hackernoon.com/the-power-to-build-communities-a-response-to-mark-zuckerberg-3f2cac9148a4""#,
        );
        includes("http://google.com", r#"href="http://google.com""#);
        includes("http://example.gay", r#"href="http://example.gay""#);
        includes("https://nic.みんな/", r#"href="https://nic.みんな/""#);
        includes(
            "https://nic.みんな/",
            r#"<span class="">nic.みんな/</span>"#,
        );
        includes(
            "http://www.mcmansionhell.com/post/156408871451/50-states-of-mcmansion-hell-scottsdale-arizona. ",
            r#"href="http://www.mcmansionhell.com/post/156408871451/50-states-of-mcmansion-hell-scottsdale-arizona""#,
        );
        includes("(http://google.com/)", r#"href="http://google.com/""#);
        includes("http://www.google.com!", r#"href="http://www.google.com""#);
        includes("http://www.google.com'", r#"href="http://www.google.com""#);
        includes("http://www.google.com>", r#"href="http://www.google.com""#);
    }

    #[test]
    fn matches_query_strings() {
        includes(
            "https://www.ruby-toolbox.com/search?utf8=%E2%9C%93&q=autolink",
            r#"href="https://www.ruby-toolbox.com/search?utf8=%E2%9C%93&amp;q=autolink""#,
        );
        includes(
            "https://www.ruby-toolbox.com/search?utf8=✓&q=autolink",
            r#"href="https://www.ruby-toolbox.com/search?utf8=✓&amp;q=autolink""#,
        );
        includes(
            "https://www.ruby-toolbox.com/search?utf8=✓",
            r#"href="https://www.ruby-toolbox.com/search?utf8=✓""#,
        );
        includes(
            "https://www.ruby-toolbox.com/search?utf8=%E2%9C%93&utf81=✓&q=autolink",
            r#"href="https://www.ruby-toolbox.com/search?utf8=%E2%9C%93&amp;utf81=✓&amp;q=autolink""#,
        );
    }

    #[test]
    fn matches_urls_in_context() {
        includes(
            "https://en.wikipedia.org/wiki/Diaspora_(software)",
            r#"href="https://en.wikipedia.org/wiki/Diaspora_(software)""#,
        );
        includes(
            r#""https://example.com/""#,
            r#"href="https://example.com/""#,
        );
        includes("<https://example.com/>", r#"href="https://example.com/""#);
        includes(
            "https://ja.wikipedia.org/wiki/日本",
            r#"href="https://ja.wikipedia.org/wiki/日本""#,
        );
        includes(
            "https://ko.wikipedia.org/wiki/대한민국",
            r#"href="https://ko.wikipedia.org/wiki/대한민국""#,
        );
        includes(
            "https://example.com/　abc123",
            r#"href="https://example.com/""#,
        );
        includes(
            "「[https://example.org/」",
            r#"href="https://example.org/""#,
        );
        includes(
            "https://baike.baidu.com/item/中华人民共和国",
            r#"href="https://baike.baidu.com/item/中华人民共和国""#,
        );
        includes(
            "https://zh.wikipedia.org/wiki/臺灣",
            r#"href="https://zh.wikipedia.org/wiki/臺灣""#,
        );
        includes(
            "https://gta.fandom.com/wiki/TW@ Content",
            r#"href="https://gta.fandom.com/wiki/TW@""#,
        );
    }

    #[test]
    fn escapes_unsafe_code() {
        includes(
            "http://example.com/b<del>b</del>",
            r#""http://example.com/b""#,
        );
        includes(
            "http://example.com/b<del>b</del>",
            "&lt;del&gt;b&lt;/del&gt;",
        );
        includes(
            r#"http://example.com/blahblahblahblah/a<script>alert("Hello")</script>"#,
            r#""http://example.com/blahblahblahblah/a""#,
        );
        includes(
            r#"http://example.com/blahblahblahblah/a<script>alert("Hello")</script>"#,
            "&lt;script&gt;alert(&quot;Hello&quot;)&lt;/script&gt;",
        );
        includes(
            r#"<script>alert("Hello")</script>"#,
            "<p>&lt;script&gt;alert(&quot;Hello&quot;)&lt;/script&gt;</p>",
        );
        includes(
            r#"<img src="javascript:alert('XSS');">"#,
            "<p>&lt;img src=&quot;javascript:alert(&#39;XSS&#39;);&quot;&gt;</p>",
        );
    }

    #[test]
    fn outputs_an_invalid_url_raw() {
        assert_eq!(
            to_s(r"http://www\.google\.com"),
            r"<p>http://www\.google\.com</p>"
        );
    }

    #[test]
    fn truncates_a_lengthy_url() {
        let text = "lorem https://prepitaph.org/wip/web-dovespair/ ipsum";
        includes(text, r#"<span class="invisible">https://</span>"#);
        includes(
            text,
            r#"<span class="ellipsis">prepitaph.org/wip/web-dovespai</span>"#,
        );
        includes(text, r#"<span class="invisible">r/</span>"#);
    }

    #[test]
    fn does_not_truncate_a_sufficiently_short_url() {
        let text = "lorem https://prepitaph.org/wip/web-devspair/ ipsum";
        includes(text, r#"<span class="invisible">https://</span>"#);
        includes(
            text,
            r#"<span class="">prepitaph.org/wip/web-devspair/</span>"#,
        );
        includes(text, r#"<span class="invisible"></span>"#);
    }

    #[test]
    fn creates_hashtag_links() {
        includes(
            "#hashtag",
            r#"/tags/hashtag" class="mention hashtag" rel="tag">#<span>hashtag</span></a>"#,
        );
        includes(
            "#hashtagタグ",
            r#"/tags/hashtag%E3%82%BF%E3%82%B0" class="mention hashtag" rel="tag">#<span>hashtagタグ</span></a>"#,
        );
    }

    #[test]
    fn matches_extended_uris() {
        includes("xmpp:user@instance.com", r#"href="xmpp:user@instance.com""#);
        includes(
            "please join xmpp:muc@instance.com?join right now",
            r#"href="xmpp:muc@instance.com?join""#,
        );
        includes(
            "wikipedia gives this example of a magnet uri: magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a",
            r#"href="magnet:?xt=urn:btih:c12fe1c06bba254a9dc9f519b335aa7c1367a88a""#,
        );
    }

    // spec/lib/html_aware_formatter_spec.rb, the local case
    #[test]
    fn formats_local_text() {
        assert_eq!(to_s("Foo bar"), "<p>Foo bar</p>");
    }

    #[test]
    fn writes_the_whole_link() {
        assert_eq!(
            to_s("see https://example.com/a #Tag"),
            format!(
                r#"<p>see <a href="https://example.com/a" target="_blank" rel="nofollow noopener" translate="no"><span class="invisible">https://</span><span class="">example.com/a</span><span class="invisible"></span></a> <a href="https://{DOMAIN}/tags/Tag" class="mention hashtag" rel="tag">#<span>Tag</span></a></p>"#
            )
        );
    }

    #[test]
    fn shows_the_domain_when_usernames_collide_or_asked() {
        let accounts = [
            MentionTarget {
                username: "bob".into(),
                domain: None,
                url: format!("https://{DOMAIN}/@bob"),
            },
            MentionTarget {
                username: "bob".into(),
                domain: Some("remote.example".into()),
                url: "https://remote.example/@bob".into(),
            },
            MentionTarget {
                username: "carol".into(),
                domain: Some("xn--bcher-kva.example".into()),
                url: "https://xn--bcher-kva.example/@carol".into(),
            },
        ];
        let options = Options {
            preloaded_accounts: &accounts,
            ..Options::new(DOMAIN)
        };
        assert_eq!(
            format("@bob@remote.example @carol@xn--bcher-kva.example", &options),
            r#"<p><span class="h-card" translate="no"><a href="https://remote.example/@bob" class="u-url mention">@<span>bob@remote.example</span></a></span> <span class="h-card" translate="no"><a href="https://xn--bcher-kva.example/@carol" class="u-url mention">@<span>carol</span></a></span></p>"#
        );
        let lookup = [accounts[2].clone()];
        let field = Options {
            lookup: &lookup,
            multiline: false,
            with_domains: true,
            with_rel_me: true,
            ..Options::new(DOMAIN)
        };
        assert_eq!(
            format("@carol@xn--bcher-kva.example https://e.example", &field),
            r#"<span class="h-card" translate="no"><a href="https://xn--bcher-kva.example/@carol" class="u-url mention">@<span>carol@bücher.example</span></a></span> <a href="https://e.example" target="_blank" rel="nofollow noopener me" translate="no"><span class="invisible">https://</span><span class="">e.example</span><span class="invisible"></span></a>"#
        );
    }

    #[test]
    fn a_mention_of_this_instance_is_local() {
        let accounts = [MentionTarget {
            username: "alice".into(),
            domain: None,
            url: format!("https://{DOMAIN}/@alice"),
        }];
        let options = Options {
            preloaded_accounts: &accounts,
            ..Options::new(DOMAIN)
        };
        assert!(
            format(&format!("@alice@{}", DOMAIN.to_uppercase()), &options)
                .contains("@<span>alice</span>")
        );
    }

    #[test]
    fn quotes_get_a_fallback_link() {
        let options = Options {
            quoted_status_url: Some("https://remote.example/@bob/1"),
            ..Options::new(DOMAIN)
        };
        assert_eq!(
            format("hi  there", &options),
            r#"<p class="quote-inline">RE: <a href="https://remote.example/@bob/1" target="_blank" rel="nofollow noopener" translate="no"><span class="invisible">https://</span><span class="">remote.example/@bob/1</span><span class="invisible"></span></a></p><p>hi there</p>"#
        );
        assert_eq!(
            format("", &options),
            r#"<p class="quote-inline">RE: <a href="https://remote.example/@bob/1" target="_blank" rel="nofollow noopener" translate="no"><span class="invisible">https://</span><span class="">remote.example/@bob/1</span><span class="invisible"></span></a></p>"#
        );
        // Already linked in the text: no fallback.
        assert_eq!(
            format("https://remote.example/@bob/1", &options),
            to_s("https://remote.example/@bob/1")
        );
    }

    #[test]
    fn single_line_keeps_newlines() {
        let options = Options {
            multiline: false,
            ..Options::new(DOMAIN)
        };
        assert_eq!(format("a\nb", &options), "a\nb");
    }

    #[test]
    fn mention_lookups_skip_this_instance() {
        assert_eq!(
            mention_lookups(&format!("@a @b@{DOMAIN} @c@else.example"), DOMAIN),
            vec![
                ("a".to_owned(), None),
                ("b".to_owned(), None),
                ("c".to_owned(), Some("else.example".to_owned()))
            ]
        );
    }
}
