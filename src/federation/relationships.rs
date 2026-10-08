//! The activities Mastodon writes about follows and blocks, with the ids its
//! serializers give them: `ActivityPub::FollowSerializer`,
//! `AcceptFollowSerializer`, `RejectFollowSerializer`,
//! `UndoFollowSerializer`, `BlockSerializer` and `UndoBlockSerializer`.
//!
//! A `Follow`, `FollowRequest` or `Block` row is identified by its `uri`
//! (`uri_for`, since each says it is not `local?`), which `set_uri` gives
//! every one made here ([`generate_uri`]). A row without one — made before
//! eunha set it, or a `Follow.new` that was never saved — falls back to its
//! actor's id with a fragment, as the serializers' `||` does. The `Accept`,
//! `Reject` and `Undo` around it are named after the row's database id.

use serde_json::Value;

/// `ActivityPub::TagManager#generate_uri_for`: `URI.join(root_url,
/// 'payloads', SecureRandom.uuid)`, which, since `'payloads'` has no
/// trailing slash, `URI.join` replaces with the UUID.
pub fn generate_uri(domain: &str) -> String {
    format!("https://{domain}/{}", uuid::Uuid::new_v4())
}

/// The row's database id as the serializers join it: nothing for a row
/// that was never saved.
fn id_part(id: Option<i64>) -> String {
    id.map(|id| id.to_string()).unwrap_or_default()
}

/// `ActivityPub::FollowSerializer#id`: the follow's (or request's) `uri`, or
/// else `{follower}#follows/{id}`.
pub fn follow_id(uri: Option<&str>, follower: &str, id: Option<i64>) -> String {
    match uri.filter(|uri| !uri.is_empty()) {
        Some(uri) => uri.to_owned(),
        None => format!("{follower}#follows/{}", id_part(id)),
    }
}

/// `ActivityPub::AcceptFollowSerializer`: `followee` accepts the follow (or
/// request) `id` of `follower`.
pub fn accept_follow(
    followee: &str,
    follower: &str,
    id: Option<i64>,
    uri: Option<&str>,
) -> anyhow::Result<Value> {
    crate::federation::activity::accept_follow(
        &format!("{followee}#accepts/follows/{}", id_part(id)),
        followee,
        &follow_id(uri, follower, id),
        follower,
        followee,
    )
}

/// `ActivityPub::RejectFollowSerializer`: `followee` rejects the follow (or
/// request) `id` of `follower`.
pub fn reject_follow(
    followee: &str,
    follower: &str,
    id: Option<i64>,
    uri: Option<&str>,
) -> anyhow::Result<Value> {
    crate::federation::activity::reject_follow(
        &format!("{followee}#rejects/follows/{}", id_part(id)),
        followee,
        &follow_id(uri, follower, id),
        follower,
        followee,
    )
}

/// `ActivityPub::UndoFollowSerializer`: `follower` takes back its follow (or
/// request) `id` of `followee`.
pub fn undo_follow(
    follower: &str,
    followee: &str,
    id: Option<i64>,
    uri: Option<&str>,
) -> anyhow::Result<Value> {
    crate::federation::activity::undo_follow(
        &format!("{follower}#follows/{}/undo", id_part(id)),
        follower,
        &follow_id(uri, follower, id),
        follower,
        followee,
    )
}

/// `ActivityPub::BlockSerializer#id`: the block's `uri`, or else
/// `{blocker}#blocks/{id}`.
pub fn block_id(uri: Option<&str>, blocker: &str, id: i64) -> String {
    match uri.filter(|uri| !uri.is_empty()) {
        Some(uri) => uri.to_owned(),
        None => format!("{blocker}#blocks/{id}"),
    }
}

/// `ActivityPub::BlockSerializer`: `blocker`'s block `id` of `blocked`.
pub fn block(blocker: &str, blocked: &str, id: i64, uri: Option<&str>) -> anyhow::Result<Value> {
    crate::federation::activity::block(&block_id(uri, blocker, id), blocker, blocked)
}

/// `ActivityPub::UndoBlockSerializer`: `blocker` takes back its block `id`
/// of `blocked`.
pub fn undo_block(
    blocker: &str,
    blocked: &str,
    id: i64,
    uri: Option<&str>,
) -> anyhow::Result<Value> {
    crate::federation::activity::undo_block(
        &format!("{blocker}#blocks/{id}/undo"),
        blocker,
        &block_id(uri, blocker, id),
        blocked,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_uris_are_a_uuid_under_the_root() {
        let uri = generate_uri("example.com");
        let uuid = uri.strip_prefix("https://example.com/").unwrap();
        assert!(uuid::Uuid::parse_str(uuid).is_ok(), "{uri}");
    }

    #[test]
    fn ids_follow_the_serializers() {
        let alice = "https://a.test/users/alice";
        let bob = "https://b.test/users/bob";
        let accept = accept_follow(alice, bob, Some(7), Some("https://b.test/f/1")).unwrap();
        assert_eq!(accept["id"], format!("{alice}#accepts/follows/7"));
        assert_eq!(accept["object"]["id"], "https://b.test/f/1");
        assert_eq!(accept["object"]["actor"], bob);
        assert_eq!(accept["object"]["object"], alice);

        let reject = reject_follow(alice, bob, Some(7), None).unwrap();
        assert_eq!(reject["id"], format!("{alice}#rejects/follows/7"));
        assert_eq!(reject["object"]["id"], format!("{bob}#follows/7"));

        let undo = undo_follow(alice, bob, Some(9), Some("https://a.test/x")).unwrap();
        assert_eq!(undo["id"], format!("{alice}#follows/9/undo"));
        assert_eq!(undo["object"]["id"], "https://a.test/x");
        assert_eq!(undo["object"]["object"], bob);

        // `FollowRequest.new`, which has neither a uri nor an id.
        let accept = accept_follow(alice, bob, None, None).unwrap();
        assert_eq!(accept["id"], format!("{alice}#accepts/follows/"));
        assert_eq!(accept["object"]["id"], format!("{bob}#follows/"));

        let block = block(alice, bob, 3, None).unwrap();
        assert_eq!(block["id"], format!("{alice}#blocks/3"));
        let undo = undo_block(alice, bob, 3, Some("https://a.test/b")).unwrap();
        assert_eq!(undo["id"], format!("{alice}#blocks/3/undo"));
        assert_eq!(undo["object"]["id"], "https://a.test/b");
    }
}
