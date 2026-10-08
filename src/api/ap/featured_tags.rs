//! An account's featured hashtags over ActivityPub: the collection its actor
//! names as `featuredTags` (`ActivityPub::CollectionsController` at
//! `collections/tags`), and the `Add` and `Remove` that tell its followers
//! when one is featured or no longer is (`ActivityPub::AddHashtagSerializer`,
//! `ActivityPub::RemoveHashtagSerializer`).

use serde_json::{json, Value};

use crate::error::AppResult;
use crate::state::AppState;

/// The `@context` of a document with hashtags in it: ActivityStreams, and
/// `Hashtag` (`context_extensions :hashtag`).
#[must_use]
pub fn context(has_hashtags: bool) -> Value {
    if has_hashtags {
        super::context_helper::serialized_context(&["activitystreams"], &["hashtag"])
    } else {
        super::context_helper::serialized_context(&["activitystreams"], &[])
    }
}

/// A featured hashtag as `ActivityPub::HashtagSerializer` writes one: its
/// `href` the account's posts with the tag, its `name` as it was featured,
/// or else as the tag is displayed.
#[must_use]
pub fn hashtag(domain: &str, username: &str, tag_name: &str, display_name: &str) -> Value {
    json!({
        "type": "Hashtag",
        // `short_account_tag_url(account, tag)`.
        "href": format!("https://{domain}/@{username}/tagged/{tag_name}"),
        "name": format!("#{display_name}"),
    })
}

/// A featured tag's name as it is displayed (`FeaturedTag#display_name`):
/// as it was featured, or else the tag's.
#[must_use]
pub fn display_name(featured: Option<&str>, tag_display: Option<&str>, tag_name: &str) -> String {
    featured.or(tag_display).unwrap_or(tag_name).to_owned()
}

/// The hashtags `account_id` features, in the order they were featured.
pub async fn hashtags(
    state: &AppState,
    domain: &str,
    account_id: i64,
    username: &str,
) -> AppResult<Vec<Value>> {
    let rows = sqlx::query!(
        r#"SELECT ft.name AS featured, t.name, t.display_name
           FROM featured_tags ft JOIN tags t ON t.id = ft.tag_id
           WHERE ft.account_id = $1 ORDER BY ft.id"#,
        account_id,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let display = display_name(
                row.featured.as_deref(),
                row.display_name.as_deref(),
                &row.name,
            );
            hashtag(domain, username, &row.name, &display)
        })
        .collect())
}

/// `Add` (featured) or `Remove` (no longer featured) of a hashtag: the
/// actor's, with its featured posts as the target, since the URI of its
/// featured tags is not stored anywhere a receiver could match it (as the
/// serializers say), and no `id`.
#[must_use]
pub fn activity(add: bool, actor: &str, featured: &str, hashtag: Value) -> Value {
    json!({
        "@context": context(true),
        "type": if add { "Add" } else { "Remove" },
        "actor": actor,
        "target": featured,
        "object": hashtag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_hashtag_is_as_mastodon_serializes_it() {
        let tag = hashtag("a.test", "alice", "cats", "Cats");
        assert_eq!(
            activity(
                true,
                "https://a.test/users/alice",
                "https://a.test/users/alice/collections/featured",
                tag,
            ),
            json!({
                "@context": ["https://www.w3.org/ns/activitystreams", {"Hashtag": "as:Hashtag"}],
                "type": "Add",
                "actor": "https://a.test/users/alice",
                "target": "https://a.test/users/alice/collections/featured",
                "object": {
                    "type": "Hashtag",
                    "href": "https://a.test/@alice/tagged/cats",
                    "name": "#Cats",
                },
            })
        );
    }

    #[test]
    fn a_featured_tag_is_displayed_as_it_was_featured() {
        assert_eq!(display_name(Some("Cats"), Some("CATS"), "cats"), "Cats");
        assert_eq!(display_name(None, Some("CATS"), "cats"), "CATS");
        assert_eq!(display_name(None, None, "cats"), "cats");
    }
}
