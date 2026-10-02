//! `Sanitize.fragment` with Mastodon's configurations
//! (*lib/sanitize_ext/sanitize_config.rb*), walking the tree as the Sanitize
//! gem does: each node goes through the transformers in order, then
//! `CleanElement`, before its children. An element that is not allowed is
//! dropped and its children kept in its place, except for the elements whose
//! contents go with them; a block-level one leaves a space on either side.

use std::sync::LazyLock;

use super::html::{self, Element, Node};

/// `Sanitize::Config::LINK_PROTOCOLS`.
pub const LINK_PROTOCOLS: &[&str] = &[
    "http", "https", "dat", "dweb", "ipfs", "ipns", "ssb", "gopher", "xmpp", "magnet", "gemini",
];

/// `Sanitize::Config::HTTP_PROTOCOLS`.
const HTTP_PROTOCOLS: &[&str] = &["http", "https"];

/// The default `remove_contents`: these go with everything inside them.
const REMOVE_CONTENTS: &[&str] = &[
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

/// The default `whitespace_elements`, each a space before and after.
const WHITESPACE_ELEMENTS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "dd",
    "div",
    "dl",
    "dt",
    "footer",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hgroup",
    "hr",
    "li",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "ul",
];

/// What a transformer did with a node.
enum Outcome {
    Keep,
    /// The node is gone; these took its place and are walked in turn.
    Replace(Vec<Node>),
}

type Transformer = fn(&mut Element) -> Outcome;

/// A `Sanitize::Config`, as far as Mastodon's use them.
pub(crate) struct Config {
    elements: &'static [&'static str],
    /// `attributes`, with `:all` as `"*"`.
    attributes: &'static [(&'static str, &'static [&'static str])],
    add_attributes: &'static [(&'static str, &'static [(&'static str, &'static str)])],
    /// `protocols`: element, attribute, the schemes allowed.
    protocols: &'static [(&'static str, &'static str, &'static [&'static str])],
    transformers: &'static [Transformer],
}

impl Config {
    fn allows_attribute(&self, element: &str, attribute: &str) -> bool {
        self.attributes
            .iter()
            .any(|(e, attrs)| (*e == element || *e == "*") && attrs.contains(&attribute))
    }
}

/// `Sanitize::Config::MASTODON_STRICT`.
pub(crate) static MASTODON_STRICT: Config = Config {
    elements: &[
        "p",
        "br",
        "span",
        "a",
        "del",
        "s",
        "pre",
        "blockquote",
        "code",
        "b",
        "strong",
        "u",
        "i",
        "em",
        "ul",
        "ol",
        "li",
        "ruby",
        "rt",
        "rp",
    ],
    attributes: &[
        ("*", &["lang"]),
        ("a", &["href", "rel", "class", "translate"]),
        ("span", &["class", "translate"]),
        ("ol", &["start", "reversed"]),
        ("li", &["value"]),
        ("p", &["class"]),
    ],
    add_attributes: &[("a", &[("rel", "nofollow noopener"), ("target", "_blank")])],
    protocols: &[],
    transformers: &[
        allowed_class_transformer,
        translate_transformer,
        math_transformer,
        unsupported_elements_transformer,
        unsupported_href_transformer,
    ],
};

/// `Sanitize::Config::MASTODON_OEMBED`.
pub(crate) static MASTODON_OEMBED: Config = Config {
    elements: &["audio", "iframe", "source", "video"],
    attributes: &[
        ("audio", &["controls"]),
        (
            "iframe",
            &[
                "allowfullscreen",
                "frameborder",
                "height",
                "scrolling",
                "src",
                "width",
            ],
        ),
        ("source", &["src", "type"]),
        ("video", &["controls", "height", "loop", "width"]),
    ],
    add_attributes: &[(
        "iframe",
        &[(
            "sandbox",
            "allow-scripts allow-same-origin allow-popups allow-popups-to-escape-sandbox allow-forms",
        )],
    )],
    protocols: &[
        ("iframe", "src", HTTP_PROTOCOLS),
        ("source", "src", HTTP_PROTOCOLS),
    ],
    transformers: &[],
};

/// `Sanitize::REGEX_PROTOCOL`: what an attribute value's scheme is, if it has
/// one.
pub(crate) fn protocol_of(value: &str) -> Option<String> {
    static REGEX_PROTOCOL: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)\A([^/#]*?)(?::|&#0*58|&#x0*3a)").expect("valid pattern")
    });
    REGEX_PROTOCOL.captures(value).map(|c| c[1].to_lowercase())
}

/// `Sanitize::REGEX_UNSUITABLE_CHARS`, stripped before parsing.
fn preprocess(html: &str) -> String {
    html.chars()
        .filter(|c| {
            !matches!(
                c,
                '\u{0000}'
                    | '\u{0340}'
                    | '\u{0341}'
                    | '\u{17a3}'
                    | '\u{17d3}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{206a}'..='\u{206f}'
                    | '\u{fff9}'..='\u{fffb}'
                    | '\u{feff}'
                    | '\u{fffc}'
                    | '\u{1d173}'..='\u{1d17a}'
                    | '\u{e0000}'..='\u{e007f}'
            )
        })
        .collect()
}

/// `Sanitize.fragment(html, config)`. `None` where Nokogiri refuses the parse.
pub(crate) fn fragment(html: &str, config: &Config) -> Option<String> {
    if html.is_empty() {
        return Some(String::new());
    }
    let nodes = html::parse_fragment(&preprocess(html))?;
    Some(html::serialize(&clean_nodes(nodes, config)))
}

/// `Sanitize.fragment(html, Sanitize::Config::MASTODON_STRICT)`.
pub fn strict(html: &str) -> String {
    fragment(html, &MASTODON_STRICT).unwrap_or_default()
}

/// `Sanitize.fragment(html, Sanitize::Config::MASTODON_OEMBED)`.
pub fn oembed(html: &str) -> String {
    fragment(html, &MASTODON_OEMBED).unwrap_or_default()
}

fn clean_nodes(nodes: Vec<Node>, config: &Config) -> Vec<Node> {
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        clean_node(node, config, &mut out);
    }
    out
}

fn clean_node(node: Node, config: &Config, out: &mut Vec<Node>) {
    let mut element = match node {
        Node::Text(t) => {
            out.push(Node::Text(t));
            return;
        }
        Node::Element(e) => e,
    };
    for transformer in config.transformers {
        if let Outcome::Replace(nodes) = transformer(&mut element) {
            for node in nodes {
                clean_node(node, config, out);
            }
            return;
        }
    }
    clean_element(element, config, out);
}

/// `Sanitize::Transformers::CleanElement`.
fn clean_element(mut element: Element, config: &Config, out: &mut Vec<Node>) {
    let name = element.name.to_lowercase();
    if !config.elements.contains(&name.as_str()) {
        let whitespace = WHITESPACE_ELEMENTS.contains(&name.as_str());
        let had_children = !element.children.is_empty();
        if whitespace {
            out.push(Node::Text(" ".to_owned()));
        }
        if had_children && !REMOVE_CONTENTS.contains(&name.as_str()) {
            for child in element.children {
                clean_node(child, config, out);
            }
        }
        if whitespace && had_children {
            out.push(Node::Text(" ".to_owned()));
        }
        return;
    }

    element.attrs.retain_mut(|(key, value)| {
        let attribute = key.to_lowercase();
        if !config.allows_attribute(&name, &attribute) {
            return false;
        }
        if let Some((_, _, allowed)) = config
            .protocols
            .iter()
            .find(|(e, a, _)| *e == name && *a == attribute)
        {
            let ok = match protocol_of(value) {
                Some(scheme) => allowed.contains(&scheme.as_str()),
                None => false,
            };
            if !ok {
                return false;
            }
            *value = value.trim().to_owned();
        }
        true
    });

    if let Some((_, attributes)) = config.add_attributes.iter().find(|(e, _)| *e == name) {
        for (key, value) in *attributes {
            element.set_attr(key, value);
        }
    }

    element.children = clean_nodes(std::mem::take(&mut element.children), config);
    out.push(Node::Element(element));
}

/// `ALLOWED_CLASS_TRANSFORMER`: only microformats classes and Mastodon's own
/// survive.
fn allowed_class_transformer(element: &mut Element) -> Outcome {
    if let Some(classes) = element.attr("class") {
        let kept = classes
            .split(['\t', '\n', '\x0c', '\r', ' '])
            .filter(|c| {
                ["h-", "p-", "u-", "dt-", "e-"]
                    .iter()
                    .any(|prefix| c.starts_with(prefix))
                    || matches!(
                        *c,
                        "mention" | "hashtag" | "ellipsis" | "invisible" | "quote-inline"
                    )
            })
            .collect::<Vec<_>>()
            .join(" ");
        element.set_attr("class", &kept);
    }
    Outcome::Keep
}

/// `TRANSLATE_TRANSFORMER`: `translate` only as `no`.
fn translate_transformer(element: &mut Element) -> Outcome {
    if element.attr("translate").is_some_and(|v| v != "no") {
        element.remove_attr("translate");
    }
    Outcome::Keep
}

/// `MATH_TRANSFORMER`: FEP-dc88 MathML, shown by its TeX annotation in
/// dollar signs, or else its plain-text one.
fn math_transformer(element: &mut Element) -> Outcome {
    if element.name.to_lowercase() != "math" {
        return Outcome::Keep;
    }
    let Some(semantics) = element.element_children().next() else {
        return Outcome::Keep;
    };
    if semantics.name != "semantics" {
        return Outcome::Keep;
    }
    let annotation = |encoding: &str| {
        semantics
            .element_children()
            .find(|c| c.name == "annotation" && c.attr("encoding") == Some(encoding))
    };
    let text = if let Some(tex) = annotation("application/x-tex") {
        if element.attr("display") == Some("block") {
            format!("$${}$$", tex.text())
        } else {
            format!("${}$", tex.text())
        }
    } else if let Some(plain) = annotation("text/plain") {
        plain.text()
    } else {
        return Outcome::Keep;
    };
    Outcome::Replace(vec![Node::Text(text)])
}

/// `UNSUPPORTED_ELEMENTS_TRANSFORMER`: a heading becomes a paragraph in bold.
fn unsupported_elements_transformer(element: &mut Element) -> Outcome {
    if !matches!(
        element.name.to_lowercase().as_str(),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
    ) {
        return Outcome::Keep;
    }
    let mut strong = std::mem::replace(
        element,
        Element {
            name: String::new(),
            attrs: Vec::new(),
            children: Vec::new(),
        },
    );
    strong.name = "strong".to_owned();
    Outcome::Replace(vec![Node::Element(Element {
        name: "p".to_owned(),
        attrs: Vec::new(),
        children: vec![Node::Element(strong)],
    })])
}

/// `UNSUPPORTED_HREF_TRANSFORMER`: a link whose scheme is not one of
/// `LINK_PROTOCOLS`, or that is relative or has no `href`, is replaced by
/// its text.
fn unsupported_href_transformer(element: &mut Element) -> Outcome {
    if element.name.to_lowercase() != "a" {
        return Outcome::Keep;
    }
    let supported = element
        .attr("href")
        .and_then(protocol_of)
        .is_some_and(|scheme| LINK_PROTOCOLS.contains(&scheme.as_str()));
    if supported {
        Outcome::Keep
    } else {
        Outcome::Replace(vec![Node::Text(element.text())])
    }
}

#[cfg(test)]
mod tests {
    use super::strict;

    // spec/lib/sanitize/config_spec.rb
    #[test]
    fn converts_h1_to_p_strong() {
        assert_eq!(strict("<h1>Foo</h1>"), "<p><strong>Foo</strong></p>");
    }

    #[test]
    fn keeps_ul() {
        assert_eq!(
            strict("<p>Check out:</p><ul><li>Foo</li><li>Bar</li></ul>"),
            "<p>Check out:</p><ul><li>Foo</li><li>Bar</li></ul>"
        );
    }

    #[test]
    fn keeps_start_and_reversed_attributes_of_ol() {
        assert_eq!(
            strict(r#"<p>Check out:</p><ol start="3" reversed=""><li>Foo</li><li>Bar</li></ol>"#),
            r#"<p>Check out:</p><ol start="3" reversed=""><li>Foo</li><li>Bar</li></ol>"#
        );
    }

    #[test]
    fn keeps_ruby_tags() {
        assert_eq!(
            strict("<p><ruby>明日 <rp>(</rp><rt>Ashita</rt><rp>)</rp></ruby></p>"),
            "<p><ruby>明日 <rp>(</rp><rt>Ashita</rt><rp>)</rp></ruby></p>"
        );
    }

    #[test]
    fn removes_a_without_href() {
        assert_eq!(strict("<a>Test</a>"), "Test");
    }

    #[test]
    fn removes_a_without_href_and_only_keeps_text_content() {
        assert_eq!(
            strict(r#"<a><span class="invisible">foo&amp;</span><span>Test</span></a>"#),
            "foo&amp;Test"
        );
    }

    #[test]
    fn removes_a_with_unsupported_scheme_in_href() {
        assert_eq!(strict(r#"<a href="foo://bar">Test</a>"#), "Test");
    }

    #[test]
    fn does_not_reinterpret_html_when_removing_unsupported_links() {
        assert_eq!(
            strict(
                r#"<a href="foo://bar">Test&lt;a href="https://example.com"&gt;test&lt;/a&gt;</a>"#
            ),
            r#"Test&lt;a href="https://example.com"&gt;test&lt;/a&gt;"#
        );
    }

    #[test]
    fn removes_math_when_unparsable_due_to_missing_attributes() {
        assert_eq!(
            strict("<math><semantics><annotation>x</annotation></semantics></math>"),
            ""
        );
    }

    #[test]
    fn removes_math_when_unparsable_due_to_missing_encoding_attribute() {
        assert_eq!(
            strict(r#"<math><semantics><annotation class="foo">x</annotation></semantics></math>"#),
            ""
        );
    }

    #[test]
    fn keeps_a_with_href() {
        assert_eq!(
            strict(r#"<a href="http://example.com">Test</a>"#),
            r#"<a href="http://example.com" rel="nofollow noopener" target="_blank">Test</a>"#
        );
    }

    #[test]
    fn keeps_a_with_translate_no() {
        assert_eq!(
            strict(r#"<a href="http://example.com" translate="no">Test</a>"#),
            r#"<a href="http://example.com" translate="no" rel="nofollow noopener" target="_blank">Test</a>"#
        );
    }

    #[test]
    fn removes_translate_attribute_with_invalid_value() {
        assert_eq!(
            strict(r#"<a href="http://example.com" translate="foo">Test</a>"#),
            r#"<a href="http://example.com" rel="nofollow noopener" target="_blank">Test</a>"#
        );
    }

    #[test]
    fn removes_a_with_unparsable_href() {
        assert_eq!(strict(r#"<a href=" https://google.fr">Test</a>"#), "Test");
    }

    #[test]
    fn keeps_a_with_supported_scheme_and_no_host() {
        assert_eq!(
            strict(r#"<a href="dweb:/a/foo">Test</a>"#),
            r#"<a href="dweb:/a/foo" rel="nofollow noopener" target="_blank">Test</a>"#
        );
    }

    #[test]
    fn sanitizes_math_to_latex() {
        assert_eq!(
            strict(
                r#"<math><semantics><mrow><msup><mi>x</mi><mi>n</mi></msup><mo>+</mo><mi>y</mi></mrow><annotation encoding="application/x-tex">x^n+y</annotation></semantics></math>"#
            ),
            "$x^n+y$"
        );
    }

    #[test]
    fn sanitizes_math_blocks_to_latex() {
        assert_eq!(
            strict(
                r#"<math display="block"><semantics><mrow><msup><mi>x</mi><mi>n</mi></msup><mo>+</mo><mi>y</mi></mrow><annotation encoding="application/x-tex">x^n+y</annotation></semantics></math>"#
            ),
            "$$x^n+y$$"
        );
    }

    #[test]
    fn math_sanitizer_falls_back_to_plaintext() {
        assert_eq!(
            strict(
                r#"<math><semantics><msqrt><mi>x</mi></msqrt><annotation encoding="text/plain">sqrt(x)</annotation></semantics></math>"#
            ),
            "sqrt(x)"
        );
    }

    #[test]
    fn prefers_latex() {
        assert_eq!(
            strict(
                r#"<math><semantics><msqrt><mi>x</mi></msqrt><annotation encoding="text/plain">sqrt(x)</annotation><annotation encoding="application/x-tex">\sqrt x</annotation></semantics></math>"#
            ),
            r"$\sqrt x$"
        );
    }

    // spec/lib/html_aware_formatter_spec.rb, the remote cases
    #[test]
    fn strips_scripts() {
        assert!(!strict(r#"<script>alert("Hello")</script>"#).contains("<script>"));
    }

    #[test]
    fn strips_malicious_classes() {
        assert_eq!(
            strict(r#"<span class="mention  status__content__spoiler-link">Show more</span>"#),
            r#"<span class="mention">Show more</span>"#
        );
    }

    #[test]
    fn keeps_mastodon_markup_in_attribute_order() {
        assert_eq!(
            strict(
                r#"<p>hi <span class="h-card"><a href="https://e.example/@a" class="u-url mention" rel="tag">@<span>a</span></a></span><br><img src="x"></p><div>block</div><hr>"#
            ),
            r#"<p>hi <span class="h-card"><a href="https://e.example/@a" class="u-url mention" rel="nofollow noopener" target="_blank">@<span>a</span></a></span><br></p> block  "#
        );
    }

    #[test]
    fn relative_and_script_links_become_text() {
        assert_eq!(
            strict(r#"<a href="/tags/x">#x</a> <a href="javascript:alert(1)">y</a>"#),
            "#x y"
        );
    }
}
