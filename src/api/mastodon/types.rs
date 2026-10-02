/// Mastodon REST API serialization types.
/// Reference: https://docs.joinmastodon.org/entities/
use serde::{Deserialize, Serialize};

// ── Account ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Account {
    pub id: String,
    pub username: String,
    pub acct: String,
    pub display_name: String,
    pub locked: bool,
    pub bot: bool,
    pub group: bool,
    pub discoverable: Option<bool>,
    pub indexable: bool,
    pub hide_collections: Option<bool>,
    pub created_at: String, // ISO 8601 date (midnight UTC, day precision)
    pub note: String,       // HTML
    pub url: String,
    pub uri: String,
    pub avatar: String,
    pub avatar_static: String,
    /// Alt text for the avatar, so a client can describe it.
    pub avatar_description: String,
    pub header: String,
    pub header_static: String,
    pub header_description: String,
    /// Profile display preferences: whether the account's media, media in
    /// replies, and featured collections are shown on its profile.
    pub show_media: bool,
    pub show_media_replies: bool,
    pub show_featured: bool,
    /// Who may feature this account's posts, and whether they must ask first.
    pub feature_approval: FeatureApproval,
    pub followers_count: i64,
    pub following_count: i64,
    pub statuses_count: i64,
    pub last_status_at: Option<String>,
    pub emojis: Vec<CustomEmoji>,
    pub fields: Vec<Field>,
    pub roles: Vec<AccountRole>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved: Option<Box<Account>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suspended: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limited: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noindex: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memorial: Option<bool>,
    /// Mastodon 4.7.0: present and true when the account's handle cannot be
    /// verified, in which case `username` and `acct` are placeholders rather
    /// than the handle the account claims.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_handle: Option<bool>,
    /// Mastodon 4.7: whether visitors may subscribe to the account's posts by
    /// email, present only while the instance has the feature enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_subscriptions: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mute_expires_at: Option<String>, // only on MutedAccount (GET /api/v1/mutes)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<AccountSource>, // only on CredentialAccount
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>, // only on CredentialAccount
}

/// Who may feature an account's posts.
///
/// `automatic` and `manual` name audiences — `public`, `followers`,
/// `following`, `disabled`, or `unsupported_policy` for a flag this version
/// does not recognise. `current_user` says where the requesting account stands:
/// `automatic`, `manual`, `denied`, `missing` when the account has no policy
/// recorded yet, or `unknown` when it cannot be determined.
#[derive(Debug, Clone, Serialize)]
pub struct FeatureApproval {
    pub automatic: Vec<String>,
    pub manual: Vec<String>,
    pub current_user: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountSource {
    pub privacy: String,
    pub sensitive: bool,
    pub language: Option<String>,
    pub note: String, // plain text
    pub fields: Vec<Field>,
    pub follow_requests_count: i64,
    pub discoverable: Option<bool>,
    pub indexable: bool,
    pub hide_collections: Option<bool>,
    pub attribution_domains: Vec<String>,
    pub quote_policy: String,
}

/// Simplified role used in the Account.roles array (REST::AccountSerializer::RoleSerializer).
#[derive(Debug, Clone, Serialize)]
pub struct AccountRole {
    pub id: String,
    pub name: String,
    pub color: String,
}

/// Full role used in CredentialAccount.role (REST::RoleSerializer).
#[derive(Debug, Clone, Serialize)]
pub struct Role {
    pub id: String,
    pub name: String,
    pub color: String,
    pub permissions: String,
    pub highlighted: bool,
    /// How many collections an account with this role may create.
    pub collection_limit: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub value: String,
    pub verified_at: Option<String>,
}

// ── Status ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub id: String,
    pub created_at: String,
    pub in_reply_to_id: Option<String>,
    pub in_reply_to_account_id: Option<String>,
    pub sensitive: bool,
    pub spoiler_text: String,
    pub visibility: String,
    pub language: Option<String>,
    pub uri: String,
    pub url: Option<String>,
    pub replies_count: i64,
    pub reblogs_count: i64,
    pub favourites_count: i64,
    pub quotes_count: i64,
    pub edited_at: Option<String>,
    pub content: String,
    pub reblog: Option<Box<Status>>,
    /// Which application posted this. Serialized even when there is none, as
    /// Mastodon does — a boost carries `"application": null` rather than
    /// omitting the key, and a client testing for it finds it either way.
    pub application: Option<Application>,
    pub account: Account,
    pub media_attachments: Vec<MediaAttachment>,
    pub mentions: Vec<StatusMention>,
    pub tags: Vec<StatusTag>,
    pub emojis: Vec<CustomEmoji>,
    pub card: Option<PreviewCard>,
    pub poll: Option<Poll>,
    pub quote: Option<QuoteInfo>,
    pub quote_approval: QuoteApproval,
    pub tagged_collections: Vec<serde_json::Value>,
    // Viewer-dependent fields: omitted entirely when the request is unauthenticated,
    // matching Mastodon's `attribute :favourited, if: :current_user?` behaviour.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub favourited: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reblogged: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bookmarked: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filtered: Option<Vec<serde_json::Value>>,
    // Only present on DELETE response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusMention {
    pub id: String,
    pub username: String,
    pub acct: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusTag {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Application {
    pub name: String,
    pub website: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuoteApproval {
    pub automatic: Vec<String>,
    pub manual: Vec<String>,
    pub current_user: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct QuoteInfo {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quoted_status: Option<Box<Status>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quoted_status_id: Option<String>,
}

// ── Media ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct MediaAttachment {
    pub id: String,
    #[serde(rename = "type")]
    pub media_type: String,
    pub url: Option<String>,
    pub preview_url: Option<String>,
    pub remote_url: Option<String>,
    pub preview_remote_url: Option<String>,
    pub text_url: Option<String>,
    pub description: Option<String>,
    pub blurhash: Option<String>,
    pub meta: Option<serde_json::Value>,
}

// ── Instance ───────────────────────────────────────────────────────────────

/// Mastodon v2 Instance entity
#[derive(Debug, Serialize)]
pub struct InstanceV2 {
    pub domain: String,
    pub title: String,
    pub version: String,
    pub source_url: String,
    pub description: String,
    pub usage: InstanceUsage,
    pub thumbnail: InstanceThumbnail,
    pub icon: Vec<serde_json::Value>,
    pub languages: Vec<String>,
    pub configuration: InstanceConfiguration,
    pub registrations: InstanceRegistrations,
    pub contact: InstanceContact,
    pub rules: Vec<Rule>,
    pub api_versions: serde_json::Value,
    pub wrapstodon: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct InstanceUsage {
    pub users: InstanceUsageUsers,
}

#[derive(Debug, Serialize)]
pub struct InstanceUsageUsers {
    pub active_month: i64,
}

#[derive(Debug, Serialize)]
pub struct InstanceThumbnail {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blurhash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub versions: Option<serde_json::Value>,
    /// The thumbnail's alt text. Sent even when unset, as Mastodon does.
    pub description: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct InstanceConfiguration {
    pub urls: InstanceUrls,
    pub vapid: VapidConfiguration,
    pub accounts: AccountsConfiguration,
    pub statuses: StatusesConfiguration,
    pub media_attachments: MediaConfiguration,
    pub polls: PollsConfiguration,
    pub translation: TranslationConfiguration,
    pub timelines_access: TimelinesAccess,
    pub limited_federation: bool,
}

#[derive(Debug, Serialize)]
pub struct TimelinesAccess {
    pub live_feeds: TimelineAccessControl,
    pub hashtag_feeds: TimelineAccessControl,
    pub trending_link_feeds: TimelineAccessControl,
}

#[derive(Debug, Serialize)]
pub struct TimelineAccessControl {
    pub local: String,
    pub remote: String,
}

#[derive(Debug, Serialize)]
pub struct VapidConfiguration {
    pub public_key: String,
}

#[derive(Debug, Serialize)]
pub struct InstanceUrls {
    pub streaming: String,
    pub status: Option<String>,
    pub about: Option<String>,
    pub privacy_policy: Option<String>,
    pub terms_of_service: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AccountsConfiguration {
    pub max_featured_tags: u32,
    /// How long the alt text on an avatar or header may be. Mastodon serves
    /// both, and a client that lets someone write one needs to know the limit.
    pub max_avatar_description_length: u32,
    pub max_header_description_length: u32,
    pub max_pinned_statuses: u32,
    pub max_profile_fields: u32,
    pub max_display_name_length: u32,
    pub max_note_length: u32,
    pub profile_field_name_limit: u32,
    pub profile_field_value_limit: u32,
}

#[derive(Debug, Serialize)]
pub struct StatusesConfiguration {
    pub max_characters: u32,
    pub max_media_attachments: u32,
    pub characters_reserved_per_url: u32,
}

#[derive(Debug, Serialize)]
pub struct MediaConfiguration {
    pub supported_mime_types: Vec<String>,
    pub description_limit: u64,
    pub image_size_limit: u64,
    pub image_matrix_limit: u64,
    pub video_size_limit: u64,
    pub video_frame_rate_limit: u64,
    pub video_matrix_limit: u64,
}

#[derive(Debug, Serialize)]
pub struct PollsConfiguration {
    pub max_options: u32,
    pub max_characters_per_option: u32,
    pub min_expiration: u64,
    pub max_expiration: u64,
}

#[derive(Debug, Serialize)]
pub struct TranslationConfiguration {
    pub enabled: bool,
}

#[derive(Debug, Serialize)]
pub struct InstanceRegistrations {
    pub enabled: bool,
    pub approval_required: bool,
    pub reason_required: bool,
    pub min_age: Option<u32>,
    pub message: Option<String>,
    /// Serialized even when absent: Mastodon sends `null`, and a client testing
    /// for the key finds it either way.
    pub url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct InstanceContact {
    pub email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<Account>,
}

#[derive(Debug, Serialize)]
pub struct Rule {
    pub id: String,
    pub text: String,
    pub hint: String,
    pub translations: serde_json::Value,
}

// ── Notification ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct Notification {
    pub id: String,
    #[serde(rename = "type")]
    pub notification_type: String,
    pub created_at: String,
    pub group_key: String,
    pub account: Account,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<Report>,
    // true when notification is routed to filtered inbox (notification policies)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filtered: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moderation_warning: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection: Option<serde_json::Value>,
}

// ── OAuth ──────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct CredentialApplication {
    pub id: String,
    pub name: String,
    pub website: Option<String>,
    pub scopes: Vec<String>,
    pub redirect_uri: String,
    pub redirect_uris: Vec<String>,
    pub client_id: String,
    pub client_secret: String,
    pub client_secret_expires_at: i64,
    pub vapid_key: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Token {
    pub access_token: String,
    pub token_type: String,
    pub scope: String,
    pub created_at: i64,
}

// ── Emoji ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct CustomEmoji {
    pub shortcode: String,
    pub url: String,
    pub static_url: String,
    pub visible_in_picker: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub featured: Option<bool>,
}

// ── Poll ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Poll {
    pub id: String,
    pub expires_at: Option<String>,
    pub expired: bool,
    pub multiple: bool,
    pub votes_count: i64,
    pub voters_count: Option<i64>,
    pub options: Vec<PollOption>,
    pub emojis: Vec<CustomEmoji>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub own_votes: Option<Vec<i32>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PollOption {
    pub title: String,
    pub votes_count: Option<i64>,
}

// ── Preview Card ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct PreviewCard {
    pub url: String,
    pub title: String,
    pub description: String,
    pub language: Option<String>,
    #[serde(rename = "type")]
    pub card_type: String,
    pub author_name: String,
    pub author_url: String,
    pub provider_name: String,
    pub provider_url: String,
    pub html: String,
    pub width: i32,
    pub height: i32,
    pub image: Option<String>,
    pub image_description: String,
    pub embed_url: String,
    pub blurhash: Option<String>,
    pub published_at: Option<String>,
    pub authors: Vec<PreviewCardAuthor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_attribution: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<TagHistory>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PreviewCardAuthor {
    pub name: String,
    pub url: String,
    pub account: Option<Account>,
}

// ── Relationship ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct Relationship {
    pub id: String,
    pub following: bool,
    pub showing_reblogs: bool,
    pub notifying: bool,
    pub languages: Option<Vec<String>>,
    pub followed_by: bool,
    pub blocking: bool,
    pub blocked_by: bool,
    pub muting: bool,
    pub muting_notifications: bool,
    pub muting_expires_at: Option<String>,
    pub requested: bool,
    pub requested_by: bool,
    pub domain_blocking: bool,
    pub endorsed: bool,
    pub note: String,
}

// ── Search ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SearchResults {
    pub accounts: Vec<Account>,
    pub statuses: Vec<Status>,
    pub hashtags: Vec<Tag>,
    pub collections: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct Tag {
    pub id: String,
    pub name: String,
    pub url: String,
    pub history: Vec<TagHistory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub following: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub featuring: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TagHistory {
    pub day: String,
    pub accounts: String,
    pub uses: String,
}

// ── Status Context / Source ───────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct StatusContext {
    pub ancestors: Vec<Status>,
    pub descendants: Vec<Status>,
}

#[derive(Debug, Serialize)]
pub struct StatusSource {
    pub id: String,
    pub text: String,
    pub spoiler_text: String,
}

// ── Markers ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct MarkerInfo {
    pub last_read_id: String,
    pub version: i32,
    pub updated_at: String,
}

// ── Preferences ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct Preferences {
    #[serde(rename = "posting:default:visibility")]
    pub posting_default_visibility: String,
    #[serde(rename = "posting:default:sensitive")]
    pub posting_default_sensitive: bool,
    #[serde(rename = "posting:default:language")]
    pub posting_default_language: Option<String>,
    #[serde(rename = "posting:default:quote_policy")]
    pub posting_default_quote_policy: String,
    #[serde(rename = "reading:expand:media")]
    pub reading_expand_media: String,
    #[serde(rename = "reading:expand:spoilers")]
    pub reading_expand_spoilers: bool,
    #[serde(rename = "reading:autoplay:gifs")]
    pub reading_autoplay_gifs: bool,
}

// ── List ───────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct List {
    pub id: String,
    pub title: String,
    pub replies_policy: String,
    pub exclusive: bool,
}

// ── Status Edit ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct StatusEdit {
    pub content: String,
    pub spoiler_text: String,
    pub sensitive: bool,
    pub created_at: String,
    pub account: Account,
    pub media_attachments: Vec<MediaAttachment>,
    pub emojis: Vec<CustomEmoji>,
    pub poll: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote: Option<serde_json::Value>,
}

// ── Instance V1 ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct InstanceV1 {
    pub uri: String,
    pub title: String,
    pub short_description: String,
    pub description: String,
    pub email: String,
    pub version: String,
    pub urls: InstanceV1Urls,
    pub stats: InstanceV1Stats,
    pub thumbnail: String,
    pub languages: Vec<String>,
    pub registrations: bool,
    pub approval_required: bool,
    pub invites_enabled: bool,
    pub configuration: serde_json::Value,
    pub contact_account: Option<Account>,
    pub rules: Vec<Rule>,
}

#[derive(Debug, Serialize)]
pub struct InstanceV1Urls {
    pub streaming_api: String,
}

#[derive(Debug, Serialize)]
pub struct InstanceV1Stats {
    pub user_count: i64,
    pub status_count: i64,
    pub domain_count: i64,
}

// ── Pagination ─────────────────────────────────────────────────────────────

// ── Filter ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct Filter {
    pub id: String,
    pub title: String,
    pub context: Vec<String>,
    pub expires_at: Option<String>,
    pub filter_action: String,
    pub keywords: Vec<FilterKeyword>,
    pub statuses: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct FilterStatus {
    pub id: String,
    pub status_id: String,
}

#[derive(Debug, Serialize)]
pub struct FilterKeyword {
    pub id: String,
    pub keyword: String,
    pub whole_word: bool,
}

#[derive(Debug, Serialize)]
pub struct FilterV1 {
    pub id: String,
    pub phrase: String,
    pub context: Vec<String>,
    pub whole_word: bool,
    pub expires_at: Option<String>,
    pub irreversible: bool,
}

// ── Profile ──────────────────────────────────────────────────────────────────
// Response shape for GET /api/v1/profile (REST::ProfileSerializer)

#[derive(Debug, Serialize)]
pub struct Profile {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub note: String, // plain text
    pub fields: Vec<Field>,
    pub formatted_note: String, // HTML
    pub formatted_fields: Vec<Field>,
    pub avatar: Option<String>,
    pub avatar_static: Option<String>,
    pub header: Option<String>,
    pub header_static: Option<String>,
    pub locked: bool,
    pub bot: bool,
    pub hide_collections: Option<bool>,
    pub discoverable: Option<bool>,
    pub indexable: bool,
    pub attribution_domains: Vec<String>,
    pub featured_tags: Vec<FeaturedTag>,
}

// ── FeaturedTag ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct FeaturedTag {
    pub id: String,
    pub name: String,
    pub url: String,
    pub statuses_count: String,
    pub last_status_at: Option<String>,
}

// ── AnnouncementReaction ──────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AnnouncementReaction {
    pub name: String,
    pub count: i64,
    pub me: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub static_url: Option<String>,
}

// ── Announcement ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct Announcement {
    pub id: String,
    pub content: String,
    pub all_day: bool,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub published_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read: Option<bool>,
    pub reactions: Vec<AnnouncementReaction>,
    pub statuses: Vec<serde_json::Value>,
    pub tags: Vec<serde_json::Value>,
    pub emojis: Vec<serde_json::Value>,
    pub mentions: Vec<serde_json::Value>,
}

// ── Conversation ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct Conversation {
    pub id: String,
    pub unread: bool,
    pub accounts: Vec<Account>,
    pub last_status: Option<Status>,
}

// ── Report ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub id: String,
    pub action_taken: bool,
    pub action_taken_at: Option<String>,
    pub category: String,
    pub comment: String,
    pub forwarded: bool,
    pub created_at: String,
    pub status_ids: Vec<String>,
    pub rule_ids: Vec<String>,
    pub collection_ids: Vec<String>,
    pub target_account: Account,
}

// ── Notification v2 ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct PartialAccount {
    pub id: String,
    pub acct: String,
    pub locked: bool,
    pub bot: bool,
    pub url: String,
    pub avatar: String,
    pub avatar_static: String,
}

#[derive(Debug, Serialize)]
pub struct NotificationGroupsResponse {
    pub notification_groups: Vec<NotificationGroup>,
    pub accounts: Vec<Account>,
    pub statuses: Vec<Status>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_accounts: Option<Vec<PartialAccount>>,
}

#[derive(Debug, Serialize)]
pub struct NotificationGroup {
    pub group_key: String,
    pub notifications_count: i64,
    #[serde(rename = "type")]
    pub notification_type: String,
    pub most_recent_notification_id: i64,
    pub page_max_id: String,
    pub page_min_id: String,
    pub latest_page_notification_at: String,
    pub sample_account_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<Report>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moderation_warning: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annual_report: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<serde_json::Value>,
}

// ── Notification Policy ─────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct NotificationPolicy {
    pub for_not_following: String,
    pub for_not_followers: String,
    pub for_new_accounts: String,
    pub for_private_mentions: String,
    pub for_limited_accounts: String,
    pub for_bots: String,
    pub summary: NotificationPolicySummary,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct NotificationPolicySummary {
    pub pending_requests_count: i64,
    pub pending_notifications_count: i64,
}

// ── Notification Policy V1 ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct NotificationPolicyV1 {
    pub filter_not_following: bool,
    pub filter_not_followers: bool,
    pub filter_new_accounts: bool,
    pub filter_private_mentions: bool,
    pub filter_bots: bool,
    pub summary: NotificationPolicySummary,
}

// ── Notification Request ─────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct NotificationRequest {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub notifications_count: String,
    pub account: Account,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status: Option<Status>,
}

#[derive(Debug, Deserialize)]
pub struct NotificationPagination {
    pub limit: Option<i64>,
    pub max_id: Option<String>,
    pub since_id: Option<String>,
    pub min_id: Option<String>,
}

// ── Suggestion v2 ───────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct SuggestionV2 {
    pub source: Option<String>,
    pub sources: Vec<String>,
    pub account: Account,
}

// ── App Credentials ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct AppCredentials {
    pub id: String,
    pub name: String,
    pub website: Option<String>,
    pub scopes: Vec<String>,
    pub redirect_uri: String,
    pub redirect_uris: Vec<String>,
    pub vapid_key: Option<String>,
}

// ── Extended Description ────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct ExtendedDescription {
    pub updated_at: Option<String>,
    pub content: String,
}

/// Query parameters used by timeline/list endpoints.
#[derive(Debug, Serialize)]
pub struct FamiliarFollowers {
    pub id: String,
    pub accounts: Vec<Account>,
}

#[derive(Debug, Deserialize)]
pub struct PaginationParams {
    pub max_id: Option<String>,
    pub since_id: Option<String>,
    pub min_id: Option<String>,
    pub limit: Option<String>,
}

impl PaginationParams {
    pub fn limit_clamped(&self, default: i64, max: i64) -> i64 {
        self.limit
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(default)
            .min(max)
            .max(1)
    }
}
