//! Builders for the FEP-style consent handshake (ojak's reusable pattern).
//!
//! A requester sends a `*Request`; the target replies with an `Accept` carrying
//! a `result` authorization URI, or a `Reject`. The same pattern backs quote
//! posts (`QuoteRequest`/`QuoteAuthorization`) and account features
//! (`FeatureRequest`/`FeatureAuthorization`). These helpers write ojak's
//! generated consent vocabulary as delivery-ready JSON, under a context that
//! defines the consent terms, as Mastodon's `context_helper` does: a strict
//! JSON-LD consumer drops a type its context does not define.

use ojak_vocab as vocab;
use serde_json::{json, Value};
use vocab::{AnyActor, AnyObject, Iri};

use super::activity::with_context;

fn iri(s: &str) -> anyhow::Result<Iri> {
    crate::federation::portable::iri(s)
}

fn object_iri(s: &str) -> anyhow::Result<AnyObject> {
    iri(s).map(AnyObject::Iri)
}

/// The ActivityStreams context, and the terms of one consent type.
fn context(terms: Value) -> Value {
    json!([vocab::ACTIVITYSTREAMS_CONTEXT, terms])
}

/// The terms an authorization stamp's interaction properties need.
fn authorization_terms(kind: &str, iri: &str) -> Value {
    json!({
        "gts": "https://gotosocial.org/ns#",
        kind: iri,
        "interactingObject": {"@id": "gts:interactingObject", "@type": "@id"},
        "interactionTarget": {"@id": "gts:interactionTarget", "@type": "@id"},
    })
}

/// Build a `FeatureRequest` (collection owner asks a remote account for consent
/// to be featured). `id` is the request activity URI, `account` the account to
/// feature, `collection` the collection doing the featuring.
pub fn feature_request(
    id: &str,
    actor: &str,
    account: &str,
    collection: &str,
) -> anyhow::Result<Value> {
    let request = vocab::FeatureRequest {
        id: Some(iri(id)?),
        actors: vec![AnyActor::Iri(iri(actor)?)],
        objects: vec![object_iri(account)?],
        instruments: vec![object_iri(collection)?],
        ..Default::default()
    };
    Ok(with_context(
        &request,
        context(json!({"FeatureRequest": "https://w3id.org/fep/7aa9#FeatureRequest"})),
    ))
}

/// Build a `QuoteRequest` (we ask a remote author for consent to quote their
/// post). `id` is the request activity URI, `quoted_status` the post we want to
/// quote, `quoting_status` our quote post.
pub fn quote_request(
    id: &str,
    actor: &str,
    quoted_status: &str,
    quoting_status: &str,
) -> anyhow::Result<Value> {
    let request = vocab::QuoteRequest {
        id: Some(iri(id)?),
        actors: vec![AnyActor::Iri(iri(actor)?)],
        objects: vec![object_iri(quoted_status)?],
        instruments: vec![object_iri(quoting_status)?],
        ..Default::default()
    };
    Ok(with_context(
        &request,
        context(json!({"QuoteRequest": "https://w3id.org/fep/044f#QuoteRequest"})),
    ))
}

/// Build an `Accept` granting a request, pointing `result` at an authorization
/// stamp. `to` is the original requester.
pub fn accept(
    id: &str,
    actor: &str,
    to: &str,
    request_uri: &str,
    authorization_uri: &str,
) -> anyhow::Result<Value> {
    let accept = vocab::Accept {
        id: Some(iri(id)?),
        actors: vec![AnyActor::Iri(iri(actor)?)],
        objects: vec![object_iri(request_uri)?],
        results: vec![object_iri(authorization_uri)?],
        tos: vec![object_iri(to)?],
        ..Default::default()
    };
    Ok(super::activity::document(&accept))
}

/// Build a `Reject` declining a request.
pub fn reject(id: &str, actor: &str, to: &str, request_uri: &str) -> anyhow::Result<Value> {
    let reject = vocab::Reject {
        id: Some(iri(id)?),
        actors: vec![AnyActor::Iri(iri(actor)?)],
        objects: vec![object_iri(request_uri)?],
        tos: vec![object_iri(to)?],
        ..Default::default()
    };
    Ok(super::activity::document(&reject))
}

/// Build a `FeatureAuthorization` stamp object (served at `id`).
pub fn feature_authorization(id: &str, collection: &str, account: &str) -> anyhow::Result<Value> {
    let authorization = vocab::FeatureAuthorization {
        id: Some(iri(id)?),
        interacting_object: Some(object_iri(collection)?),
        interaction_target: Some(object_iri(account)?),
        ..Default::default()
    };
    Ok(with_context(
        &authorization,
        context(authorization_terms(
            "FeatureAuthorization",
            "https://w3id.org/fep/7aa9#FeatureAuthorization",
        )),
    ))
}

/// Build a `QuoteAuthorization` stamp object (served at `id`).
pub fn quote_authorization(
    id: &str,
    quoted_account: &str,
    quoting_status: &str,
    quoted_status: &str,
) -> anyhow::Result<Value> {
    let authorization = vocab::QuoteAuthorization {
        id: Some(iri(id)?),
        attributions: vec![AnyActor::Iri(iri(quoted_account)?)],
        interacting_object: Some(object_iri(quoting_status)?),
        interaction_target: Some(object_iri(quoted_status)?),
        ..Default::default()
    };
    Ok(with_context(
        &authorization,
        context(authorization_terms(
            "QuoteAuthorization",
            "https://w3id.org/fep/044f#QuoteAuthorization",
        )),
    ))
}

/// The `@context` of a document carrying a `QuoteAuthorization`: Mastodon's
/// `quote_authorizations` extension.
pub fn quote_authorization_context() -> Value {
    context(authorization_terms(
        "QuoteAuthorization",
        "https://w3id.org/fep/044f#QuoteAuthorization",
    ))
}

/// `ActivityPub::DeleteQuoteAuthorizationSerializer`: `actor` takes back the
/// stamp `authorization` (a `QuoteAuthorization` without its `@context`),
/// addressed to the public. The context is the stamp's, as the serializer
/// hoists its object's `context_extensions`.
pub fn delete_quote_authorization(id: &str, actor: &str, authorization: Value) -> Value {
    json!({
        "@context": quote_authorization_context(),
        "id": id,
        "type": "Delete",
        "actor": actor,
        "to": [vocab::ACTIVITYSTREAMS_PUBLIC],
        "object": authorization,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_quote_authorization_shape() {
        let stamp = quote_authorization(
            "https://b.test/users/bob/quote_authorizations/1",
            "https://b.test/users/bob",
            "https://a.test/notes/1",
            "https://b.test/notes/9",
        )
        .unwrap();
        let mut object = stamp.clone();
        object.as_object_mut().unwrap().remove("@context");
        let v = delete_quote_authorization(
            "https://b.test/users/bob/quote_authorizations/1#delete",
            "https://b.test/users/bob",
            object,
        );
        assert_eq!(v["type"], "Delete");
        assert_eq!(
            v["to"],
            json!(["https://www.w3.org/ns/activitystreams#Public"])
        );
        assert_eq!(v["object"]["type"], "QuoteAuthorization");
        assert!(v["object"].get("@context").is_none());
        assert_eq!(v["@context"], stamp["@context"]);
    }

    #[test]
    fn feature_request_shape() {
        let v = feature_request(
            "https://a.test/users/alice/feature_requests/1",
            "https://a.test/users/alice",
            "https://b.test/users/bob",
            "https://a.test/collections/1",
        )
        .unwrap();
        assert_eq!(v["type"], "FeatureRequest");
        assert_eq!(v["actor"], "https://a.test/users/alice");
        assert_eq!(v["object"], "https://b.test/users/bob");
        assert_eq!(v["instrument"], "https://a.test/collections/1");
    }

    #[test]
    fn accept_carries_result_and_to() {
        let v = accept(
            "https://b.test/users/bob#accepts/feature_requests/1",
            "https://b.test/users/bob",
            "https://a.test/users/alice",
            "https://a.test/users/alice/feature_requests/1",
            "https://b.test/users/bob/feature_authorizations/1",
        )
        .unwrap();
        assert_eq!(v["type"], "Accept");
        assert_eq!(v["to"], "https://a.test/users/alice");
        assert_eq!(v["object"], "https://a.test/users/alice/feature_requests/1");
        assert_eq!(
            v["result"],
            "https://b.test/users/bob/feature_authorizations/1"
        );
    }

    #[test]
    fn authorizations_have_correct_markers() {
        let f = feature_authorization(
            "https://b.test/users/bob/feature_authorizations/1",
            "https://a.test/collections/1",
            "https://b.test/users/bob",
        )
        .unwrap();
        assert_eq!(f["type"], "FeatureAuthorization");
        assert_eq!(f["interactingObject"], "https://a.test/collections/1");
        assert_eq!(f["interactionTarget"], "https://b.test/users/bob");
        assert!(f.get("attributedTo").is_none());

        let q = quote_authorization(
            "https://b.test/notes/9/approvals/1",
            "https://b.test/users/bob",
            "https://a.test/notes/1",
            "https://b.test/notes/9",
        )
        .unwrap();
        assert_eq!(q["type"], "QuoteAuthorization");
        assert_eq!(q["id"], "https://b.test/notes/9/approvals/1");
        assert_eq!(q["attributedTo"], "https://b.test/users/bob");
        assert_eq!(q["interactingObject"], "https://a.test/notes/1");
        assert_eq!(q["interactionTarget"], "https://b.test/notes/9");
        // The @context is a compound array declaring the FEP-044f
        // QuoteAuthorization term (plus GoToSocial's interaction term
        // definitions) so the markers resolve for remote consumers.
        let ctx = &q["@context"];
        assert!(
            ctx.as_array().is_some_and(|entries| entries
                .iter()
                .any(|e| e.get("QuoteAuthorization").and_then(|v| v.as_str())
                    == Some("https://w3id.org/fep/044f#QuoteAuthorization"))),
            "compound @context must declare the QuoteAuthorization term: {ctx}"
        );
    }
}
