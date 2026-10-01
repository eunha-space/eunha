use chrono::NaiveDateTime;
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow)]
pub struct Account {
    pub id: i64,
    pub username: String,
    pub domain: Option<String>,
    pub display_name: String,
    pub note: String,
    pub url: Option<String>,
    /// Nullable since Mastodon 4.7.0: an account whose URI is not known, which
    /// for a local account means one whose actor id is derived rather than
    /// stored. Was the empty string before.
    pub uri: Option<String>,
    pub private_key: Option<String>,
    pub public_key: String,
    pub locked: bool,
    pub discoverable: Option<bool>,
    pub indexable: bool,
    pub inbox_url: String,
    pub outbox_url: String,
    pub shared_inbox_url: String,
    pub suspended_at: Option<NaiveDateTime>,
    pub silenced_at: Option<NaiveDateTime>,
    pub sensitized_at: Option<NaiveDateTime>,
    pub hide_collections: Option<bool>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
    pub fields: Option<serde_json::Value>,
    pub attribution_domains: Option<Vec<String>>,
    pub actor_type: Option<String>,
    pub also_known_as: Option<Vec<String>>,
    pub featured_collection_url: Option<String>,
    pub followers_url: String,
    pub following_url: String,
    pub last_webfingered_at: Option<NaiveDateTime>,
    pub memorial: bool,
    pub moved_to_account_id: Option<i64>,
    pub protocol: i32,
    pub requested_review_at: Option<NaiveDateTime>,
    pub reviewed_at: Option<NaiveDateTime>,
    pub suspension_origin: Option<i32>,
    pub trendable: Option<bool>,
    pub id_scheme: Option<i32>,
    // Paperclip/ActiveStorage compat columns (added in migration 067)
    pub avatar_file_name: Option<String>,
    pub avatar_content_type: Option<String>,
    pub avatar_file_size: Option<i32>,
    pub avatar_updated_at: Option<NaiveDateTime>,
    pub header_file_name: Option<String>,
    pub header_content_type: Option<String>,
    pub header_file_size: Option<i32>,
    pub header_updated_at: Option<NaiveDateTime>,
    pub avatar_remote_url: Option<String>,
    pub header_remote_url: String,
    pub avatar_storage_schema_version: Option<i32>,
    pub header_storage_schema_version: Option<i32>,
    // Added in Mastodon v4.6.0
    pub avatar_description: String,
    pub header_description: String,
    pub show_featured: bool,
    pub show_media: bool,
    pub show_media_replies: bool,
    pub feature_approval_policy: i32,
    pub collections_url: Option<String>,
    // Added in Mastodon v4.7.0
    /// When the account's owner asked for it to be deleted. Distinct from
    /// `suspended_at`, which now only means a moderator suspension.
    pub requested_deletion_at: Option<NaiveDateTime>,
}

impl Account {
    pub fn is_local(&self) -> bool {
        self.domain.is_none()
    }

    /// Mastodon's `Account#pretty_acct`.
    ///
    /// An account whose handle cannot be verified shows neither the handle it
    /// claims nor the domain it claims it on: both are what could not be
    /// verified.
    pub fn acct(&self) -> String {
        if self.has_invalid_handle() {
            return format!("{}@handle.invalid", self.id);
        }
        match &self.domain {
            None => self.username.clone(),
            Some(d) => format!("{}@{}", self.username, d),
        }
    }

    /// The account's stored ActivityPub actor id, if it has one.
    ///
    /// Local accounts generally do not: Mastodon derives theirs from the id
    /// scheme rather than storing it. Absence was the empty string before
    /// Mastodon 4.7.0 and is NULL after it, so both are treated as absent.
    pub fn stored_uri(&self) -> Option<&str> {
        self.uri.as_deref().filter(|uri| !uri.is_empty())
    }

    /// Mastodon's `Account#unavailable?`: hidden from everyone, whether because
    /// a moderator suspended it or because its owner asked for it to be deleted.
    ///
    /// Before 4.7.0 a self-deleted account was recorded as suspended, so this
    /// was just `suspended_at`.
    pub fn is_unavailable(&self) -> bool {
        self.suspended_at.is_some() || self.requested_deletion_at.is_some()
    }

    /// Mastodon's `Account#deleted?`.
    pub fn is_deleted(&self) -> bool {
        self.requested_deletion_at.is_some()
    }

    /// Mastodon's `Account#invalidated_username?`.
    ///
    /// Since 4.7.0 an account whose handle cannot be verified keeps its actor
    /// id but has its username replaced with a marker no server could issue.
    /// Eunha never writes one, but a database imported from Mastodon can
    /// contain them, and they must not be shown as if they were real handles.
    pub fn has_invalid_handle(&self) -> bool {
        self.username.starts_with("! ")
    }

    /// Mastodon's `Account#pretty_username`.
    pub fn pretty_username(&self) -> String {
        if self.has_invalid_handle() {
            self.id.to_string()
        } else {
            self.username.clone()
        }
    }
}

#[derive(Debug, Clone, FromRow)]
pub struct User {
    pub id: i64,
    pub account_id: i64,
    pub email: String,
    pub encrypted_password: String,
    pub confirmed_at: Option<NaiveDateTime>,
    pub invite_id: Option<i64>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct Status {
    pub id: i64,
    pub account_id: i64,
    pub application_id: Option<i64>,
    pub text: String,
    pub spoiler_text: String,
    pub in_reply_to_id: Option<i64>,
    pub in_reply_to_account_id: Option<i64>,
    pub reblog_of_id: Option<i64>,
    pub visibility: i32,
    pub language: Option<String>,
    pub sensitive: bool,
    pub url: Option<String>,
    pub uri: Option<String>,
    pub deleted_at: Option<NaiveDateTime>,
    pub edited_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub reply: bool,
    pub conversation_id: Option<i64>,
    // Added in migration 065
    pub fetched_replies_at: Option<NaiveDateTime>,
    pub local: Option<bool>,
    pub ordered_media_attachment_ids: Option<Vec<i64>>,
    pub poll_id: Option<i64>,
    pub quote_approval_policy: i32,
    pub trendable: Option<bool>,
    pub updated_at: Option<NaiveDateTime>,
}

#[derive(Debug, Clone, FromRow)]
pub struct MediaAttachment {
    pub id: i64,
    pub account_id: Option<i64>,
    pub status_id: Option<i64>,
    pub remote_url: Option<String>,
    pub description: Option<String>,
    pub blurhash: Option<String>,
    pub created_at: NaiveDateTime,
    // Mastodon compat columns (added in migration 067)
    pub updated_at: NaiveDateTime,
    pub shortcode: Option<String>,
    pub r#type: Option<i32>,
    pub file_meta: Option<serde_json::Value>,
    pub scheduled_status_id: Option<i64>,
    pub processing: Option<i32>,
    pub file_storage_schema_version: Option<i32>,
    pub file_file_name: Option<String>,
    pub file_content_type: Option<String>,
    pub file_file_size: Option<i32>,
    pub file_updated_at: Option<NaiveDateTime>,
    pub thumbnail_file_name: Option<String>,
    pub thumbnail_content_type: Option<String>,
    pub thumbnail_file_size: Option<i32>,
    pub thumbnail_updated_at: Option<NaiveDateTime>,
    pub thumbnail_remote_url: Option<String>,
    // Added in Mastodon v4.6.0
    pub thumbnail_storage_schema_version: Option<i32>,
}

#[derive(Debug, Clone, FromRow)]
pub struct Follow {
    pub id: i64,
    pub account_id: i64,
    pub target_account_id: i64,
    pub show_reblogs: bool,
    pub notify: bool,
    pub languages: Vec<String>,
    pub uri: Option<String>,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct Notification {
    pub id: i64,
    pub account_id: i64,
    pub from_account_id: i64,
    pub r#type: Option<String>,
    pub created_at: NaiveDateTime,
    // Added in migration 065
    pub filtered: bool,
    pub group_key: Option<String>,
    // Mastodon polymorphic association columns
    pub activity_id: Option<i64>,
    pub activity_type: Option<String>,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct OauthApplication {
    pub id: i64,
    pub name: String,
    pub uid: String,
    pub secret: String,
    pub redirect_uri: String,
    pub scopes: Option<String>,
    pub website: Option<String>,
    pub created_at: Option<NaiveDateTime>,
    // Added in migration 065
    pub confidential: bool,
    pub superapp: bool,
    pub updated_at: Option<NaiveDateTime>,
    pub owner_type: Option<String>,
    pub owner_id: Option<i64>,
}

#[derive(Debug, Clone, FromRow)]
pub struct OauthAccessToken {
    pub id: i64,
    pub application_id: Option<i64>,
    pub resource_owner_id: Option<i64>,
    pub token: String,
    pub refresh_token: Option<String>,
    pub scopes: Option<String>,
    pub expires_in: Option<i32>,
    pub revoked_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub last_used_at: Option<NaiveDateTime>,
    pub last_used_ip: Option<std::net::IpAddr>,
}

#[derive(Debug, Clone, FromRow)]
pub struct Tag {
    pub id: i64,
    pub name: String,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct CustomEmoji {
    pub id: i64,
    pub shortcode: String,
    pub domain: Option<String>,
    pub image_remote_url: Option<String>,
    pub visible_in_picker: bool,
    pub disabled: bool,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct Favourite {
    pub id: i64,
    pub account_id: i64,
    pub status_id: i64,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct List {
    pub id: i64,
    pub account_id: i64,
    pub title: String,
    pub replies_policy: i32,
    pub exclusive: bool,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct StatusEdit {
    pub id: i64,
    pub status_id: i64,
    pub account_id: Option<i64>,
    pub text: String,
    pub spoiler_text: String,
    pub sensitive: Option<bool>,
    pub created_at: NaiveDateTime,
    // Added in migration 065. One per attachment in
    // `ordered_media_attachment_ids`, as it was described at that version;
    // Mastodon writes NULL for an attachment that had no description.
    pub media_descriptions: Option<Vec<Option<String>>>,
    pub ordered_media_attachment_ids: Option<Vec<i64>>,
    pub poll_options: Option<Vec<String>>,
    pub quote_id: Option<i64>,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct Poll {
    pub id: i64,
    pub status_id: i64,
    pub account_id: i64,
    pub options: Vec<String>,
    pub votes_count: i64,
    pub voters_count: Option<i64>,
    pub multiple: bool,
    pub expires_at: Option<NaiveDateTime>,
    pub created_at: NaiveDateTime,
    pub cached_tallies: Vec<i64>,
    pub hide_totals: bool,
    pub last_fetched_at: Option<NaiveDateTime>,
    pub lock_version: i32,
    pub updated_at: NaiveDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct UserDomainBlock {
    pub id: i64,
    pub account_id: i64,
    pub domain: String,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, FromRow)]
pub struct WebPushSubscription {
    pub id: i64,
    pub account_id: i64,
    pub access_token_id: i64,
    pub endpoint: String,
    pub key_p256dh: String,
    pub key_auth: String,
    pub data: serde_json::Value,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

/// Integer-to-text helpers for statuses.visibility (public=0 unlisted=1 private=2 direct=3 limited=4).
pub mod vis {
    pub const PUBLIC: i32 = 0;
    pub const UNLISTED: i32 = 1;
    pub const PRIVATE: i32 = 2;
    pub const DIRECT: i32 = 3;
    /// Limited (group-delivery) visibility — serialized as "private" per Mastodon API contract.
    pub const LIMITED: i32 = 4;

    pub fn from_str(s: &str) -> i32 {
        match s {
            "public" => PUBLIC,
            "unlisted" => UNLISTED,
            "private" => PRIVATE,
            _ => DIRECT,
        }
    }

    pub fn to_str(v: i32) -> &'static str {
        // Mastodon masks "limited" (4) as "private" so clients don't need to handle it.
        match v {
            PUBLIC => "public",
            UNLISTED => "unlisted",
            PRIVATE | LIMITED => "private",
            _ => "direct",
        }
    }

    /// Whether a status counts towards its author's public totals.
    ///
    /// Mastodon's `increment_counter_caches` opens `return if
    /// direct_visibility?`, so a direct message moves no counter at all. The
    /// post count sits on a profile anyone can read, and one that climbed
    /// whenever an account sent a private message would report that it had.
    ///
    /// `limited` is not `direct` and does count, matching upstream.
    #[must_use]
    pub fn counted(v: i32) -> bool {
        v != DIRECT
    }

    /// Mastodon's `Status#distributable?`: a status everyone can see.
    ///
    /// It decides what may be counted in public. `replies_count` is shown to
    /// anyone who can see the parent, so counting a followers-only or direct
    /// reply would announce that a private reply exists — which is what
    /// choosing that visibility was meant to avoid.
    #[must_use]
    pub fn distributable(v: i32) -> bool {
        matches!(v, PUBLIC | UNLISTED)
    }

    /// Derive visibility from an object's `to`/`cc` audience (federation::addressing).
    pub fn from_audience<S: AsRef<str>, T: AsRef<str>>(to: &[S], cc: &[T]) -> i32 {
        use crate::federation::addressing::Visibility;
        match crate::federation::addressing::visibility_from_audience(to, cc) {
            Visibility::Public => PUBLIC,
            Visibility::Unlisted => UNLISTED,
            Visibility::Private => PRIVATE,
            Visibility::Direct => DIRECT,
        }
    }

    /// Compute the `(to, cc)` audience for an outgoing status of this visibility.
    pub fn audience(v: i32, followers: &str, mentioned: &[String]) -> (Vec<String>, Vec<String>) {
        use crate::federation::addressing::Visibility;
        let vis = match v {
            PUBLIC => Visibility::Public,
            UNLISTED => Visibility::Unlisted,
            PRIVATE => Visibility::Private,
            // Mastodon's TagManager serializes "limited" like "direct": the
            // recipients go in `to` and `cc` stays empty.
            LIMITED => return (mentioned.to_vec(), Vec::new()),
            _ => Visibility::Direct,
        };
        crate::federation::addressing::audience_for(vis, followers, mentioned)
    }
}

/// Integer-to-text helpers for lists.replies_policy (followed=0 list=1 none=2).
pub mod replies {
    pub const FOLLOWED: i32 = 0;
    pub const LIST: i32 = 1;
    pub const NONE: i32 = 2;

    pub fn from_str(s: &str) -> i32 {
        match s {
            "followed" => FOLLOWED,
            "list" => LIST,
            _ => NONE,
        }
    }

    pub fn to_str(v: i32) -> &'static str {
        match v {
            FOLLOWED => "followed",
            LIST => "list",
            _ => "none",
        }
    }
}

/// Integer-to-text helpers for quotes.state (pending=0 accepted=1 rejected=2 revoked=3).
pub mod quote_state {
    pub const PENDING: i32 = 0;
    pub const ACCEPTED: i32 = 1;
    pub const REJECTED: i32 = 2;
    pub const REVOKED: i32 = 3;

    pub fn to_str(v: i32) -> &'static str {
        match v {
            ACCEPTED => "accepted",
            REJECTED => "rejected",
            REVOKED => "revoked",
            _ => "pending",
        }
    }
}

/// `statuses.quote_approval_policy`: Mastodon's `InteractionPolicy` bitmap,
/// the automatic sub-policy in the high 16 bits and the manual one in the low
/// 16, each a set of `POLICY_FLAGS`.
pub mod quote_policy {
    pub const UNSUPPORTED_POLICY: i32 = 1 << 0;
    pub const PUBLIC: i32 = 1 << 1;
    pub const FOLLOWERS: i32 = 1 << 2;
    pub const FOLLOWING: i32 = 1 << 3;
    pub const DISABLED: i32 = 1 << 4;

    /// `POLICY_FLAGS`, in order.
    const KEYS: &[(i32, &str)] = &[
        (UNSUPPORTED_POLICY, "unsupported_policy"),
        (PUBLIC, "public"),
        (FOLLOWERS, "followers"),
        (FOLLOWING, "following"),
        (DISABLED, "disabled"),
    ];

    /// `Api::InteractionPoliciesConcern#quote_approval_policy`: the API's
    /// `public`, `followers` and `nobody`, each an automatic policy.
    pub fn from_api(s: &str) -> Option<i32> {
        match s {
            "public" => Some(PUBLIC << 16),
            "followers" => Some(FOLLOWERS << 16),
            "nobody" => Some(0),
            _ => None,
        }
    }

    pub fn automatic(bitmap: i32) -> i32 {
        bitmap >> 16
    }

    pub fn manual(bitmap: i32) -> i32 {
        bitmap & 0xFFFF
    }

    /// `SubPolicy#as_keys`.
    pub fn as_keys(sub_policy: i32) -> Vec<&'static str> {
        KEYS.iter()
            .filter(|(flag, _)| sub_policy & flag != 0)
            .map(|(_, key)| *key)
            .collect()
    }

    /// `quote_policy_for_account`'s answer.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ForAccount {
        Automatic,
        Manual,
        Unknown,
        Denied,
    }

    impl ForAccount {
        pub fn as_str(self) -> &'static str {
            match self {
                ForAccount::Automatic => "automatic",
                ForAccount::Manual => "manual",
                ForAccount::Unknown => "unknown",
                ForAccount::Denied => "denied",
            }
        }
    }

    /// `Status#quote_policy_for_account`, given whether the other account
    /// follows the author and the author follows it.
    pub fn for_account(
        bitmap: i32,
        is_author: bool,
        follows_author: bool,
        followed_by_author: bool,
    ) -> ForAccount {
        if is_author {
            return ForAccount::Automatic;
        }
        let allows = |sub: i32| {
            sub & PUBLIC != 0
                || (sub & FOLLOWERS != 0 && follows_author)
                || (sub & FOLLOWING != 0 && followed_by_author)
        };
        let (auto, manual) = (automatic(bitmap), manual(bitmap));
        if allows(auto) {
            ForAccount::Automatic
        } else if allows(manual) {
            ForAccount::Manual
        } else if (auto | manual) & UNSUPPORTED_POLICY != 0 {
            ForAccount::Unknown
        } else {
            ForAccount::Denied
        }
    }

    /// `ActivityPub::Parser::StatusParser#quote_policy`: a post's
    /// `interactionPolicy.canQuote`, read against its author's collections.
    pub fn parse(object: &serde_json::Value, followers: &str, following: &str, actor: &str) -> i32 {
        let Some(policy) = object
            .get("interactionPolicy")
            .and_then(|p| p.get("canQuote"))
            .filter(|p| p.is_object())
        else {
            return 0;
        };
        let sub = |value: Option<&serde_json::Value>| -> i32 {
            let mut actors: Vec<String> = match value {
                Some(serde_json::Value::String(s)) => vec![s.clone()],
                Some(serde_json::Value::Array(a)) => a
                    .iter()
                    .filter_map(|v| match v {
                        serde_json::Value::String(s) => Some(s.clone()),
                        serde_json::Value::Object(o) => {
                            o.get("id").and_then(|i| i.as_str()).map(str::to_owned)
                        }
                        _ => None,
                    })
                    .collect(),
                _ => vec![],
            };
            actors.sort();
            actors.dedup();
            let mut take = |candidates: &[&str]| {
                let before = actors.len();
                actors.retain(|a| !candidates.contains(&a.as_str()) || a.is_empty());
                actors.len() != before
            };
            let mut flags = 0;
            if take(&[
                "as:Public",
                "Public",
                "https://www.w3.org/ns/activitystreams#Public",
            ]) {
                flags |= PUBLIC;
            }
            if !followers.is_empty() && take(&[followers]) {
                flags |= FOLLOWERS;
            }
            if !following.is_empty() && take(&[following]) {
                flags |= FOLLOWING;
            }
            take(&[actor]);
            if !actors.is_empty() {
                flags |= UNSUPPORTED_POLICY;
            }
            flags
        };
        (sub(policy.get("automaticApproval")) << 16) | sub(policy.get("manualApproval"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_can_quote() {
            let object = serde_json::json!({"interactionPolicy": {"canQuote": {
                "automaticApproval": ["https://x.test/users/a/followers"],
                "manualApproval": "https://www.w3.org/ns/activitystreams#Public",
            }}});
            assert_eq!(
                parse(
                    &object,
                    "https://x.test/users/a/followers",
                    "",
                    "https://x.test/users/a"
                ),
                (FOLLOWERS << 16) | PUBLIC
            );
            let only_author = serde_json::json!({"interactionPolicy": {"canQuote": {
                "automaticApproval": ["https://x.test/users/a"]}}});
            assert_eq!(parse(&only_author, "", "", "https://x.test/users/a"), 0);
        }

        #[test]
        fn api_values_are_automatic_policies() {
            assert_eq!(from_api("public"), Some(131_072));
            assert_eq!(from_api("followers"), Some(262_144));
            assert_eq!(from_api("nobody"), Some(0));
            assert_eq!(as_keys(automatic(131_072)), vec!["public"]);
        }

        #[test]
        fn for_account_follows_the_sub_policies() {
            let followers = FOLLOWERS << 16;
            assert_eq!(
                for_account(followers, false, true, false),
                ForAccount::Automatic
            );
            assert_eq!(
                for_account(followers, false, false, false),
                ForAccount::Denied
            );
            assert_eq!(for_account(PUBLIC, false, false, false), ForAccount::Manual);
            assert_eq!(
                for_account(UNSUPPORTED_POLICY << 16, false, false, false),
                ForAccount::Unknown
            );
            assert_eq!(for_account(0, true, false, false), ForAccount::Automatic);
        }
    }
}

/// The bitmap in `accounts.feature_approval_policy`, which says who may feature
/// the account's posts and whether they must ask first.
///
/// Mastodon packs two sub-policies into one integer: the automatic one in the
/// high 16 bits, the manual one in the low 16. Each is a set of flags rather
/// than a single choice, so a policy can name several audiences at once, and an
/// unrecognised flag is preserved as `unsupported_policy` — a peer may be
/// running a newer or different implementation, and dropping what we cannot
/// name would silently widen the policy.
pub mod feature_policy {
    pub const UNSUPPORTED: i32 = 1 << 0;
    pub const PUBLIC: i32 = 1 << 1;
    pub const FOLLOWERS: i32 = 1 << 2;
    pub const FOLLOWING: i32 = 1 << 3;
    pub const DISABLED: i32 = 1 << 4;

    /// The automatic sub-policy: who may feature without asking.
    #[must_use]
    pub fn automatic(bitmap: i32) -> i32 {
        bitmap >> 16
    }

    /// The manual sub-policy: who may feature with the author's approval.
    #[must_use]
    pub fn manual(bitmap: i32) -> i32 {
        bitmap & 0xFFFF
    }

    /// The audiences a sub-policy names, in the order Mastodon lists them.
    #[must_use]
    pub fn as_keys(sub_policy: i32) -> Vec<&'static str> {
        [
            (UNSUPPORTED, "unsupported_policy"),
            (PUBLIC, "public"),
            (FOLLOWERS, "followers"),
            (FOLLOWING, "following"),
            (DISABLED, "disabled"),
        ]
        .into_iter()
        .filter(|(flag, _)| sub_policy & flag != 0)
        .map(|(_, name)| name)
        .collect()
    }
}

/// Integer-to-text helpers for custom_filters.action (warn=0 hide=1).
pub mod filter_action {
    pub const WARN: i32 = 0;
    pub const HIDE: i32 = 1;
    pub const BLUR: i32 = 2;

    pub fn from_str(s: &str) -> i32 {
        match s {
            "hide" => HIDE,
            "blur" => BLUR,
            _ => WARN,
        }
    }

    pub fn to_str(v: i32) -> &'static str {
        match v {
            HIDE => "hide",
            BLUR => "blur",
            _ => "warn",
        }
    }
}

/// `DomainBlock#severity`: `{ silence: 0, suspend: 1, noop: 2 }`.
pub mod domain_severity {
    pub const SILENCE: i32 = 0;
    pub const SUSPEND: i32 = 1;
    pub const NOOP: i32 = 2;

    pub fn parse(s: &str) -> Option<i32> {
        match s {
            "silence" => Some(SILENCE),
            "suspend" => Some(SUSPEND),
            "noop" => Some(NOOP),
            _ => None,
        }
    }

    /// The column defaults to `silence` and is nullable; NULL reads as the default.
    pub fn to_str(v: Option<i32>) -> &'static str {
        match v.unwrap_or(SILENCE) {
            SUSPEND => "suspend",
            NOOP => "noop",
            _ => "silence",
        }
    }
}

/// `IpBlock#severity`: `{ sign_up_requires_approval: 5000, sign_up_block: 5500, no_access: 9999 }`.
pub mod ip_severity {
    pub const SIGN_UP_REQUIRES_APPROVAL: i32 = 5000;
    pub const SIGN_UP_BLOCK: i32 = 5500;
    pub const NO_ACCESS: i32 = 9999;

    pub fn parse(s: &str) -> Option<i32> {
        match s {
            "sign_up_requires_approval" => Some(SIGN_UP_REQUIRES_APPROVAL),
            "sign_up_block" => Some(SIGN_UP_BLOCK),
            "no_access" => Some(NO_ACCESS),
            _ => None,
        }
    }

    pub fn to_str(v: i32) -> &'static str {
        match v {
            SIGN_UP_REQUIRES_APPROVAL => "sign_up_requires_approval",
            SIGN_UP_BLOCK => "sign_up_block",
            NO_ACCESS => "no_access",
            _ => "",
        }
    }
}

/// `Report#category`: `{ other: 0, spam: 1_000, legal: 1_500, violation: 2_000 }`.
pub mod report_category {
    pub const OTHER: i32 = 0;
    pub const SPAM: i32 = 1_000;
    pub const LEGAL: i32 = 1_500;
    pub const VIOLATION: i32 = 2_000;

    pub fn parse(s: &str) -> Option<i32> {
        match s {
            "other" => Some(OTHER),
            "spam" => Some(SPAM),
            "legal" => Some(LEGAL),
            "violation" => Some(VIOLATION),
            _ => None,
        }
    }

    pub fn to_str(v: i32) -> &'static str {
        match v {
            SPAM => "spam",
            LEGAL => "legal",
            VIOLATION => "violation",
            _ => "other",
        }
    }
}
