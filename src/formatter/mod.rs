//! Mastodon's content formatting: `TextFormatter` for what local accounts
//! write, which is stored as plain text and turned into HTML when it is
//! served; `Sanitize` with `MASTODON_STRICT` for the HTML remote servers send,
//! which is stored as it arrived; `HtmlAwareFormatter` choosing between the
//! two; and `PlainTextFormatter` going the other way.

pub mod extractor;
mod html;
pub mod plain_text;
pub mod sanitize;
pub mod text;
pub mod tlds;

pub use html::html_escape;
pub use text::{MentionTarget, Options};

/// `HtmlAwareFormatter.new(text, local, options).to_s`.
pub fn html_aware(text: &str, local: bool, options: &Options<'_>) -> String {
    if local {
        text::format(text, options)
    } else if text::blank(text) {
        String::new()
    } else {
        sanitize::strict(text)
    }
}

/// `Account::Field#extract_url_from_html`: a remote field's value, when it is
/// one link whose text is its `href`.
pub fn url_from_field_html(value: &str) -> Option<String> {
    let nodes = html::parse_fragment(value)?;
    let [html::Node::Element(element)] = nodes.as_slice() else {
        return None;
    };
    let href = element.attr("href")?;
    (element.name == "a" && href == element.text()).then(|| href.to_owned())
}

/// `FormattingHelper#account_field_value_format` for a field of a remote
/// account: a verified one as a shortened link to the URL it was verified
/// for, any other sanitized.
pub fn remote_field_value(value: &str, verified: bool) -> String {
    if verified {
        text::shortened_link(&url_from_field_html(value).unwrap_or_default(), false)
    } else {
        html_aware(value, false, &Options::new(""))
    }
}

/// `FormattingHelper#account_field_value_format` for a field of a local
/// account: `with_rel_me`, `with_domains`, on one line.
pub fn local_field_value(value: &str, local_domain: &str, lookup: &[MentionTarget]) -> String {
    text::format(
        value,
        &Options {
            multiline: false,
            with_domains: true,
            with_rel_me: true,
            lookup,
            ..Options::new(local_domain)
        },
    )
}

/// `FormattingHelper#account_bio_format` for a local account.
pub fn local_bio(note: &str, local_domain: &str, lookup: &[MentionTarget]) -> String {
    text::format(
        note,
        &Options {
            lookup,
            ..Options::new(local_domain)
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_text_is_sanitized() {
        let options = Options::new("example.com");
        assert_eq!(html_aware("Beep boop", false, &options), "Beep boop");
        assert_eq!(
            html_aware(
                "<ruby>明日 <rp>(</rp><rt>Ashita</rt><rp>)</rp></ruby>",
                false,
                &options
            ),
            "<ruby>明日 <rp>(</rp><rt>Ashita</rt><rp>)</rp></ruby>"
        );
        assert_eq!(html_aware("  ", false, &options), "");
    }

    #[test]
    fn verified_remote_fields_link_their_url() {
        assert_eq!(
            url_from_field_html(r#"<a href="https://a.example/" rel="me">https://a.example/</a>"#),
            Some("https://a.example/".to_owned())
        );
        assert_eq!(
            url_from_field_html(r#"<a href="https://a.example/">a.example</a>"#),
            None
        );
        assert_eq!(
            remote_field_value(
                r#"<a href="https://a.example/" rel="me">https://a.example/</a>"#,
                true
            ),
            r#"<a href="https://a.example/" target="_blank" rel="nofollow noopener" translate="no"><span class="invisible">https://</span><span class="">a.example/</span><span class="invisible"></span></a>"#
        );
    }
}
