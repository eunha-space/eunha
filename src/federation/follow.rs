//! What eunha does with an inbound Follow, as Mastodon does it, expressed as
//! `Input -> Actions`.
//!
//! These functions contain no IO: given the parsed activity plus the small bits
//! of context the host already knows (e.g. whether the target account is
//! locked), they return the [`Action`]s the host should carry out against its
//! own storage and delivery. Whether to accept a follow is eunha's policy, not
//! the protocol's, so it lives here rather than in Ojak.

use ojak_vocab::{Accept, AnyActor, AnyObject, Follow, Iri};

/// An effect the host should perform in response to an inbound activity.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Persist an accepted follower relationship.
    RecordFollow,
    /// Persist a pending follow request (the target account is locked).
    RecordFollowRequest,
    /// Deliver this `Accept` activity to the follower's inbox.
    SendAccept(Box<Accept>),
}

/// Decide how to handle an inbound `Follow` addressed to `local_actor`.
///
/// - Returns no actions if the follow targets someone other than `local_actor`.
/// - A locked account yields a single [`Action::RecordFollowRequest`].
/// - Otherwise the follow is accepted: [`Action::RecordFollow`] plus an
///   [`Action::SendAccept`] carrying the `Accept` to deliver, which embeds the
///   `Follow` so the follower recognises what was accepted.
#[must_use]
pub fn on_follow(follow: Follow, local_actor: &Iri, locked: bool, accept_id: Iri) -> Vec<Action> {
    if follow.objects.first().and_then(AnyObject::id) != Some(local_actor) {
        return Vec::new();
    }
    if locked {
        return vec![Action::RecordFollowRequest];
    }
    let accept = Accept {
        id: Some(accept_id),
        actors: vec![AnyActor::Iri(local_actor.clone())],
        objects: vec![AnyObject::Follow(Box::new(follow))],
        ..Accept::default()
    };
    vec![Action::RecordFollow, Action::SendAccept(Box::new(accept))]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn iri(s: &str) -> Iri {
        s.parse().expect("valid IRI")
    }

    fn follow_to(target: &str) -> Follow {
        Follow {
            id: Some(iri("https://remote.test/users/bob/follows/1")),
            actors: vec![AnyActor::Iri(iri("https://remote.test/users/bob"))],
            objects: vec![AnyObject::Iri(iri(target))],
            ..Follow::default()
        }
    }

    #[test]
    fn unlocked_follow_accepts_and_sends() {
        let me = iri("https://a.test/users/alice");
        let follow = follow_to("https://a.test/users/alice");
        let actions = on_follow(follow.clone(), &me, false, iri("https://a.test/accepts/1"));
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0], Action::RecordFollow);
        let Action::SendAccept(accept) = &actions[1] else {
            panic!("expected SendAccept, got {:?}", actions[1]);
        };
        assert_eq!(accept.actors, vec![AnyActor::Iri(me)]);
        assert_eq!(
            accept.objects,
            vec![AnyObject::Follow(Box::new(follow))],
            "the Follow is embedded"
        );
        // Written as a delivery, the embedded Follow carries no @context of
        // its own; the Accept's covers it.
        let written = ojak_vocab::write(&**accept);
        assert!(written["@context"].is_array());
        assert!(written["object"].get("@context").is_none());
        assert_eq!(written["object"]["type"], "Follow");
    }

    #[test]
    fn locked_follow_is_pending() {
        let me = iri("https://a.test/users/alice");
        let actions = on_follow(
            follow_to("https://a.test/users/alice"),
            &me,
            true,
            iri("https://a.test/accepts/1"),
        );
        assert_eq!(actions, vec![Action::RecordFollowRequest]);
    }

    #[test]
    fn follow_for_someone_else_is_ignored() {
        let me = iri("https://a.test/users/alice");
        let actions = on_follow(
            follow_to("https://a.test/users/carol"),
            &me,
            false,
            iri("https://a.test/accepts/1"),
        );
        assert!(actions.is_empty());
    }
}
