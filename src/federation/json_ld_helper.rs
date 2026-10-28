//! Mastodon's `JsonLdHelper` accessors, for a document as it arrived.
//!
//! Mastodon dispatches on what a peer sent rather than on what it means, so
//! it reads the raw JSON: `type` may be a string or an array, `actor` an IRI
//! or an embedded object, `url` a string, a `Link` or a list of either. These
//! read a [`serde_json::Value`] with the same semantics, so that Eunha answers
//! as Mastodon does for the same document.
//!
//! Where Mastodon's Ruby would raise on a shape it does not expect — the `id`
//! of an array, say — these answer `None` or `false`.
//!
//! Mastodon: app/helpers/json_ld_helper.rb

use ojak_vocab::ACTIVITYSTREAMS_CONTEXT;
use serde_json::Value;

/// Whether Ruby holds `value` true: anything present but `null` and `false`.
#[must_use]
pub fn truthy(value: Option<&Value>) -> bool {
    !matches!(value, None | Some(Value::Null | Value::Bool(false)))
}

/// `equals_or_includes?`: whether `haystack` is `needle`, or is an array that
/// includes it.
#[must_use]
pub fn equals_or_includes(haystack: Option<&Value>, needle: &str) -> bool {
    match haystack {
        Some(Value::Array(items)) => items.iter().any(|item| item.as_str() == Some(needle)),
        Some(Value::String(value)) => value == needle,
        _ => false,
    }
}

/// Whether `json`'s `type` is, or includes, `type_name`: a document may give
/// several types, as `["Mention", "Link"]`.
#[must_use]
pub fn type_is(json: &Value, type_name: &str) -> bool {
    equals_or_includes(json.get("type"), type_name)
}

/// `first_of_value`: the first of an array, or the value itself. An empty
/// array has none.
#[must_use]
pub fn first_of_value(value: &Value) -> Option<&Value> {
    match value {
        Value::Array(items) => items.first(),
        value => Some(value),
    }
}

/// `as_array`: nothing for an absent or `null` value, an array's items, or
/// the value alone.
#[must_use]
pub fn as_array(value: Option<&Value>) -> Vec<&Value> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(item) => vec![item],
    }
}

/// `value_or_id`: a string as it is, or an object's `id`. Anything else, or
/// an `id` that is not a string, has none.
#[must_use]
pub fn value_or_id(value: &Value) -> Option<&str> {
    match value {
        Value::String(id) => Some(id),
        Value::Object(object) => object.get("id").and_then(Value::as_str),
        _ => None,
    }
}

/// `as_array(value).map { value_or_id(_1) }`, without the ones that have
/// none: the ids an audience, a list of tags' hrefs or `alsoKnownAs` names.
#[must_use]
pub fn ids(value: Option<&Value>) -> Vec<&str> {
    as_array(value)
        .into_iter()
        .filter_map(value_or_id)
        .collect()
}

/// `first_lang_string`: an `xsd:string` or `rdf:langString` property, which
/// compacts to a string, a value object or an array of either, or failing
/// that the first value of its language map (`nameMap` for `name`).
#[must_use]
pub fn first_lang_string<'a>(json: &'a Value, name: &str) -> Option<&'a str> {
    match json.get(name).filter(|value| truthy(Some(value))) {
        Some(value) => match first_of_value(value)? {
            Value::String(text) => Some(text),
            Value::Object(object) => object.get("@value").and_then(Value::as_str),
            _ => None,
        },
        None => json
            .get(format!("{name}Map"))
            .filter(|map| truthy(Some(map)))
            .and_then(Value::as_object)
            .and_then(|map| map.values().next())
            .and_then(Value::as_str),
    }
}

/// `url_to_href`: the `href` of a `url`, which may be a string, a `Link`, or
/// an array of strings or of `Link`s. Among `Link`s the first whose
/// `mimeType` is `preferred_type` is taken, a `Link` without one being
/// `text/html`; with no preference, the first.
#[must_use]
pub fn url_to_href<'a>(value: &'a Value, preferred_type: Option<&str>) -> Option<&'a str> {
    let single = match value {
        Value::Object(_) => preferred_link(std::slice::from_ref(value), preferred_type),
        Value::Array(links) if !links.first().is_some_and(Value::is_string) => {
            preferred_link(links, preferred_type)
        }
        Value::Array(links) => links.first(),
        value => Some(value),
    }?;
    match single {
        Value::String(href) => Some(href),
        link => link.get("href").and_then(Value::as_str),
    }
}

fn preferred_link<'a>(links: &'a [Value], preferred_type: Option<&str>) -> Option<&'a Value> {
    links.iter().find(|link| {
        preferred_type.is_none_or(|preferred| {
            let mime = link
                .get("mimeType")
                .and_then(Value::as_str)
                .filter(|mime| !mime.trim().is_empty())
                .unwrap_or("text/html");
            mime == preferred
        })
    })
}

/// `supported_context?`: whether `json`'s `@context` is, or includes, the
/// ActivityStreams context.
#[must_use]
pub fn supported_context(json: &Value) -> bool {
    equals_or_includes(json.get("@context"), ACTIVITYSTREAMS_CONTEXT)
}

/// `unsupported_uri_scheme?`: whether `uri` is anything but an `http` or
/// `https` URL, by how it starts, as Mastodon tells it.
#[must_use]
pub fn unsupported_uri_scheme(uri: Option<&str>) -> bool {
    !uri.is_some_and(|uri| uri.starts_with("http://") || uri.starts_with("https://"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truthiness_is_rubys() {
        assert!(!truthy(None));
        assert!(!truthy(Some(&json!(null))));
        assert!(!truthy(Some(&json!(false))));
        for value in [json!(true), json!(0), json!(""), json!([]), json!({})] {
            assert!(truthy(Some(&value)), "{value}");
        }
    }

    #[test]
    fn equals_or_includes_reads_a_value_or_an_array() {
        assert!(equals_or_includes(Some(&json!("Note")), "Note"));
        assert!(equals_or_includes(Some(&json!(["Link", "Note"])), "Note"));
        assert!(!equals_or_includes(Some(&json!(["Link"])), "Note"));
        assert!(!equals_or_includes(None, "Note"));
        assert!(type_is(&json!({"type": ["Mention", "Link"]}), "Mention"));
    }

    #[test]
    fn values_and_ids() {
        assert_eq!(first_of_value(&json!([1, 2])), Some(&json!(1)));
        assert_eq!(first_of_value(&json!([])), None);
        assert_eq!(first_of_value(&json!("a")), Some(&json!("a")));
        assert!(as_array(Some(&json!(null))).is_empty());
        assert_eq!(as_array(Some(&json!("a"))), vec![&json!("a")]);
        assert_eq!(
            value_or_id(&json!("https://a.example/1")),
            Some("https://a.example/1")
        );
        assert_eq!(
            value_or_id(&json!({"id": "https://a.example/1"})),
            Some("https://a.example/1")
        );
        assert_eq!(value_or_id(&json!({"id": 1})), None);
        assert_eq!(value_or_id(&json!(["https://a.example/1"])), None);
        assert_eq!(
            ids(Some(
                &json!(["https://a.example/1", {"id": "https://a.example/2"}, 3])
            )),
            vec!["https://a.example/1", "https://a.example/2"]
        );
    }

    #[test]
    fn first_lang_string_reads_each_compaction() {
        assert_eq!(first_lang_string(&json!({"name": "a"}), "name"), Some("a"));
        assert_eq!(
            first_lang_string(&json!({"name": ["a", "b"]}), "name"),
            Some("a")
        );
        assert_eq!(
            first_lang_string(&json!({"name": {"@value": "a", "@language": "en"}}), "name"),
            Some("a")
        );
        assert_eq!(
            first_lang_string(&json!({"nameMap": {"en": "a"}}), "name"),
            Some("a")
        );
        assert_eq!(
            first_lang_string(&json!({"name": null, "nameMap": {"en": "a"}}), "name"),
            Some("a")
        );
        assert_eq!(first_lang_string(&json!({}), "name"), None);
    }

    #[test]
    fn url_to_href_prefers_the_type_asked_for() {
        let links = json!([
            {"type": "Link", "mimeType": "application/activity+json", "href": "https://a.example/ap"},
            {"type": "Link", "href": "https://a.example/html"},
        ]);
        assert_eq!(
            url_to_href(&links, Some("text/html")),
            Some("https://a.example/html")
        );
        assert_eq!(url_to_href(&links, None), Some("https://a.example/ap"));
        assert_eq!(url_to_href(&links, Some("image/png")), None);
        assert_eq!(
            url_to_href(&json!("https://a.example/"), Some("text/html")),
            Some("https://a.example/")
        );
        assert_eq!(
            url_to_href(&json!(["https://a.example/1", "https://a.example/2"]), None),
            Some("https://a.example/1")
        );
        assert_eq!(
            url_to_href(&json!({"href": "https://a.example/h"}), Some("text/html")),
            Some("https://a.example/h")
        );
    }

    #[test]
    fn contexts_and_schemes() {
        assert!(supported_context(
            &json!({"@context": ACTIVITYSTREAMS_CONTEXT})
        ));
        assert!(supported_context(
            &json!({"@context": [ACTIVITYSTREAMS_CONTEXT, {}]})
        ));
        assert!(!supported_context(&json!({})));
        assert!(!unsupported_uri_scheme(Some("https://a.example/")));
        assert!(!unsupported_uri_scheme(Some("http://a.example/")));
        assert!(unsupported_uri_scheme(Some("ap://did:key:z6Mk/")));
        assert!(unsupported_uri_scheme(None));
    }
}
