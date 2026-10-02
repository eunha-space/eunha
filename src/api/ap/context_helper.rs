//! Mastodon's `ContextHelper`: the named JSON-LD contexts and the context
//! extensions its ActivityPub serializers declare, and how
//! `ActivityPub::Adapter` folds the ones a document used into its
//! `@context`.

use serde_json::{json, Map, Value};

/// `ContextHelper::NAMED_CONTEXT_MAP`, in its order.
pub const NAMED_CONTEXTS: [(&str, &str); 4] = [
    ("activitystreams", "https://www.w3.org/ns/activitystreams"),
    ("security", "https://w3id.org/security/v1"),
    ("controlled_identifiers", "https://www.w3.org/ns/cid/v1"),
    ("webfinger", "https://purl.archive.org/socialweb/webfinger"),
];

/// `ContextHelper::CONTEXT_EXTENSION_MAP`, in its order.
#[must_use]
pub fn extension(name: &str) -> Option<Value> {
    let toot = "http://joinmastodon.org/ns#";
    Some(match name {
        "manually_approves_followers" => {
            json!({ "manuallyApprovesFollowers": "as:manuallyApprovesFollowers" })
        }
        "sensitive" => json!({ "sensitive": "as:sensitive" }),
        "hashtag" => json!({ "Hashtag": "as:Hashtag" }),
        "moved_to" => json!({ "movedTo": { "@id": "as:movedTo", "@type": "@id" } }),
        "also_known_as" => {
            json!({ "alsoKnownAs": { "@id": "as:alsoKnownAs", "@type": "@id" } })
        }
        "emoji" => json!({ "toot": toot, "Emoji": "toot:Emoji" }),
        "featured" => json!({
            "toot": toot,
            "featured": { "@id": "toot:featured", "@type": "@id" },
            "featuredTags": { "@id": "toot:featuredTags", "@type": "@id" },
        }),
        "property_value" => json!({
            "schema": "http://schema.org#",
            "PropertyValue": "schema:PropertyValue",
            "value": "schema:value",
        }),
        "atom_uri" => json!({ "ostatus": "http://ostatus.org#", "atomUri": "ostatus:atomUri" }),
        "conversation" => json!({
            "ostatus": "http://ostatus.org#",
            "inReplyToAtomUri": "ostatus:inReplyToAtomUri",
            "conversation": "ostatus:conversation",
        }),
        "focal_point" => json!({
            "toot": toot,
            "focalPoint": { "@container": "@list", "@id": "toot:focalPoint" },
        }),
        "blurhash" => json!({ "toot": toot, "blurhash": "toot:blurhash" }),
        "discoverable" => json!({ "toot": toot, "discoverable": "toot:discoverable" }),
        "indexable" => json!({ "toot": toot, "indexable": "toot:indexable" }),
        "memorial" => json!({ "toot": toot, "memorial": "toot:memorial" }),
        "voters_count" => json!({ "toot": toot, "votersCount": "toot:votersCount" }),
        "suspended" => json!({ "toot": toot, "suspended": "toot:suspended" }),
        "attribution_domains" => json!({
            "toot": toot,
            "attributionDomains": { "@id": "toot:attributionDomains", "@container": "@set" },
        }),
        "profile_settings" => json!({
            "toot": toot,
            "showFeatured": "toot:showFeatured",
            "showMedia": "toot:showMedia",
            "showRepliesInMedia": "toot:showRepliesInMedia",
        }),
        "quote_requests" => json!({ "QuoteRequest": "https://w3id.org/fep/044f#QuoteRequest" }),
        "quotes" => json!({
            "quote": { "@id": "https://w3id.org/fep/044f#quote", "@type": "@id" },
            "quoteUri": "http://fedibird.com/ns#quoteUri",
            "_misskey_quote": "https://misskey-hub.net/ns#_misskey_quote",
            "quoteAuthorization": {
                "@id": "https://w3id.org/fep/044f#quoteAuthorization",
                "@type": "@id",
            },
        }),
        "interaction_policies" => json!({
            "gts": "https://gotosocial.org/ns#",
            "interactionPolicy": { "@id": "gts:interactionPolicy", "@type": "@id" },
            "canFeature": { "@id": "https://w3id.org/fep/7aa9#canFeature", "@type": "@id" },
            "canQuote": { "@id": "gts:canQuote", "@type": "@id" },
            "automaticApproval": { "@id": "gts:automaticApproval", "@type": "@id" },
            "manualApproval": { "@id": "gts:manualApproval", "@type": "@id" },
        }),
        "quote_authorizations" => json!({
            "gts": "https://gotosocial.org/ns#",
            "QuoteAuthorization": "https://w3id.org/fep/044f#QuoteAuthorization",
            "interactingObject": { "@id": "gts:interactingObject", "@type": "@id" },
            "interactionTarget": { "@id": "gts:interactionTarget", "@type": "@id" },
        }),
        "feature_requests" => {
            json!({ "FeatureRequest": "https://w3id.org/fep/7aa9#FeatureRequest" })
        }
        "featured_collections" => json!({
            "FeaturedCollection": "https://w3id.org/fep/7aa9#FeaturedCollection",
            "FeaturedItem": "https://w3id.org/fep/7aa9#FeaturedItem",
            "FeatureRequest": "https://w3id.org/fep/7aa9#FeatureRequest",
            "FeatureAuthorization": "https://w3id.org/fep/7aa9#FeatureAuthorization",
            "topic": { "@id": "https://w3id.org/fep/7aa9#topic", "@type": "@id" },
            "featuredObject": { "@id": "https://w3id.org/fep/7aa9#featuredObject", "@type": "@id" },
            "featureAuthorization": {
                "@id": "https://w3id.org/fep/7aa9#featureAuthorization",
                "@type": "@id",
            },
        }),
        "feature_authorizations" => json!({
            "gts": "https://gotosocial.org/ns#",
            "FeatureAuthorization": "https://w3id.org/fep/7aa9#FeatureAuthorization",
            "interactingObject": { "@id": "gts:interactingObject", "@type": "@id" },
            "interactionTarget": { "@id": "gts:interactionTarget", "@type": "@id" },
        }),
        _ => return None,
    })
}

/// Every key of `CONTEXT_EXTENSION_MAP`, in its order.
pub const EXTENSIONS: [&str; 26] = [
    "manually_approves_followers",
    "sensitive",
    "hashtag",
    "moved_to",
    "also_known_as",
    "emoji",
    "featured",
    "property_value",
    "atom_uri",
    "conversation",
    "focal_point",
    "blurhash",
    "discoverable",
    "indexable",
    "memorial",
    "voters_count",
    "suspended",
    "attribution_domains",
    "profile_settings",
    "quote_requests",
    "quotes",
    "interaction_policies",
    "quote_authorizations",
    "feature_requests",
    "featured_collections",
    "feature_authorizations",
];

/// `ContextHelper#serialized_context`: the named contexts in the order
/// given, then one object merging the extensions, or the lone context
/// when there is only one.
#[must_use]
pub fn serialized_context(named: &[&str], extensions: &[&str]) -> Value {
    let mut context: Vec<Value> = named
        .iter()
        .filter_map(|name| {
            NAMED_CONTEXTS
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, url)| json!(url))
        })
        .collect();
    let mut merged = Map::new();
    for name in extensions {
        if let Some(Value::Object(terms)) = extension(name) {
            merged.extend(terms);
        }
    }
    if !merged.is_empty() {
        context.push(Value::Object(merged));
    }
    if context.len() == 1 {
        context.remove(0)
    } else {
        Value::Array(context)
    }
}

/// `ContextHelper#full_context`: every named context and every extension.
#[must_use]
pub fn full_context() -> Value {
    let named: Vec<&str> = NAMED_CONTEXTS.iter().map(|(name, _)| *name).collect();
    serialized_context(&named, &EXTENSIONS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_full_context_names_everything() {
        let context = full_context();
        let array = context.as_array().unwrap();
        assert_eq!(array.len(), 5);
        assert_eq!(array[3], "https://purl.archive.org/socialweb/webfinger");
        let terms = array[4].as_object().unwrap();
        assert_eq!(terms["toot"], "http://joinmastodon.org/ns#");
        assert!(!terms.contains_key("featuredCollections"));
        assert!(terms.contains_key("FeatureAuthorization"));
        assert!(terms.contains_key("atomUri"));
    }

    #[test]
    fn a_lone_context_is_not_an_array() {
        assert_eq!(
            serialized_context(&["activitystreams"], &[]),
            "https://www.w3.org/ns/activitystreams"
        );
    }
}
