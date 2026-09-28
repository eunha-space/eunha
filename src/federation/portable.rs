//! Portable actors and objects (FEP-ef61), as eunha meets them: accounts and
//! statuses on other servers whose ids are `ap://did:key:…` rather than
//! `https://…`.
//!
//! A portable id names a key, not a host, so nothing about it says where to
//! send a request. Ojak fetches it from the gateways it hints at and keeps
//! only a copy whose proof by the key holds. Eunha stores it by its
//! canonical id, gives it the first gateway's host as its domain, which is
//! also where its WebFinger address lives, and keeps its inbox as the
//! `https` URL at that gateway, so that delivering to it is delivering to
//! any other inbox.

use ojak::origin::Origin;
use ojak::portable::ApUri;
use serde_json::Value;

/// `uri` in the spelling it is stored and compared in: a portable id's
/// canonical form, without location hints; anything else as it is.
pub fn canonical(uri: &str) -> String {
    match ApUri::parse(uri) {
        Some(portable) => portable.canonical(),
        None => uri.to_owned(),
    }
}

/// `uri` as an IRI of ojak's vocabulary. A portable id's canonical form
/// has colons in its authority, which no IRI parser accepts; it is held with
/// them percent-encoded, which is the same id, and written back canonical.
pub fn iri(uri: &str) -> anyhow::Result<ojak_vocab::Iri> {
    if let Ok(iri) = uri.parse() {
        return Ok(iri);
    }
    ApUri::parse(uri)
        .and_then(|portable| portable.encoded().parse().ok())
        .ok_or_else(|| anyhow::anyhow!("invalid ActivityPub IRI {uri:?}"))
}

/// Whether two ids have the same authority: the same DID for portable ids,
/// which is what vouches for them, and the same host for anything else, as
/// Mastodon compares them.
pub fn same_authority(a: &str, b: &str) -> bool {
    match (Origin::of(a), Origin::of(b)) {
        (Some(Origin::Did(a)), Some(Origin::Did(b))) => a == b,
        (Some(Origin::Did(_)), _) | (_, Some(Origin::Did(_))) => false,
        _ => match (url::Url::parse(a), url::Url::parse(b)) {
            (Ok(a), Ok(b)) => {
                matches!(a.scheme(), "http" | "https")
                    && matches!(b.scheme(), "http" | "https")
                    && a.host_str().map(str::to_ascii_lowercase)
                        == b.host_str().map(str::to_ascii_lowercase)
            }
            _ => false,
        },
    }
}

/// Where a portable actor is reached, from its document: the first gateway
/// it lists, and its inbox, outbox and shared inbox at that gateway.
#[derive(Debug, PartialEq)]
pub struct Reach {
    /// The first gateway's host, stored as the account's domain.
    pub domain: String,
    pub inbox: String,
    pub outbox: String,
    pub shared_inbox: String,
}

/// How to reach the portable actor `actor`, or `None` when it is not one or
/// lists no gateway.
pub fn reach(actor: &Value) -> Option<Reach> {
    let id = actor.get("id").and_then(Value::as_str)?;
    ApUri::parse(id)?;
    let gateways = ojak::portable::gateways(actor);
    let gateway = gateways.first()?;
    let domain = url::Url::parse(gateway).ok()?.host_str()?.to_owned();
    let at_gateway = |key: &str| {
        let value = actor.get(key).and_then(Value::as_str).unwrap_or_default();
        match ApUri::parse(value) {
            Some(uri) => uri.at_gateway(gateway),
            None => value.to_owned(),
        }
    };
    let shared = actor
        .get("endpoints")
        .and_then(|endpoints| endpoints.get("sharedInbox"))
        .or_else(|| actor.get("sharedInbox"))
        .and_then(Value::as_str)
        .map(|shared| match ApUri::parse(shared) {
            Some(uri) => uri.at_gateway(gateway),
            None => shared.to_owned(),
        })
        .unwrap_or_default();
    Some(Reach {
        domain,
        inbox: at_gateway("inbox"),
        outbox: at_gateway("outbox"),
        shared_inbox: shared,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DID: &str = "did:key:z6MkrJVnaZkeFzdQyMZu1cgjg7k1pZZ6pvBQ7XJPt4swbTQ2";

    #[test]
    fn a_portable_id_is_stored_canonical() {
        assert_eq!(
            canonical(&format!(
                "ap://{DID}/actor?@gateway=https%3A%2F%2Fserver1.example"
            )),
            format!("ap://{DID}/actor")
        );
        assert_eq!(
            canonical(&format!(
                "https://g.example/.well-known/apgateway/{DID}/actor"
            )),
            format!("ap://{DID}/actor")
        );
        assert_eq!(
            canonical("https://a.example/users/alice"),
            "https://a.example/users/alice"
        );
    }

    #[test]
    fn a_portable_id_is_an_iri_and_writes_back_canonical() {
        use ojak_vocab::json::ToJson as _;

        let id = format!("ap://{DID}/actor");
        assert_eq!(iri(&id).unwrap().to_json(), json!(id));
        assert!(iri("not an iri").is_err());
    }

    #[test]
    fn authority_is_the_did_or_the_host() {
        assert!(same_authority(
            &format!("ap://{DID}/objects/1"),
            &format!("ap://{DID}/actor")
        ));
        assert!(!same_authority(
            &format!("ap://{DID}/objects/1"),
            "ap://did:key:z6MkOther/actor"
        ));
        assert!(!same_authority(
            &format!("ap://{DID}/objects/1"),
            "https://g.example/users/x"
        ));
        assert!(same_authority(
            "https://A.example/notes/1",
            "https://a.example/users/alice"
        ));
        assert!(!same_authority("https://a.example/", "https://b.example/"));
    }

    #[test]
    fn a_portable_actor_is_reached_at_its_first_gateway() {
        let actor = json!({
            "id": format!("ap://{DID}/actor"),
            "type": "Person",
            "inbox": format!("ap://{DID}/actor/inbox"),
            "outbox": format!("ap://{DID}/actor/outbox"),
            "gateways": ["https://server1.example", "https://server2.example"],
        });
        assert_eq!(
            reach(&actor),
            Some(Reach {
                domain: "server1.example".into(),
                inbox: format!("https://server1.example/.well-known/apgateway/{DID}/actor/inbox"),
                outbox: format!("https://server1.example/.well-known/apgateway/{DID}/actor/outbox"),
                shared_inbox: String::new(),
            })
        );
        assert_eq!(
            reach(&json!({"id": "https://a.example/users/alice", "gateways": ["https://x"]})),
            None
        );
    }
}
