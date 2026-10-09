//! ActivityPub URI construction for local accounts, mirroring Mastodon's
//! `ActivityPub::TagManager#uri_for`.
//!
//! A local actor uses one of two URL schemes depending on its `id_scheme`
//! (Mastodon enum: `username_ap_id = 0`, `numeric_ap_id = 1`, default numeric):
//!   * `username_ap_id` → `https://{domain}/users/{username}`
//!   * `numeric_ap_id`  → `https://{domain}/ap/users/{id}`
//!
//! The per-instance actor (id `-99`) is served at `https://{domain}/actor`.
//!
//! Every sub-resource (`/inbox`, `/outbox`, `/followers`, `/statuses/{id}`, …)
//! is just the actor URI with a suffix appended, in both schemes. The
//! human-facing `url` (`/@username`) is independent of the scheme.

use crate::db::models::Account;
use crate::federation::instance_actor::INSTANCE_ACTOR_ID;

/// `accounts.id_scheme` value for Mastodon's `numeric_ap_id` scheme.
pub const NUMERIC_AP_ID: i32 = 1;

/// The canonical ActivityPub actor URI for a local account.
pub fn account_uri(domain: &str, id: i64, id_scheme: Option<i32>, username: &str) -> String {
    if id == INSTANCE_ACTOR_ID {
        format!("https://{domain}/actor")
    } else if id_scheme == Some(NUMERIC_AP_ID) {
        format!("https://{domain}/ap/users/{id}")
    } else {
        format!("https://{domain}/users/{username}")
    }
}

/// The actor URI for a loaded [`Account`].
pub fn account_uri_of(domain: &str, account: &Account) -> String {
    account_uri(domain, account.id, account.id_scheme, &account.username)
}

/// The account's `keyId` (its `publicKey` id), used to sign and verify requests.
pub fn key_id(domain: &str, id: i64, id_scheme: Option<i32>, username: &str) -> String {
    format!("{}#main-key", account_uri(domain, id, id_scheme, username))
}

/// The `keyId` for a loaded [`Account`].
pub fn key_id_of(domain: &str, account: &Account) -> String {
    key_id(domain, account.id, account.id_scheme, &account.username)
}

/// The canonical ActivityPub URI of a local status.
pub fn status_uri(
    domain: &str,
    account_id: i64,
    account_id_scheme: Option<i32>,
    account_username: &str,
    status_id: i64,
) -> String {
    format!(
        "{}/statuses/{}",
        account_uri(domain, account_id, account_id_scheme, account_username),
        status_id
    )
}

/// The id of a local status's activity, Mastodon's
/// `TagManager#activity_uri_for`: the status URI with `/activity`. It is also
/// a boost's own URI (`uri_for` a reblog), so a boost's `Announce`, the `Undo`
/// of it and its `statuses.uri` all name the same thing.
pub fn activity_uri(
    domain: &str,
    account_id: i64,
    account_id_scheme: Option<i32>,
    account_username: &str,
    status_id: i64,
) -> String {
    format!(
        "{}/activity",
        status_uri(
            domain,
            account_id,
            account_id_scheme,
            account_username,
            status_id
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_scheme() {
        assert_eq!(
            account_uri("seoul.earth", 5, Some(0), "alice"),
            "https://seoul.earth/users/alice"
        );
        // None (unset) is treated as username scheme, matching eunha-native accounts.
        assert_eq!(
            account_uri("seoul.earth", 5, None, "alice"),
            "https://seoul.earth/users/alice"
        );
    }

    #[test]
    fn numeric_scheme() {
        assert_eq!(
            account_uri("seoul.earth", 42, Some(1), "alice"),
            "https://seoul.earth/ap/users/42"
        );
        assert_eq!(
            key_id("seoul.earth", 42, Some(1), "alice"),
            "https://seoul.earth/ap/users/42#main-key"
        );
        assert_eq!(
            status_uri("seoul.earth", 42, Some(1), "alice", 99),
            "https://seoul.earth/ap/users/42/statuses/99"
        );
        assert_eq!(
            activity_uri("seoul.earth", 42, Some(1), "alice", 99),
            "https://seoul.earth/ap/users/42/statuses/99/activity"
        );
        assert_eq!(
            activity_uri("seoul.earth", 42, Some(0), "alice", 99),
            "https://seoul.earth/users/alice/statuses/99/activity"
        );
    }

    #[test]
    fn instance_actor() {
        assert_eq!(
            account_uri("seoul.earth", INSTANCE_ACTOR_ID, Some(1), "seoul.earth"),
            "https://seoul.earth/actor"
        );
    }
}
