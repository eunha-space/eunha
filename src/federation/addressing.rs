//! Audience ⇄ visibility mapping for ActivityPub objects.
//!
//! Pure logic with no IO: derive an object's visibility from
//! its `to`/`cc` audience, and compute the `to`/`cc` audience for a given
//! visibility. Follows the common fediverse convention (as used by Mastodon)
//! where the public collection and the actor's `followers` collection encode
//! the four visibility levels.

use ojak_vocab::ACTIVITYSTREAMS_PUBLIC;

/// Suffix identifying an actor's `followers` collection in an audience URI.
const FOLLOWERS_SUFFIX: &str = "/followers";

/// How widely an object is addressed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Visibility {
    /// Addressed to the public collection in `to` — listed publicly.
    #[default]
    Public,
    /// Public collection only in `cc` — public but unlisted.
    Unlisted,
    /// Addressed to a `followers` collection — followers only.
    Private,
    /// Addressed only to specific actors.
    Direct,
}

/// Derive an object's [`Visibility`] from its `to` / `cc` audience.
#[must_use]
pub fn visibility_from_audience<S, T>(to: &[S], cc: &[T]) -> Visibility
where
    S: AsRef<str>,
    T: AsRef<str>,
{
    let is_public = |u: &str| u == ACTIVITYSTREAMS_PUBLIC;
    let is_followers = |u: &str| u.ends_with(FOLLOWERS_SUFFIX);

    if to.iter().any(|u| is_public(u.as_ref())) {
        Visibility::Public
    } else if cc.iter().any(|u| is_public(u.as_ref())) {
        Visibility::Unlisted
    } else if to.iter().any(|u| is_followers(u.as_ref()))
        || cc.iter().any(|u| is_followers(u.as_ref()))
    {
        Visibility::Private
    } else {
        Visibility::Direct
    }
}

/// Compute the `(to, cc)` audience for an outgoing object of the given
/// visibility, addressed to the actor's `followers` collection plus any
/// explicitly `mentioned` recipient URIs.
#[must_use]
pub fn audience_for<S: AsRef<str>>(
    visibility: Visibility,
    followers: &str,
    mentioned: &[S],
) -> (Vec<String>, Vec<String>) {
    let public = ACTIVITYSTREAMS_PUBLIC.to_string();
    let followers = followers.to_string();
    let mentions = || mentioned.iter().map(|m| m.as_ref().to_string());

    match visibility {
        Visibility::Public => {
            let mut cc = Vec::with_capacity(mentioned.len() + 1);
            cc.push(followers);
            cc.extend(mentions());
            (vec![public], cc)
        }
        Visibility::Unlisted => {
            let mut cc = Vec::with_capacity(mentioned.len() + 1);
            cc.push(public);
            cc.extend(mentions());
            (vec![followers], cc)
        }
        Visibility::Private => (vec![followers], mentions().collect()),
        Visibility::Direct => (mentions().collect(), Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PUBLIC: &str = ACTIVITYSTREAMS_PUBLIC;
    const FOLLOWERS: &str = "https://a.test/users/alice/followers";

    #[test]
    fn derives_each_visibility() {
        assert_eq!(
            visibility_from_audience(&[PUBLIC], &[FOLLOWERS]),
            Visibility::Public
        );
        assert_eq!(
            visibility_from_audience(&[FOLLOWERS], &[PUBLIC]),
            Visibility::Unlisted
        );
        assert_eq!(
            visibility_from_audience(&[FOLLOWERS], &["https://a.test/users/bob"]),
            Visibility::Private
        );
        assert_eq!(
            visibility_from_audience(&["https://a.test/users/bob"], &[] as &[&str]),
            Visibility::Direct
        );
    }

    #[test]
    fn audience_round_trips_through_visibility() {
        let mentions = vec!["https://b.test/users/bob".to_string()];
        for vis in [
            Visibility::Public,
            Visibility::Unlisted,
            Visibility::Private,
            Visibility::Direct,
        ] {
            let (to, cc) = audience_for(vis, FOLLOWERS, &mentions);
            assert_eq!(
                visibility_from_audience(&to, &cc),
                vis,
                "round-trip {vis:?}"
            );
        }
    }

    #[test]
    fn audience_shapes_match_mastodon() {
        let mentions = vec!["https://b.test/users/bob".to_string()];
        assert_eq!(
            audience_for(Visibility::Public, FOLLOWERS, &mentions),
            (
                vec![PUBLIC.to_string()],
                vec![FOLLOWERS.to_string(), mentions[0].clone()]
            )
        );
        assert_eq!(
            audience_for(Visibility::Unlisted, FOLLOWERS, &mentions),
            (
                vec![FOLLOWERS.to_string()],
                vec![PUBLIC.to_string(), mentions[0].clone()]
            )
        );
        assert_eq!(
            audience_for(Visibility::Private, FOLLOWERS, &mentions),
            (vec![FOLLOWERS.to_string()], vec![mentions[0].clone()])
        );
        assert_eq!(
            audience_for(Visibility::Direct, FOLLOWERS, &mentions),
            (vec![mentions[0].clone()], Vec::<String>::new())
        );
    }
}
