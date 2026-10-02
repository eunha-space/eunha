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

/// Ruby's `format(text, domain:)`, as both serializers call it before
/// rendering, ported from `sprintf.c` for the one argument Mastodon passes: a
/// hash holding `domain`.
///
/// `%{domain}` and `%<domain>s` become the local domain and `%%` a single `%`;
/// a bare conversion takes the hash itself, so the `% s` in "100% sure" prints
/// `{domain: "example.com"}` as Ruby 4 inspects it. What Ruby raises on (an
/// unknown name, a second bare conversion, a named one mixed with a bare one, a
/// number made of the hash or the domain, a malformed or trailing `%`) is an
/// `Err` carrying Ruby's message, which the API answers as upstream answers its
/// unrescued exception.
pub fn interpolate_domain(text: &str, domain: &str) -> Result<String, String> {
    RubyFormat {
        chars: text.chars().collect(),
        domain,
    }
    .run()
}

struct RubyFormat<'a> {
    chars: Vec<char>,
    domain: &'a str,
}

/// The value a conversion formats: the hash Mastodon passes, or the domain a
/// name looked up in it.
#[derive(Clone, Copy)]
enum FormatArg {
    Hash,
    Domain,
}

/// `posarg` in `sprintf.c`: how the arguments have been addressed so far.
#[derive(Clone, Copy, PartialEq)]
enum Addressing {
    None,
    /// The count of bare conversions taken.
    Unnumbered(usize),
    Numbered,
    Named,
}

const HASH_INTO_INTEGER: &str = "no implicit conversion of Hash into Integer";

impl RubyFormat<'_> {
    /// `{domain: "…"}`, `Hash#inspect` as Ruby 3.4 and later write it.
    fn hash_inspect(&self) -> String {
        format!("{{domain: {}}}", ruby_inspect(self.domain))
    }

    fn run(self) -> Result<String, String> {
        let chars = &self.chars;
        let end = chars.len();
        let mut out = String::with_capacity(end);
        let mut addressing = Addressing::None;
        let mut p = 0;
        while p < end {
            if chars[p] != '%' {
                out.push(chars[p]);
                p += 1;
                continue;
            }
            p += 1;
            if p >= end {
                return Err("incomplete format specifier; use %% (double %) instead".into());
            }
            // Any flag, width or precision; `%%` takes none.
            let mut flagged = false;
            let mut minus = false;
            let mut width: Option<usize> = None;
            let mut prec: Option<usize> = None;
            let mut value: Option<FormatArg> = None;
            let mut named = false;
            loop {
                let Some(&c) = chars.get(p) else {
                    return Err("malformed format string".into());
                };
                match c {
                    ' ' | '#' | '+' | '-' | '0' => {
                        if width.is_some() {
                            return Err("flag after width".into());
                        }
                        if prec.is_some() {
                            return Err("flag after precision".into());
                        }
                        flagged = true;
                        minus |= c == '-';
                        p += 1;
                    }
                    '1'..='9' => {
                        let (n, next) = self.number(p)?;
                        p = next;
                        if chars[p] == '$' {
                            if value.is_some() {
                                return Err(format!("value given twice - {n}$"));
                            }
                            match addressing {
                                Addressing::Unnumbered(k) => {
                                    return Err(format!("numbered({n}) after unnumbered({k})"));
                                }
                                Addressing::Named => {
                                    return Err(format!("numbered({n}) after named"));
                                }
                                _ => {}
                            }
                            addressing = Addressing::Numbered;
                            if n != 1 {
                                return Err("too few arguments".into());
                            }
                            value = Some(FormatArg::Hash);
                            p += 1;
                        } else {
                            if width.is_some() {
                                return Err("width given twice".into());
                            }
                            if prec.is_some() {
                                return Err("width after precision".into());
                            }
                            flagged = true;
                            width = Some(n);
                        }
                    }
                    '<' | '{' => {
                        let term = if c == '<' { '>' } else { '}' };
                        let Some(close) = chars[p..].iter().position(|&x| x == term) else {
                            return Err("malformed name - unmatched parenthesis".into());
                        };
                        let close = p + close;
                        let name: String = chars[p..=close].iter().collect();
                        if named {
                            return Err(format!("named{name} after another name"));
                        }
                        match addressing {
                            Addressing::Unnumbered(k) => {
                                return Err(format!("named{name} after unnumbered({k})"));
                            }
                            Addressing::Numbered => {
                                return Err(format!("named{name} after numbered"));
                            }
                            _ => {}
                        }
                        addressing = Addressing::Named;
                        if name[1..name.len() - 1] != *"domain" {
                            return Err(format!("key{name} not found"));
                        }
                        named = true;
                        value = Some(FormatArg::Domain);
                        p = close;
                        if term == '}' {
                            out.push_str(&self.string(
                                FormatArg::Domain,
                                false,
                                minus,
                                width,
                                prec,
                            ));
                            break;
                        }
                        p += 1;
                    }
                    '*' => return Err(HASH_INTO_INTEGER.into()),
                    '.' => {
                        if prec.is_some() {
                            return Err("precision given twice".into());
                        }
                        flagged = true;
                        p += 1;
                        if chars.get(p) == Some(&'*') {
                            return Err(HASH_INTO_INTEGER.into());
                        }
                        let (n, next) = self.number(p)?;
                        prec = Some(n);
                        p = next;
                    }
                    '%' => {
                        if flagged {
                            return Err("invalid format character - %".into());
                        }
                        out.push('%');
                        break;
                    }
                    'c' | 's' | 'p' => {
                        let arg = match value {
                            Some(arg) => arg,
                            None => Self::next_arg(&mut addressing)?,
                        };
                        if c == 'c' {
                            let FormatArg::Domain = arg else {
                                return Err(HASH_INTO_INTEGER.into());
                            };
                            let first = self.domain.chars().take(1).collect();
                            out.push_str(&pad(first, minus, width));
                        } else {
                            out.push_str(&self.string(arg, c == 'p', minus, width, prec));
                        }
                        break;
                    }
                    'd' | 'i' | 'o' | 'x' | 'X' | 'b' | 'B' | 'u' | 'f' | 'g' | 'G' | 'e' | 'E'
                    | 'a' | 'A' => {
                        let arg = match value {
                            Some(arg) => arg,
                            None => Self::next_arg(&mut addressing)?,
                        };
                        let numeric = if matches!(c, 'f' | 'g' | 'G' | 'e' | 'E' | 'a' | 'A') {
                            "Float"
                        } else {
                            "Integer"
                        };
                        return Err(match arg {
                            FormatArg::Hash => format!("can't convert Hash into {numeric}"),
                            FormatArg::Domain => format!(
                                "invalid value for {numeric}(): {}",
                                ruby_inspect(self.domain)
                            ),
                        });
                    }
                    c if c.is_ascii_graphic() => {
                        return Err(format!("malformed format string - %{c}"));
                    }
                    _ => return Err("malformed format string".into()),
                }
            }
            p += 1;
        }
        Ok(out)
    }

    /// `GETNEXTARG`: the next bare conversion's argument, of which there is
    /// one, the hash.
    fn next_arg(addressing: &mut Addressing) -> Result<FormatArg, String> {
        let taken = match *addressing {
            Addressing::Numbered => return Err("unnumbered(1) mixed with numbered".into()),
            Addressing::Named => return Err("unnumbered(1) mixed with named".into()),
            Addressing::None => 0,
            Addressing::Unnumbered(k) => k,
        };
        *addressing = Addressing::Unnumbered(taken + 1);
        if taken >= 1 {
            return Err("too few arguments".into());
        }
        Ok(FormatArg::Hash)
    }

    /// `GETNUM`: digits, which a conversion must still follow.
    fn number(&self, mut p: usize) -> Result<(usize, usize), String> {
        let mut n: usize = 0;
        while let Some(d) = self.chars.get(p).and_then(|c| c.to_digit(10)) {
            n = n
                .checked_mul(10)
                .and_then(|n| n.checked_add(d as usize))
                .filter(|&n| n <= i32::MAX as usize)
                .ok_or_else(|| "width too big".to_owned())?;
            p += 1;
        }
        if p >= self.chars.len() {
            return Err("malformed format string - %*[0-9]".into());
        }
        Ok((n, p))
    }

    /// `%s` and `%p`: `to_s` or `inspect`, cut to the precision and padded to
    /// the width, both counted in characters.
    fn string(
        &self,
        arg: FormatArg,
        inspect: bool,
        minus: bool,
        width: Option<usize>,
        prec: Option<usize>,
    ) -> String {
        let text = match (arg, inspect) {
            (FormatArg::Hash, _) => self.hash_inspect(),
            (FormatArg::Domain, false) => self.domain.to_owned(),
            (FormatArg::Domain, true) => ruby_inspect(self.domain),
        };
        let text = match prec {
            Some(prec) => text.chars().take(prec).collect(),
            None => text,
        };
        pad(text, minus, width)
    }
}

/// `String#inspect` for a domain, which holds nothing but printable text.
fn ruby_inspect(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

fn pad(text: String, minus: bool, width: Option<usize>) -> String {
    let len = text.chars().count();
    match width {
        Some(width) if width > len => {
            let fill = " ".repeat(width - len);
            if minus {
                text + &fill
            } else {
                fill + &text
            }
        }
        _ => text,
    }
}

/// The policy documents' `content`: interpolated, then rendered. An `Err` is
/// what Ruby's `format` raised.
pub fn render_policy(text: &str, domain: &str) -> Result<String, String> {
    interpolate_domain(text, domain).map(|text| render(&text))
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

    /// Each case checked against `format(text, domain: "ex.com")` in Ruby
    /// 4.0.6, which Mastodon 4.7 runs on.
    #[test]
    fn formats_as_ruby_does() {
        let formatted = [
            (
                "at %{domain} (%<domain>s), 100%% sure",
                "at ex.com (ex.com), 100% sure",
            ),
            ("100% sure", "100{domain: \"ex.com\"}ure"),
            ("%<domain>s %-15{domain}|", "ex.com ex.com         |"),
            ("%.3{domain}", "ex."),
            ("%1$s", "{domain: \"ex.com\"}"),
            ("%p", "{domain: \"ex.com\"}"),
            ("%-25s|", "{domain: \"ex.com\"}       |"),
            ("%05s|", "{domain: \"ex.com\"}|"),
            ("%%{domain}", "%{domain}"),
            ("%{domain}%{domain}", "ex.comex.com"),
            ("%.2s", "{d"),
            ("%<domain>5s|", "ex.com|"),
            ("%5<domain>s|", "ex.com|"),
            ("%<domain>.2s", "ex"),
            ("%<domain>p", "\"ex.com\""),
            ("%<domain>%", "%"),
            ("% 10.4s|", "      {dom|"),
            ("%.s", ""),
            ("plain text", "plain text"),
        ];
        for (text, want) in formatted {
            assert_eq!(
                interpolate_domain(text, "ex.com").as_deref(),
                Ok(want),
                "{text:?}"
            );
        }
        let raises = [
            "%",
            "a %",
            "a % ",
            "%{domain} and 100% sure",
            "100% sure and %{domain}",
            "%s %s",
            "%{other}",
            "%d",
            "%<domain>d",
            "50%!",
            "%\n",
            "% \n",
            "%5",
            "%c",
            "%{",
            "%<domain>",
            "%-%",
            "%5%",
            "%.%",
            "%*s",
            "%é",
            "%1$s %{domain}",
            "%{domain} %1$s",
            "%2$s",
            "%1$s %s",
            "%s %1$s",
            "%<domain>{domain}",
            "%<domain>1$s",
            "%f",
            "%.",
            "%1$",
            "%{}",
        ];
        for text in raises {
            assert!(interpolate_domain(text, "ex.com").is_err(), "{text:?}");
        }
    }
}
