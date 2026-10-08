//! Finding the ActivityPub object behind a URL, mirroring Mastodon's
//! `FetchResourceService`.
//!
//! A URL someone pastes into search is rarely an object's `id`: it is the page
//! they were reading. Mastodon asks that page for ActivityPub through an
//! `Accept` header, and when the answer is HTML anyway it looks for the
//! `rel="alternate"` link naming the object — first in the `Link:` header, then
//! in the document itself. A server that serves its pages and its objects from
//! different paths is reachable *only* through that link: oeee.cafe hands
//! `/@author/{id}` to people and `/ap/posts/{id}` to servers, advertising the
//! second from the first with nothing but a `<link>` tag.
//!
//! Finding the alternate link is ojak's (`ojak::fetch::link_header_alternate`
//! and `html_alternate`); which objects count, and what to do with them, is
//! Mastodon's and stays here. The "served from elsewhere" step is not ojak's
//! `Fetcher::lookup`: Mastodon asks the `id` again whenever it differs from
//! where the object was served, same origin or not, with this `Accept`, and
//! keeps the response code for `ResolveURLService`.
//!
//! Two follow-ups are allowed, each `terminal` — the alternate link, and an
//! object whose `id` is not the URL it was served from — so a server cannot
//! walk us around an unbounded chain of redirections of its own choosing.

use serde_json::Value;

use crate::state::AppState;

/// `ActivityPub::TagManager::CONTEXT`.
const AS_CONTEXT: &str = "https://www.w3.org/ns/activitystreams";

/// `FetchResourceService::ACCEPT_HEADER`. `text/html` is accepted, at the
/// lowest possible priority, because the HTML is what carries the link to the
/// object on servers that do not content-negotiate.
const ACCEPT: &str = "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\", application/activity+json, text/html;q=0.1";

/// `ActivityPub::FetchRemoteActorService::SUPPORTED_TYPES`.
pub const ACTOR_TYPES: [&str; 5] = ["Application", "Group", "Organization", "Person", "Service"];

/// `ActivityPub::Activity::Create::SUPPORTED_TYPES + CONVERTED_TYPES` — the
/// object types a status can be built from.
pub const OBJECT_TYPES: [&str; 8] = [
    "Note", "Question", "Image", "Audio", "Video", "Article", "Page", "Event",
];

/// `FeaturedCollection`, which `FetchResourceService#expected_type?` also accepts.
pub const COLLECTION_TYPES: [&str; 1] = ["FeaturedCollection"];

/// An object fetched from the server that claims it.
pub struct FetchedResource {
    /// Where the object was finally served from, which is also its `id`.
    pub url: String,
    pub json: Value,
}

/// The outcome of a fetch. `response_code` is kept even when nothing was
/// resolved, because what to do next depends on *why* — Mastodon's
/// `ResolveURLService#process_url_from_db` reads it to tell "the origin is
/// down" from "the URL is wrong".
pub struct Fetched {
    pub resource: Option<FetchedResource>,
    pub response_code: Option<u16>,
}

/// Fetch whatever ActivityPub object `url` names, following one alternate link.
pub async fn fetch_resource(state: &AppState, url: &str) -> Fetched {
    if url.is_empty() {
        return Fetched {
            resource: None,
            response_code: None,
        };
    }
    let mut response_code = None;
    let resource = process(state, url, false, &mut response_code).await;
    Fetched {
        resource,
        response_code,
    }
}

async fn process(
    state: &AppState,
    url: &str,
    terminal: bool,
    code: &mut Option<u16>,
) -> Option<FetchedResource> {
    let resp = crate::federation::fetch::signed_get(state, url, ACCEPT)
        .await
        .ok()?;
    *code = Some(resp.status);
    if resp.status != 200 {
        return None;
    }

    let content_type = resp
        .headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    // Redirects are followed by the fetcher, each hop signed again; what the
    // body is compared with is where it was finally served from.
    let served = resp.url.as_str();

    if ojak::fetch::is_activity_content_type(&content_type) {
        return match read_activitypub(served, &resp.body) {
            ApBody::Resource(json) => Some(FetchedResource {
                url: served.to_owned(),
                json,
            }),
            // Served from somewhere other than the id it claims: ask the id
            // itself, once, so that what we store came from the host that owns
            // it. A second disagreement is a server playing games.
            ApBody::Elsewhere(id) if !terminal => Box::pin(process(state, &id, true, code)).await,
            _ => None,
        };
    }

    if terminal {
        return None;
    }

    // Not ActivityPub. Follow the alternate link, if the page names one.
    if let Some(href) = ojak::fetch::link_header_alternate(&resp.headers) {
        return Box::pin(process(state, &href, true, code)).await;
    }
    if mime_type(&content_type) != "text/html" {
        return None;
    }
    let href = ojak::fetch::html_alternate(&String::from_utf8_lossy(&resp.body), &resp.url)?;
    Box::pin(process(state, &href, true, code)).await
}

/// What an ActivityPub-typed response body turned out to be.
#[derive(Debug, PartialEq)]
enum ApBody {
    /// An object we can use, served from its own id.
    Resource(Value),
    /// An object whose `id` is not where it was served from.
    Elsewhere(String),
    /// Not JSON, not ActivityStreams, or not a type we resolve.
    Unusable,
}

/// `FetchResourceService#process_response`'s reading of an ActivityPub body.
fn read_activitypub(url: &str, body: &[u8]) -> ApBody {
    let Ok(json) = serde_json::from_slice::<Value>(body) else {
        return ApBody::Unusable;
    };
    if !supported_context(&json) || !expected_type(&json) {
        return ApBody::Unusable;
    }
    match json.get("id").and_then(Value::as_str).unwrap_or_default() {
        "" => ApBody::Unusable,
        id if id == url => ApBody::Resource(json),
        id => ApBody::Elsewhere(id.to_owned()),
    }
}

/// The media type of a `Content-Type`, without its parameters.
fn mime_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// `JsonLdHelper#supported_context?`.
pub(crate) fn supported_context(json: &Value) -> bool {
    match json.get("@context") {
        Some(Value::String(s)) => s == AS_CONTEXT,
        Some(Value::Array(items)) => items.iter().any(|v| v.as_str() == Some(AS_CONTEXT)),
        _ => false,
    }
}

/// `FetchResourceService#process_response`'s type gate: an actor, something a
/// status can be built from, or a featured collection.
fn expected_type(json: &Value) -> bool {
    type_matches(json, &ACTOR_TYPES)
        || type_matches(json, &OBJECT_TYPES)
        || type_matches(json, &COLLECTION_TYPES)
}

/// `JsonLdHelper#equals_or_includes_any?` over an object's `type`, which the
/// specification allows to be either a string or an array of them.
pub fn type_matches(json: &Value, types: &[&str]) -> bool {
    match json.get("type") {
        Some(Value::String(s)) => types.contains(&s.as_str()),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .any(|s| types.contains(&s)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn content_type_needs_the_activitystreams_profile() {
        assert!(ojak::fetch::is_activity_content_type(
            "application/activity+json"
        ));
        assert!(ojak::fetch::is_activity_content_type(
            "application/activity+json; charset=utf-8"
        ));
        assert!(ojak::fetch::is_activity_content_type(
            "application/ld+json; profile=\"https://www.w3.org/ns/activitystreams\""
        ));
        // JSON-LD that is not ActivityStreams, and plain pages, are not objects.
        assert!(!ojak::fetch::is_activity_content_type(
            "application/ld+json"
        ));
        assert!(!ojak::fetch::is_activity_content_type("application/json"));
        assert!(!ojak::fetch::is_activity_content_type(
            "text/html; charset=utf-8"
        ));
    }

    #[test]
    fn type_may_be_a_string_or_an_array() {
        assert!(type_matches(&json!({"type": "Note"}), &OBJECT_TYPES));
        assert!(type_matches(
            &json!({"type": ["Note", "Hashtag"]}),
            &OBJECT_TYPES
        ));
        assert!(!type_matches(&json!({"type": "Collection"}), &OBJECT_TYPES));
        assert!(!type_matches(&json!({}), &OBJECT_TYPES));
    }

    #[test]
    fn context_may_be_a_string_or_an_array() {
        assert!(supported_context(&json!({"@context": AS_CONTEXT})));
        assert!(supported_context(
            &json!({"@context": [AS_CONTEXT, "https://w3id.org/security/v1"]})
        ));
        assert!(!supported_context(
            &json!({"@context": "https://schema.org"})
        ));
        assert!(!supported_context(&json!({})));
    }

    /// The object oeee.cafe serves at the end of the alternate link.
    fn oeee_note() -> Value {
        json!({
            "@context": ["https://www.w3.org/ns/activitystreams", "https://w3id.org/security/v1"],
            "id": "https://oeee.cafe/ap/posts/75fbf20d",
            "type": "Note",
            "attributedTo": "https://oeee.cafe/ap/users/e54177ff",
            "content": "<p>drawing</p>",
            "url": "https://oeee.cafe/@miro/75fbf20d",
        })
    }

    #[test]
    fn an_object_served_from_its_own_id_is_the_resource() {
        let body = serde_json::to_vec(&oeee_note()).unwrap();
        assert_eq!(
            read_activitypub("https://oeee.cafe/ap/posts/75fbf20d", &body),
            ApBody::Resource(oeee_note())
        );
    }

    #[test]
    fn an_object_claiming_another_id_sends_us_there() {
        let body = serde_json::to_vec(&oeee_note()).unwrap();
        // What a `/@author/{id}` page would have answered had it served JSON.
        assert_eq!(
            read_activitypub("https://oeee.cafe/@miro/75fbf20d", &body),
            ApBody::Elsewhere("https://oeee.cafe/ap/posts/75fbf20d".into())
        );
    }

    #[test]
    fn a_body_that_is_not_activitystreams_is_unusable() {
        let no_context = json!({"id": "https://a.test/1", "type": "Note"});
        let wrong_type =
            json!({"@context": AS_CONTEXT, "id": "https://a.test/1", "type": "Collection"});
        let no_id = json!({"@context": AS_CONTEXT, "type": "Note"});
        for body in [no_context, wrong_type, no_id] {
            assert_eq!(
                read_activitypub("https://a.test/1", &serde_json::to_vec(&body).unwrap()),
                ApBody::Unusable
            );
        }
        assert_eq!(
            read_activitypub("https://a.test/1", b"<html>not json</html>"),
            ApBody::Unusable
        );
    }
}
