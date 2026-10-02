//! Mastodon's `PlainTextFormatter` (*app/lib/plain_text_formatter.rb*): a
//! post or bio as plain text, for search, filters and plain-text email.

use std::sync::LazyLock;

use super::html::{self, Node};

/// `PlainTextFormatter::NEWLINE_TAGS_RE`.
static NEWLINE_TAGS_RE: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)(?:<br />|<br>|</p>)+").expect("valid pattern"));

/// The elements Mastodon's Sanitize config removes with their contents.
const REMOVED: &[&str] = &[
    "iframe",
    "math",
    "noembed",
    "noframes",
    "noscript",
    "plaintext",
    "script",
    "style",
    "svg",
    "xmp",
];

/// `PlainTextFormatter.new(text, local).to_s`.
pub fn format(text: &str, local: bool) -> String {
    if local {
        return text.to_owned();
    }
    let with_newlines = NEWLINE_TAGS_RE.replace_all(text, "$0\n");
    let Some(nodes) = html::parse_fragment(&with_newlines) else {
        return String::new();
    };
    let mut out = String::new();
    text_without_removed(&nodes, &mut out);
    // `String#chomp`.
    if let Some(stripped) = out.strip_suffix("\r\n") {
        stripped.to_owned()
    } else if let Some(stripped) = out.strip_suffix(['\n', '\r']) {
        stripped.to_owned()
    } else {
        out
    }
}

fn text_without_removed(nodes: &[Node], out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) if REMOVED.contains(&e.name.to_lowercase().as_str()) => {}
            Node::Element(e) => text_without_removed(&e.children, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::format;

    // spec/lib/plain_text_formatter_spec.rb
    #[test]
    fn returns_local_text_raw() {
        assert_eq!(
            format("<p>a text by a nerd who uses an HTML tag in text</p>", true),
            "<p>a text by a nerd who uses an HTML tag in text</p>"
        );
    }

    #[test]
    fn strips_inline_tags() {
        assert_eq!(format("<b>Lorem</b> <em>ipsum</em>", false), "Lorem ipsum");
    }

    #[test]
    fn mixed_case_paragraphs_insert_a_newline() {
        assert_eq!(format("<P>Lorem</P><p>ipsum</p>", false), "Lorem\nipsum");
    }

    #[test]
    fn a_single_br_inserts_a_newline() {
        assert_eq!(format("Lorem<br>ipsum", false), "Lorem\nipsum");
    }

    #[test]
    fn consecutive_brs_insert_a_single_newline() {
        assert_eq!(format("Lorem<br><br><br>ipsum", false), "Lorem\nipsum");
    }

    #[test]
    fn unescapes_entities() {
        assert_eq!(
            format("Lorem &amp; ipsum &#x2764;", false),
            "Lorem & ipsum ❤"
        );
    }

    #[test]
    fn strips_scripts_with_their_contents() {
        assert_eq!(
            format(r#"Lorem <script> alert("Booh!") </script>ipsum"#, false),
            "Lorem ipsum"
        );
    }

    #[test]
    fn strips_comments() {
        assert_eq!(format("Lorem <!-- Booh! -->ipsum", false), "Lorem ipsum");
    }

    #[test]
    fn keeps_ruby_text() {
        assert_eq!(
            format(
                "<p>Lorem <ruby>明日 <rp>(</rp><rt>Ashita</rt><rp>)</rp></ruby> ipsum</p>",
                false
            ),
            "Lorem 明日 (Ashita) ipsum"
        );
    }
}
