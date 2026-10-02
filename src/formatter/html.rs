//! A parsed HTML fragment as an owned tree, for the sanitizer to rewrite, and
//! its serialization the way Nokogiri's HTML5 `to_html` writes it.

/// Nokogiri's `Nokogiri::Gumbo::DEFAULT_MAX_TREE_DEPTH`: deeper than this, the
/// parse raises, and `HtmlAwareFormatter` shows nothing.
const MAX_TREE_DEPTH: usize = 400;
/// `Nokogiri::Gumbo::DEFAULT_MAX_ATTRIBUTES`.
const MAX_ATTRIBUTES: usize = 400;

#[derive(Debug, Clone)]
pub(crate) enum Node {
    Text(String),
    Element(Element),
}

#[derive(Debug, Clone)]
pub(crate) struct Element {
    /// The local name, as the parser gives it (HTML names lowercased).
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Element {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Nokogiri's `node['name'] = value`: replaced where it is, or appended.
    pub fn set_attr(&mut self, name: &str, value: &str) {
        match self.attrs.iter_mut().find(|(k, _)| k == name) {
            Some((_, v)) => *v = value.to_owned(),
            None => self.attrs.push((name.to_owned(), value.to_owned())),
        }
    }

    pub fn remove_attr(&mut self, name: &str) {
        self.attrs.retain(|(k, _)| k != name);
    }

    /// Nokogiri's `#text`: the text of every descendant.
    pub fn text(&self) -> String {
        let mut out = String::new();
        text_into(&self.children, &mut out);
        out
    }

    pub fn element_children(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|c| match c {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        })
    }
}

pub(crate) fn text_into(nodes: &[Node], out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) => text_into(&e.children, out),
        }
    }
}

/// `Nokogiri::HTML5.fragment`, comments and doctypes left out. `None` where
/// Nokogiri would raise for running past its limits.
pub(crate) fn parse_fragment(html: &str) -> Option<Vec<Node>> {
    let fragment = scraper::Html::parse_fragment(html);
    convert_children(*fragment.root_element(), 1)
}

fn convert_children(node: ego_tree::NodeRef<'_, scraper::Node>, depth: usize) -> Option<Vec<Node>> {
    let mut out = Vec::new();
    for child in node.children() {
        match child.value() {
            scraper::Node::Text(t) => match out.last_mut() {
                Some(Node::Text(prev)) => prev.push_str(t),
                _ => out.push(Node::Text(t.to_string())),
            },
            scraper::Node::Element(e) => {
                if depth > MAX_TREE_DEPTH || e.attrs.len() > MAX_ATTRIBUTES {
                    return None;
                }
                out.push(Node::Element(Element {
                    name: e.name().to_owned(),
                    attrs: e
                        .attrs()
                        .map(|(k, v)| (k.to_owned(), v.to_owned()))
                        .collect(),
                    children: convert_children(child, depth + 1)?,
                }));
            }
            _ => {}
        }
    }
    Some(out)
}

const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "basefont", "bgsound", "br", "col", "embed", "frame", "hr", "img", "input",
    "keygen", "link", "meta", "param", "source", "track", "wbr",
];

const RAW_TEXT_ELEMENTS: &[&str] = &[
    "style",
    "script",
    "xmp",
    "iframe",
    "noembed",
    "noframes",
    "plaintext",
    "noscript",
];

/// `node.to_html(preserve_newline: true)` for a fragment's children.
pub(crate) fn serialize(nodes: &[Node]) -> String {
    let mut out = String::new();
    serialize_into(nodes, false, &mut out);
    out
}

fn serialize_into(nodes: &[Node], raw: bool, out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(t) if raw => out.push_str(t),
            Node::Text(t) => escape_text(t, out),
            Node::Element(e) => {
                out.push('<');
                out.push_str(&e.name);
                for (key, value) in &e.attrs {
                    out.push(' ');
                    out.push_str(key);
                    out.push_str("=\"");
                    escape_attribute(value, out);
                    out.push('"');
                }
                out.push('>');
                if VOID_ELEMENTS.contains(&e.name.as_str()) {
                    continue;
                }
                if matches!(e.name.as_str(), "pre" | "textarea" | "listing")
                    && matches!(e.children.first(), Some(Node::Text(t)) if t.starts_with('\n'))
                {
                    out.push('\n');
                }
                serialize_into(
                    &e.children,
                    RAW_TEXT_ELEMENTS.contains(&e.name.as_str()),
                    out,
                );
                out.push_str("</");
                out.push_str(&e.name);
                out.push('>');
            }
        }
    }
}

fn escape_text(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            c => out.push(c),
        }
    }
}

fn escape_attribute(value: &str, out: &mut String) {
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            c => out.push(c),
        }
    }
}

/// `ERB::Util.html_escape`.
pub fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}
