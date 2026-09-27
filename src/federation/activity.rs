//! ActivityPub activity construction.
//!
//! Builders over feder's generated vocabulary. Each takes string URIs (as the
//! rest of eunha stores them), builds the typed value, and writes it as a
//! [`serde_json::Value`] ready for delivery, under the plain ActivityStreams
//! context Mastodon uses. Mastodon/Misskey-specific JSON-LD (FEP-044f quote
//! terms) lives in `consent` rather than in the shared vocabulary crate.

use feder_vocab as vocab;
use feder_vocab::json::ToJson;
use serde_json::{json, Map, Value};
use vocab::{AnyActor, AnyObject, Iri};

/// The special collection addressing every actor (public posts).
pub const AS_PUBLIC: &str = vocab::ACTIVITYSTREAMS_PUBLIC;

fn iri(s: &str) -> anyhow::Result<Iri> {
    s.parse::<Iri>()
        .map_err(|e| anyhow::anyhow!("invalid ActivityPub IRI {s:?}: {e}"))
}

fn object_iri(s: &str) -> anyhow::Result<AnyObject> {
    iri(s).map(AnyObject::Iri)
}

fn objects(values: &[&str]) -> anyhow::Result<Vec<AnyObject>> {
    values.iter().map(|s| object_iri(s)).collect()
}

fn actor(s: &str) -> anyhow::Result<Vec<AnyActor>> {
    Ok(vec![AnyActor::Iri(iri(s)?)])
}

/// `value` written with `context` as its `@context`, first. An object nested
/// in another is written without one: the outer document's covers it.
pub(crate) fn with_context(value: &impl ToJson, context: Value) -> Value {
    let mut document = Map::new();
    document.insert("@context".into(), context);
    if let Value::Object(members) = value.to_json() {
        document.extend(members);
    }
    Value::Object(document)
}

/// `value` as a delivery, under the ActivityStreams context.
pub(crate) fn document(value: &impl ToJson) -> Value {
    with_context(value, json!(vocab::ACTIVITYSTREAMS_CONTEXT))
}

// ── Follow ──────────────────────────────────────────────────────────────────

fn build_follow(id: &str, actor_uri: &str, object: &str) -> anyhow::Result<vocab::Follow> {
    Ok(vocab::Follow {
        id: Some(iri(id)?),
        actors: actor(actor_uri)?,
        objects: vec![object_iri(object)?],
        ..Default::default()
    })
}

/// Build a `Follow` activity.
pub fn follow(id: &str, actor: &str, object: &str) -> anyhow::Result<Value> {
    Ok(document(&build_follow(id, actor, object)?))
}

// ── Accept / Reject ───────────────────────────────────────────────────────────

/// Build an `Accept(Follow)` activity sent in response to a received follow.
pub fn accept_follow(
    id: &str,
    actor_uri: &str,
    follow_id: &str,
    follow_actor: &str,
    follow_object: &str,
) -> anyhow::Result<Value> {
    let follow = build_follow(follow_id, follow_actor, follow_object)?;
    Ok(document(&vocab::Accept {
        id: Some(iri(id)?),
        actors: actor(actor_uri)?,
        objects: vec![AnyObject::Follow(Box::new(follow))],
        ..Default::default()
    }))
}

/// Build a `Reject(Follow)` activity.
pub fn reject_follow(
    id: &str,
    actor_uri: &str,
    follow_id: &str,
    follow_actor: &str,
    follow_object: &str,
) -> anyhow::Result<Value> {
    let follow = build_follow(follow_id, follow_actor, follow_object)?;
    Ok(document(&vocab::Reject {
        id: Some(iri(id)?),
        actors: actor(actor_uri)?,
        objects: vec![AnyObject::Follow(Box::new(follow))],
        ..Default::default()
    }))
}

// ── Undo ──────────────────────────────────────────────────────────────────────

fn undo(id: &str, actor_uri: &str, undone: AnyObject) -> anyhow::Result<Value> {
    Ok(document(&vocab::Undo {
        id: Some(iri(id)?),
        actors: actor(actor_uri)?,
        objects: vec![undone],
        ..Default::default()
    }))
}

/// Build an `Undo(Follow)` activity used when unfollowing.
pub fn undo_follow(
    id: &str,
    actor: &str,
    follow_id: &str,
    follow_actor: &str,
    follow_object: &str,
) -> anyhow::Result<Value> {
    let follow = build_follow(follow_id, follow_actor, follow_object)?;
    undo(id, actor, AnyObject::Follow(Box::new(follow)))
}

/// Build an `Undo(Like)` activity (unfavourite).
pub fn undo_like(
    id: &str,
    actor_uri: &str,
    like_id: &str,
    like_object: &str,
) -> anyhow::Result<Value> {
    let like = vocab::Like {
        id: Some(iri(like_id)?),
        actors: actor(actor_uri)?,
        objects: vec![object_iri(like_object)?],
        ..Default::default()
    };
    undo(id, actor_uri, AnyObject::Like(Box::new(like)))
}

/// Build an `Undo(Announce)` activity (unboost).
pub fn undo_announce(
    id: &str,
    actor_uri: &str,
    announce_id: &str,
    announce_object: &str,
) -> anyhow::Result<Value> {
    let announce = vocab::Announce {
        id: Some(iri(announce_id)?),
        actors: actor(actor_uri)?,
        objects: vec![object_iri(announce_object)?],
        ..Default::default()
    };
    undo(id, actor_uri, AnyObject::Announce(Box::new(announce)))
}

/// Build an `Undo(Block)` activity.
pub fn undo_block(
    id: &str,
    actor_uri: &str,
    block_id: &str,
    block_object: &str,
) -> anyhow::Result<Value> {
    let block = vocab::Block {
        id: Some(iri(block_id)?),
        actors: actor(actor_uri)?,
        objects: vec![object_iri(block_object)?],
        ..Default::default()
    };
    undo(id, actor_uri, AnyObject::Block(Box::new(block)))
}

// ── Delete ────────────────────────────────────────────────────────────────────

/// Build a `Delete` activity for a local object being removed.
pub fn delete(id: &str, actor_uri: &str, object: &str) -> anyhow::Result<Value> {
    let tombstone = vocab::Tombstone {
        id: Some(iri(object)?),
        ..Default::default()
    };
    Ok(document(&vocab::Delete {
        id: Some(iri(id)?),
        actors: actor(actor_uri)?,
        objects: vec![AnyObject::Tombstone(Box::new(tombstone))],
        ..Default::default()
    }))
}

/// Build the `Delete` activity announcing that a local actor is gone, matching
/// Mastodon's `ActivityPub::DeleteActorSerializer`: the actor deletes itself,
/// addressed to the public collection.
pub fn delete_actor(actor: &str) -> Value {
    serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": format!("{actor}#delete"),
        "type": "Delete",
        "actor": actor,
        "to": [AS_PUBLIC],
        "object": actor,
    })
}

// ── Move ──────────────────────────────────────────────────────────────────────

/// Build a `Move` activity for account migration. `object` is the old (moving)
/// actor; `target` is the new account's actor URI.
pub fn move_actor(id: &str, actor: &str, object: &str, target: &str) -> Value {
    serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": id,
        "type": "Move",
        "actor": actor,
        "object": object,
        "target": target,
    })
}

/// Build an `Update(Person)` activity for local actor/profile changes.
/// The actor document goes out as it was built: read into the vocabulary
/// and written back, it would lose whatever the vocabulary does not carry.
pub fn update_actor(id: &str, actor_uri: &str, object: Value) -> anyhow::Result<Value> {
    let mut update = document(&vocab::Update {
        id: Some(iri(id)?),
        actors: actor(actor_uri)?,
        tos: vec![object_iri(AS_PUBLIC)?],
        ..Default::default()
    });
    update["object"] = object;
    Ok(update)
}

// ── Like ──────────────────────────────────────────────────────────────────────

/// Build a `Like` activity (favourite).
pub fn like(id: &str, actor_uri: &str, object: &str) -> anyhow::Result<Value> {
    Ok(document(&vocab::Like {
        id: Some(iri(id)?),
        actors: actor(actor_uri)?,
        objects: vec![object_iri(object)?],
        ..Default::default()
    }))
}

// ── Announce ──────────────────────────────────────────────────────────────────

/// Build an `Announce` activity (boost/reblog).
pub fn announce(
    id: &str,
    actor_uri: &str,
    object: &str,
    to: &[&str],
    cc: &[&str],
    published: &str,
) -> anyhow::Result<Value> {
    Ok(document(&vocab::Announce {
        id: Some(iri(id)?),
        actors: actor(actor_uri)?,
        objects: vec![object_iri(object)?],
        published: Some(published.to_string()),
        tos: objects(to)?,
        ccs: objects(cc)?,
        ..Default::default()
    }))
}

// ── Block ─────────────────────────────────────────────────────────────────────

/// Build a `Block` activity.
pub fn block(id: &str, actor_uri: &str, object: &str) -> anyhow::Result<Value> {
    Ok(document(&vocab::Block {
        id: Some(iri(id)?),
        actors: actor(actor_uri)?,
        objects: vec![object_iri(object)?],
        ..Default::default()
    }))
}

/// `Add` a status to the actor's featured (pinned) collection
/// (Mastodon `ActivityPub::AddNoteSerializer`).
pub fn add_to_collection(
    id: &str,
    actor: &str,
    object: &str,
    target: &str,
) -> anyhow::Result<Value> {
    Ok(serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": id,
        "type": "Add",
        "actor": actor,
        "object": object,
        "target": target,
    }))
}

/// `Remove` a status from the actor's featured (pinned) collection
/// (Mastodon `ActivityPub::RemoveNoteSerializer`).
pub fn remove_from_collection(
    id: &str,
    actor: &str,
    object: &str,
    target: &str,
) -> anyhow::Result<Value> {
    Ok(serde_json::json!({
        "@context": "https://www.w3.org/ns/activitystreams",
        "id": id,
        "type": "Remove",
        "actor": actor,
        "object": object,
        "target": target,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_shape() {
        assert_eq!(
            follow(
                "https://a.test/f/1",
                "https://a.test/u/alice",
                "https://b.test/u/bob"
            )
            .unwrap(),
            json!({
                "@context": "https://www.w3.org/ns/activitystreams",
                "id": "https://a.test/f/1",
                "type": "Follow",
                "actor": "https://a.test/u/alice",
                "object": "https://b.test/u/bob",
            })
        );
    }

    #[test]
    fn add_and_remove_collection_shape() {
        let target = "https://a.test/u/alice/collections/featured";
        assert_eq!(
            add_to_collection(
                "https://a.test/act/1",
                "https://a.test/u/alice",
                "https://a.test/s/5",
                target
            )
            .unwrap(),
            json!({
                "@context": "https://www.w3.org/ns/activitystreams",
                "id": "https://a.test/act/1",
                "type": "Add",
                "actor": "https://a.test/u/alice",
                "object": "https://a.test/s/5",
                "target": target,
            })
        );
        let remove = remove_from_collection(
            "https://a.test/act/2",
            "https://a.test/u/alice",
            "https://a.test/s/5",
            target,
        )
        .unwrap();
        assert_eq!(remove["type"], "Remove");
        assert_eq!(remove["target"], target);
        assert_eq!(remove["object"], "https://a.test/s/5");
    }

    #[test]
    fn accept_follow_embeds_follow_without_context() {
        let v = accept_follow(
            "https://a.test/acc/1",
            "https://a.test/u/alice",
            "https://b.test/f/9",
            "https://b.test/u/bob",
            "https://a.test/u/alice",
        )
        .unwrap();
        assert_eq!(v["type"], "Accept");
        assert_eq!(v["object"]["type"], "Follow");
        assert!(v["object"].get("@context").is_none());
        assert_eq!(v["object"]["id"], "https://b.test/f/9");
    }

    #[test]
    fn undo_like_embeds_like_without_context() {
        let v = undo_like(
            "https://a.test/l/1#undo",
            "https://a.test/u/alice",
            "https://a.test/l/1",
            "https://b.test/notes/9",
        )
        .unwrap();
        assert_eq!(v["type"], "Undo");
        assert_eq!(v["object"]["type"], "Like");
        assert!(v["object"].get("@context").is_none());
        assert_eq!(v["object"]["actor"], "https://a.test/u/alice");
        assert_eq!(v["object"]["object"], "https://b.test/notes/9");
    }

    #[test]
    fn delete_uses_tombstone() {
        let v = delete(
            "https://a.test/d/1",
            "https://a.test/u/alice",
            "https://a.test/notes/1",
        )
        .unwrap();
        assert_eq!(v["type"], "Delete");
        assert_eq!(
            v["object"],
            json!({ "type": "Tombstone", "id": "https://a.test/notes/1" })
        );
    }

    #[test]
    fn update_actor_embeds_public_actor_object() {
        let v = update_actor(
            "https://a.test/u/alice#updates/1",
            "https://a.test/u/alice",
            json!({
                "id": "https://a.test/u/alice",
                "type": "Person",
                "inbox": "https://a.test/u/alice/inbox",
            }),
        )
        .unwrap();
        assert_eq!(v["type"], "Update");
        assert_eq!(v["to"], AS_PUBLIC);
        assert_eq!(v["actor"], "https://a.test/u/alice");
        assert_eq!(v["object"]["type"], "Person");
        assert_eq!(v["object"]["id"], "https://a.test/u/alice");
    }

    #[test]
    fn announce_has_audience() {
        let v = announce(
            "https://a.test/b/1",
            "https://a.test/u/alice",
            "https://b.test/notes/9",
            &[AS_PUBLIC],
            &["https://a.test/u/alice/followers"],
            "2026-06-21T00:00:00+00:00",
        )
        .unwrap();
        assert_eq!(v["type"], "Announce");
        // One value is written as itself, which JSON-LD reads as a list of one.
        assert_eq!(v["to"], AS_PUBLIC);
        assert_eq!(v["cc"], "https://a.test/u/alice/followers");
        assert_eq!(v["published"], "2026-06-21T00:00:00+00:00");
        assert_eq!(v["object"], "https://b.test/notes/9");
    }
}
