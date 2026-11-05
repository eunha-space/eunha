//! The Markdown Mastodon renders its policy documents with:
//! `Redcarpet::Markdown.new(Redcarpet::Render::HTML, escape_html: true,
//! no_images: true)`, used by `REST::TermsOfServiceSerializer`,
//! `REST::PrivacyPolicySerializer` and `FormattingHelper#markdown`.
//!
//! Redcarpet with no extensions is close to CommonMark without GitHub's
//! additions: no tables, strikethrough, footnotes or bare-URL autolinks, all of
//! which pulldown-cmark leaves off unless asked. The two options are what this
//! module adds:
//!
//!  -  `escape_html`: HTML in the text is shown, not interpreted, so a raw
//!     `<script>` reads as text.
//!  -  `no_images`: Redcarpet declines to render an image, and the source is
//!     left as it was written.
//!
//! Mastodon: app/serializers/rest/terms_of_service_serializer.rb
//! Mastodon: app/serializers/rest/privacy_policy_serializer.rb
//! Mastodon: app/helpers/formatting_helper.rb

use pulldown_cmark::{html, CowStr, Event, Options, Parser, Tag, TagEnd};

/// `markdown.render(text)`.
pub fn render(text: &str) -> String {
    render_with(text, true, false)
}

/// `Redcarpet::Markdown.new(Redcarpet::Render::HTML)`, with none of the
/// options: HTML in the text is passed through and images render, as
/// `REST::ExtendedDescriptionSerializer` renders the extended description.
pub fn render_html(text: &str) -> String {
    render_with(text, false, true)
}

/// `Redcarpet::Render::HTML, no_images: true`, as `REST::InstanceSerializer`
/// renders the closed registrations message: HTML passes through, images do
/// not render.
pub fn render_without_images(text: &str) -> String {
    render_with(text, false, false)
}

fn render_with(text: &str, escape_html: bool, images: bool) -> String {
    let mut events = Vec::new();
    // An image's closing text, waiting for its `End`.
    let mut closers: Vec<String> = Vec::new();
    for event in Parser::new_ext(text, Options::empty()) {
        match event {
            Event::Html(raw) | Event::InlineHtml(raw) if escape_html => {
                events.push(Event::Text(raw))
            }
            event @ (Event::Start(Tag::Image { .. }) | Event::End(TagEnd::Image)) if images => {
                events.push(event)
            }
            Event::Start(Tag::Image {
                dest_url, title, ..
            }) => {
                events.push(Event::Text(CowStr::Borrowed("![")));
                closers.push(if title.is_empty() {
                    format!("]({dest_url})")
                } else {
                    format!("]({dest_url} \"{title}\")")
                });
            }
            Event::End(TagEnd::Image) => {
                let closer = closers.pop().unwrap_or_default();
                events.push(Event::Text(CowStr::from(closer)));
            }
            other => events.push(other),
        }
    }
    let mut out = String::new();
    html::push_html(&mut out, events.into_iter());
    out
}

/// `object.text.gsub('%{domain}', local_domain)`, as both serializers call it
/// before rendering since 4.7.5: a stray `%` is only a `%`.
pub fn interpolate_domain(text: &str, domain: &str) -> String {
    text.replace("%{domain}", domain)
}

/// The policy documents' `content`: interpolated, then rendered.
pub fn render_policy(text: &str, domain: &str) -> String {
    render(&interpolate_domain(text, domain))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_markdown() {
        assert_eq!(
            render("## Rules\n\nBe *kind*."),
            "<h2>Rules</h2>\n<p>Be <em>kind</em>.</p>\n"
        );
    }

    #[test]
    fn escapes_html() {
        assert_eq!(
            render("Hi <b>there</b>"),
            "<p>Hi &lt;b&gt;there&lt;/b&gt;</p>\n"
        );
        assert_eq!(
            render("<script>alert(1)</script>"),
            "&lt;script&gt;alert(1)&lt;/script&gt;"
        );
    }

    #[test]
    fn leaves_images_as_written() {
        assert_eq!(
            render("![a cat](https://example.com/cat.png)"),
            "<p>![a cat](https://example.com/cat.png)</p>\n"
        );
    }

    #[test]
    fn no_github_extensions() {
        assert!(!render("~~gone~~").contains("<del>"));
        assert!(!render("see https://example.com").contains("<a"));
    }

    /// A stray `%` used to make Ruby's `format` raise; `gsub` leaves it be.
    #[test]
    fn only_the_domain_is_interpolated() {
        for (text, want) in [
            ("at %{domain}, 100% sure", "at ex.com, 100% sure"),
            ("%{domain}%{domain}", "ex.comex.com"),
            ("%<domain>s %% %s %{other}", "%<domain>s %% %s %{other}"),
            ("plain text", "plain text"),
        ] {
            assert_eq!(interpolate_domain(text, "ex.com"), want, "{text:?}");
        }
    }
}
