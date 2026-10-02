pub mod accounts;
pub mod admin;
pub mod announcements;
pub mod annual_reports;
pub mod async_refreshes;
pub mod bookmarks;
pub mod collections;
pub mod conversations;
pub mod convert;
pub mod disputes;
pub mod domain_blocks;
pub mod email_subscriptions;
pub mod emojis;
pub mod extractors;
pub mod favourites;
pub mod featured_tags;
pub mod filters;
pub mod formatting;
pub mod instance;
pub mod invites;
pub mod lists;
pub mod markers;
pub mod media;
pub mod notifications;
pub mod oauth;
pub mod oembed;
pub mod polls;
pub mod preview_cards;
pub mod push;
pub mod reports;
pub mod resolve_url;
pub mod scheduled_statuses;
pub mod search;
pub mod signup;
pub mod status_serialize;
pub mod statuses;
pub mod streaming;
pub mod tags;
pub mod timelines;
pub mod trends;
pub mod types;

use crate::middleware as mw;
use axum::{
    extract::DefaultBodyLimit,
    http::HeaderMap,
    middleware,
    routing::{delete, get, patch, post, put},
    Json, Router,
};

/// The locale a status falls back to when neither the request nor the author
/// names one — Mastodon's `I18n.default_locale`, which upstream ships as `en`.
pub const DEFAULT_LOCALE: &str = "en";

/// Build a `Link: <...>; rel="next", <...>; rel="prev"` header value for
/// paginated list endpoints. Returns `None` when `ids` is empty.
///
/// `ids` must be ordered from newest (largest) to oldest (smallest), matching
/// the `ORDER BY id DESC` convention used on every list endpoint.
pub(crate) fn link_header(
    req_headers: &HeaderMap,
    path: &str,
    extra_query: &str, // non-pagination params already joined with '&', no trailing '&'
    newest_id: &str,
    oldest_id: &str,
) -> String {
    let host = req_headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let proto = req_headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("https");
    let base = format!("{proto}://{host}");
    let sep = if extra_query.is_empty() { "" } else { "&" };
    let next = format!("{base}{path}?{extra_query}{sep}max_id={oldest_id}");
    let prev = format!("{base}{path}?{extra_query}{sep}min_id={newest_id}");
    format!(r#"<{next}>; rel="next", <{prev}>; rel="prev""#)
}

/// Build the `Link` header for an endpoint that pages by offset rather than by
/// cursor — the trends endpoints, which Mastodon paginates this way because
/// what they rank by is not the id.
///
/// Mastodon's conditions, from `Api::V1::Trends::*Controller`: `next` only when
/// the page came back full, since a short page is the last one; `prev` only
/// when the offset is more than one page in. That second condition means no
/// `prev` on the second page, which is odd but is what Mastodon does, and a
/// client that follows these headers should land where it would there.
pub(crate) fn offset_link_headers(
    req_headers: &HeaderMap,
    uri: &axum::http::Uri,
    offset: i64,
    limit: i64,
    returned: usize,
) -> HeaderMap {
    let mut resp_headers = HeaderMap::new();
    let host = req_headers
        .get("host")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    let proto = req_headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("https");
    let base = format!("{proto}://{host}{}", uri.path());
    let extra = non_pagination_query(uri.query());
    let extra = extra
        .split('&')
        .filter(|kv| !kv.is_empty() && !kv.starts_with("offset="))
        .collect::<Vec<_>>()
        .join("&");
    let sep = if extra.is_empty() { "" } else { "&" };

    let mut links = Vec::new();
    if returned as i64 == limit {
        links.push(format!(
            r#"<{base}?{extra}{sep}offset={}&limit={limit}>; rel="next""#,
            offset + limit
        ));
    }
    if offset > limit {
        links.push(format!(
            r#"<{base}?{extra}{sep}offset={}&limit={limit}>; rel="prev""#,
            offset - limit
        ));
    }
    if !links.is_empty() {
        if let Ok(val) = links.join(", ").parse() {
            resp_headers.insert(axum::http::header::LINK, val);
        }
    }
    resp_headers
}

/// Strip pagination-specific keys from a query string and return the rest.
pub(crate) fn non_pagination_query(raw_query: Option<&str>) -> String {
    raw_query
        .unwrap_or("")
        .split('&')
        .filter(|kv| {
            !kv.is_empty()
                && !kv.starts_with("max_id=")
                && !kv.starts_with("min_id=")
                && !kv.starts_with("since_id=")
                && !kv.starts_with("limit=")
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Build the response `HeaderMap` carrying the pagination `Link` header for a
/// page whose newest/oldest cursor ids are `bounds` (`None` for an empty page).
///
/// Combines [`non_pagination_query`] and [`link_header`] with the header
/// insertion every paginated list endpoint would otherwise repeat. Callers only
/// need to express how they derive the two cursor ids, e.g.
/// `result.first().zip(result.last()).map(|(n, o)| (n.id.as_str(), o.id.as_str()))`.
pub(crate) fn link_headers(
    req_headers: &HeaderMap,
    uri: &axum::http::Uri,
    bounds: Option<(&str, &str)>,
) -> HeaderMap {
    let mut resp_headers = HeaderMap::new();
    if let Some((newest, oldest)) = bounds {
        let extra = non_pagination_query(uri.query());
        let link = link_header(req_headers, uri.path(), &extra, newest, oldest);
        if let Ok(val) = link.parse() {
            resp_headers.insert(axum::http::header::LINK, val);
        }
    }
    resp_headers
}

pub fn router() -> Router {
    let auth_required = Router::new()
        // Accounts — authenticated
        .route(
            "/api/v1/accounts/verify_credentials",
            get(accounts::verify_credentials),
        )
        .route("/api/v1/accounts/search", get(accounts::search_accounts))
        .route(
            "/api/v1/accounts/relationships",
            get(accounts::get_relationships),
        )
        .route(
            "/api/v1/accounts/{id}/follow",
            post(accounts::follow_account),
        )
        .route(
            "/api/v1/accounts/{id}/unfollow",
            post(accounts::unfollow_account),
        )
        .route("/api/v1/accounts/{id}/mute", post(accounts::mute_account))
        .route(
            "/api/v1/accounts/{id}/unmute",
            post(accounts::unmute_account),
        )
        .route("/api/v1/accounts/{id}/block", post(accounts::block_account))
        .route(
            "/api/v1/accounts/{id}/unblock",
            post(accounts::unblock_account),
        )
        .route(
            "/api/v1/accounts/{id}/note",
            post(accounts::set_account_note),
        )
        .route(
            "/api/v1/accounts/{id}/remove_from_followers",
            post(accounts::remove_from_followers),
        )
        .route(
            "/api/v1/accounts/{id}/endorse",
            post(accounts::endorse_account),
        )
        .route(
            "/api/v1/accounts/{id}/unendorse",
            post(accounts::unendorse_account),
        )
        // `pin` and `unpin`: the older names Mastodon still routes to the
        // same endorsement actions.
        .route("/api/v1/accounts/{id}/pin", post(accounts::endorse_account))
        .route(
            "/api/v1/accounts/{id}/unpin",
            post(accounts::unendorse_account),
        )
        .route(
            "/api/v1/accounts/{id}/lists",
            get(accounts::get_account_lists),
        )
        .route(
            "/api/v1/accounts/{id}/in_collections",
            get(collections::account_in_collections),
        )
        .route(
            "/api/v1_alpha/accounts/{id}/in_collections",
            get(collections::account_in_collections),
        )
        // Collections (authenticated writes)
        .route("/api/v1/collections", post(collections::create_collection))
        .route(
            "/api/v1/collections/{id}",
            put(collections::update_collection).delete(collections::delete_collection),
        )
        .route(
            "/api/v1/collections/{id}/items",
            post(collections::add_collection_item),
        )
        .route(
            "/api/v1/collections/{collection_id}/items/{item_id}",
            delete(collections::delete_collection_item),
        )
        .route(
            "/api/v1/collections/{collection_id}/items/{item_id}/revoke",
            post(collections::revoke_collection_item),
        )
        // Mastodon routes collections under `/api/v1_alpha` too, to the same
        // controllers.
        .route(
            "/api/v1_alpha/collections",
            post(collections::create_collection),
        )
        .route(
            "/api/v1_alpha/collections/{id}",
            put(collections::update_collection).delete(collections::delete_collection),
        )
        .route(
            "/api/v1_alpha/collections/{id}/items",
            post(collections::add_collection_item),
        )
        .route(
            "/api/v1_alpha/collections/{collection_id}/items/{item_id}",
            delete(collections::delete_collection_item),
        )
        .route(
            "/api/v1_alpha/collections/{collection_id}/items/{item_id}/revoke",
            post(collections::revoke_collection_item),
        )
        // Preferences
        .route("/api/v1/preferences", get(accounts::get_preferences))
        // Follow requests
        .route(
            "/api/v1/follow_requests",
            get(accounts::get_follow_requests),
        )
        .route(
            "/api/v1/follow_requests/{id}/authorize",
            post(accounts::authorize_follow_request),
        )
        .route(
            "/api/v1/follow_requests/{id}/reject",
            post(accounts::reject_follow_request),
        )
        // Statuses — authenticated writes
        .route(
            "/api/v1/statuses",
            get(statuses::get_statuses_batch).post(statuses::post_status),
        )
        .route(
            "/api/v1/statuses/{id}",
            delete(statuses::delete_status).put(statuses::edit_status),
        )
        .route(
            "/api/v1/statuses/{id}/favourite",
            post(statuses::favourite_status),
        )
        .route(
            "/api/v1/statuses/{id}/unfavourite",
            post(statuses::unfavourite_status),
        )
        .route(
            "/api/v1/statuses/{id}/reblog",
            post(statuses::reblog_status),
        )
        .route(
            "/api/v1/statuses/{id}/unreblog",
            post(statuses::unreblog_status),
        )
        .route(
            "/api/v1/statuses/{id}/bookmark",
            post(statuses::bookmark_status),
        )
        .route(
            "/api/v1/statuses/{id}/unbookmark",
            post(statuses::unbookmark_status),
        )
        .route("/api/v1/statuses/{id}/pin", post(statuses::pin_status))
        .route("/api/v1/statuses/{id}/unpin", post(statuses::unpin_status))
        .route("/api/v1/statuses/{id}/mute", post(statuses::mute_status))
        .route(
            "/api/v1/statuses/{id}/unmute",
            post(statuses::unmute_status),
        )
        .route(
            "/api/v1/statuses/{id}/source",
            get(statuses::get_status_source),
        )
        .route(
            "/api/v1/statuses/{id}/interaction_policy",
            put(statuses::update_interaction_policy),
        )
        .route(
            "/api/v1/statuses/{id}/quotes",
            get(statuses::get_status_quotes),
        )
        .route(
            "/api/v1/statuses/{status_id}/quotes/{id}/revoke",
            post(statuses::revoke_quote),
        )
        // Blocks / Mutes lists
        .route("/api/v1/blocks", get(accounts::get_blocks))
        .route("/api/v1/mutes", get(accounts::get_mutes))
        // Lists
        .route(
            "/api/v1/lists",
            get(lists::get_lists).post(lists::create_list),
        )
        .route(
            "/api/v1/lists/{id}",
            get(lists::get_list)
                .put(lists::update_list)
                .delete(lists::delete_list),
        )
        .route(
            "/api/v1/lists/{id}/accounts",
            get(lists::get_list_accounts)
                .post(lists::add_list_accounts)
                .delete(lists::remove_list_accounts),
        )
        // Notifications
        .route(
            "/api/v1/notifications",
            get(notifications::get_notifications),
        )
        .route(
            "/api/v1/notifications/clear",
            post(notifications::clear_notifications),
        )
        .route(
            "/api/v1/notifications/unread_count",
            get(notifications::get_notifications_unread_count),
        )
        .route(
            "/api/v1/notifications/requests",
            get(notifications::get_notification_requests),
        )
        .route(
            "/api/v1/notifications/requests/{id}",
            get(notifications::get_notification_request),
        )
        .route(
            "/api/v1/notifications/requests/{id}/accept",
            post(notifications::accept_notification_request),
        )
        .route(
            "/api/v1/notifications/requests/{id}/dismiss",
            post(notifications::dismiss_notification_request),
        )
        .route(
            "/api/v1/notifications/requests/accept",
            post(notifications::accept_all_notification_requests),
        )
        .route(
            "/api/v1/notifications/requests/dismiss",
            post(notifications::dismiss_all_notification_requests),
        )
        .route(
            "/api/v1/notifications/requests/merged",
            get(notifications::get_notification_requests_merged),
        )
        .route(
            "/api/v1/notifications/{id}",
            get(notifications::get_notification),
        )
        .route(
            "/api/v1/notifications/{id}/dismiss",
            post(notifications::dismiss_notification),
        )
        .route(
            "/api/v2/notifications",
            get(notifications::get_notifications_v2),
        )
        .route(
            "/api/v2/notifications/clear",
            post(notifications::clear_notifications),
        )
        .route(
            "/api/v2/notifications/unread_count",
            get(notifications::get_notifications_unread_count),
        )
        .route(
            "/api/v2/notifications/{group_key}",
            get(notifications::get_notification_group),
        )
        .route(
            "/api/v2/notifications/{group_key}/dismiss",
            post(notifications::dismiss_notification_group),
        )
        .route(
            "/api/v2/notifications/{group_key}/accounts",
            get(notifications::get_notification_group_accounts),
        )
        .route(
            "/api/v1/notifications/policy",
            get(notifications::get_notification_policy_v1)
                .patch(notifications::update_notification_policy_v1),
        )
        .route(
            "/api/v2/notifications/policy",
            get(notifications::get_notification_policy)
                .patch(notifications::update_notification_policy),
        )
        // Media — get / update / delete
        .route(
            "/api/v1/media/{id}",
            get(media::get_media)
                .put(media::update_media)
                .delete(media::delete_media),
        )
        // Bookmarks / Favourites
        .route("/api/v1/bookmarks", get(bookmarks::get_bookmarks))
        .route("/api/v1/favourites", get(favourites::get_favourites))
        // Markers
        .route(
            "/api/v1/markers",
            get(markers::get_markers).post(markers::set_markers),
        )
        // Timelines
        .route("/api/v1/timelines/home", get(timelines::home_timeline))
        .route("/api/v1/timelines/list/{id}", get(timelines::list_timeline))
        // Polls (authenticated write)
        .route("/api/v1/polls/{id}/votes", post(polls::vote_poll))
        // Invites
        .route(
            "/api/v1/invites",
            get(invites::list_invites).post(invites::create_invite),
        )
        .route("/api/v1/invites/{id}", delete(invites::delete_invite))
        // Account deletion
        .route("/api/v1/accounts", delete(accounts::delete_account))
        // Email confirmation resend stub (accounts are confirmed immediately; this is a no-op)
        .route("/api/v1/emails/confirmations", post(empty_object))
        .route(
            "/api/v1/emails/check_confirmation",
            get(signup::check_email_confirmation),
        )
        // Admin API
        .route("/api/v1/admin/accounts", get(admin::list_admin_accounts))
        .route("/api/v2/admin/accounts", get(admin::list_admin_accounts_v2))
        .route(
            "/api/v1/admin/accounts/{id}",
            get(admin::get_admin_account).delete(admin::delete_admin_account),
        )
        .route(
            "/api/v1/admin/accounts/{id}/approve",
            post(admin::approve_account),
        )
        .route(
            "/api/v1/admin/accounts/{id}/reject",
            post(admin::reject_account),
        )
        .route(
            "/api/v1/admin/accounts/{id}/enable",
            post(admin::enable_account),
        )
        .route(
            "/api/v1/admin/accounts/{id}/unsilence",
            post(admin::unsilence_account),
        )
        .route(
            "/api/v1/admin/accounts/{id}/unsuspend",
            post(admin::unsuspend_account),
        )
        .route(
            "/api/v1/admin/accounts/{id}/unsensitive",
            post(admin::unsensitive_account),
        )
        .route(
            "/api/v1/admin/accounts/{id}/action",
            post(admin::account_action),
        )
        .route("/api/v1/admin/reports", get(admin::list_admin_reports))
        .route(
            "/api/v1/admin/reports/{id}",
            get(admin::get_admin_report).patch(admin::update_admin_report),
        )
        .route(
            "/api/v1/admin/reports/{id}/resolve",
            post(admin::resolve_report),
        )
        .route(
            "/api/v1/admin/reports/{id}/reopen",
            post(admin::reopen_report),
        )
        .route(
            "/api/v1/admin/reports/{id}/assign_to_self",
            post(admin::assign_report_to_self),
        )
        .route(
            "/api/v1/admin/reports/{id}/unassign",
            post(admin::unassign_report),
        )
        .route("/api/v1/admin/dimensions", post(admin::get_dimensions))
        .route("/api/v1/admin/measures", post(admin::get_measures))
        .route("/api/v1/admin/retention", post(admin::get_retention))
        .route(
            "/api/v1/admin/custom_emojis",
            get(admin::list_admin_custom_emojis).post(admin::create_admin_custom_emoji),
        )
        .route(
            "/api/v1/admin/custom_emojis/{id}",
            patch(admin::update_admin_custom_emoji).delete(admin::delete_admin_custom_emoji),
        )
        .route(
            "/api/v1/admin/domain_blocks",
            get(admin::list_domain_blocks).post(admin::create_domain_block),
        )
        .route(
            "/api/v1/admin/domain_blocks/{id}",
            get(admin::get_admin_domain_block)
                .patch(admin::update_admin_domain_block)
                .delete(admin::delete_domain_block),
        )
        .route(
            "/api/v1/admin/domain_allows",
            get(admin::list_domain_allows).post(admin::create_domain_allow),
        )
        .route(
            "/api/v1/admin/domain_allows/{id}",
            get(admin::get_domain_allow).delete(admin::delete_domain_allow),
        )
        .route(
            "/api/v1/admin/ip_blocks",
            get(admin::list_ip_blocks).post(admin::create_ip_block),
        )
        .route(
            "/api/v1/admin/ip_blocks/{id}",
            get(admin::get_ip_block)
                .patch(admin::update_ip_block)
                .delete(admin::delete_ip_block),
        )
        .route(
            "/api/v1/admin/email_domain_blocks",
            get(admin::list_email_domain_blocks).post(admin::create_email_domain_block),
        )
        .route(
            "/api/v1/admin/email_domain_blocks/{id}",
            get(admin::get_email_domain_block).delete(admin::delete_email_domain_block),
        )
        .route("/api/v1/admin/trends/tags", get(admin::admin_trending_tags))
        .route(
            "/api/v1/admin/trends/links/publishers",
            get(admin::admin_list_publishers),
        )
        .route(
            "/api/v1/admin/trends/links/publishers/{id}/approve",
            post(admin::admin_approve_publisher),
        )
        .route(
            "/api/v1/admin/trends/links/publishers/{id}/reject",
            post(admin::admin_reject_publisher),
        )
        .route(
            "/api/v1/admin/trends/statuses",
            get(admin::admin_trending_statuses),
        )
        .route(
            "/api/v1/admin/trends/links",
            get(admin::admin_trending_links),
        )
        .route(
            "/api/v1/admin/trends/tags/{id}/approve",
            post(admin::admin_approve_trending_tag),
        )
        .route(
            "/api/v1/admin/trends/tags/{id}/reject",
            post(admin::admin_reject_trending_tag),
        )
        .route(
            "/api/v1/admin/trends/statuses/{id}/approve",
            post(admin::admin_approve_trending_status),
        )
        .route(
            "/api/v1/admin/trends/statuses/{id}/reject",
            post(admin::admin_reject_trending_status),
        )
        .route(
            "/api/v1/admin/trends/links/{id}/approve",
            post(admin::admin_approve_trending_link),
        )
        .route(
            "/api/v1/admin/trends/links/{id}/reject",
            post(admin::admin_reject_trending_link),
        )
        // Email subscriptions: what Mastodon's admin pages for them do.
        .route(
            "/api/v1/admin/email_subscriptions",
            get(admin::show_email_subscriptions),
        )
        .route(
            "/api/v1/admin/email_subscriptions/setup",
            post(admin::setup_email_subscriptions),
        )
        .route(
            "/api/v1/admin/email_subscriptions/disable",
            post(admin::disable_email_subscriptions),
        )
        .route(
            "/api/v1/admin/email_subscriptions/purge",
            post(admin::purge_email_subscriptions),
        )
        .route(
            "/api/v1/admin/email_subscriptions/additional_footer_text",
            put(admin::update_email_footer_text),
        )
        .route(
            "/api/v1/admin/email_subscriptions/accounts/{id}",
            get(admin::show_email_subscription_account),
        )
        .route(
            "/api/v1/admin/email_subscriptions/accounts/{id}/subscriptions",
            get(admin::list_email_subscription_subscribers),
        )
        .route(
            "/api/v1/admin/email_subscriptions/accounts/{id}/enable",
            post(admin::enable_email_subscription_account),
        )
        .route(
            "/api/v1/admin/email_subscriptions/accounts/{id}/disable",
            post(admin::disable_email_subscription_account),
        )
        .route(
            "/api/v1/admin/email_subscriptions/{id}",
            delete(admin::delete_email_subscription),
        )
        .route("/api/v1/admin/tags", get(admin::list_admin_tags))
        .route(
            "/api/v1/admin/tags/{id}",
            get(admin::get_admin_tag).patch(admin::update_admin_tag),
        )
        .route(
            "/api/v1/admin/canonical_email_blocks",
            get(admin::list_canonical_email_blocks).post(admin::create_canonical_email_block),
        )
        .route(
            "/api/v1/admin/canonical_email_blocks/test",
            post(admin::test_canonical_email_block),
        )
        .route(
            "/api/v1/admin/canonical_email_blocks/{id}",
            get(admin::get_canonical_email_block).delete(admin::delete_canonical_email_block),
        )
        // Terms of service, at Mastodon's admin paths
        .route(
            "/api/v1/admin/terms_of_service",
            get(admin::admin_terms_of_service),
        )
        .route(
            "/api/v1/admin/terms_of_service/history",
            get(admin::admin_terms_of_service_history),
        )
        .route(
            "/api/v1/admin/terms_of_service/draft",
            get(admin::admin_terms_of_service_draft)
                .put(admin::update_admin_terms_of_service_draft),
        )
        .route(
            "/api/v1/admin/terms_of_service/generate",
            get(admin::admin_terms_of_service_generator)
                .post(admin::generate_admin_terms_of_service),
        )
        .route(
            "/api/v1/admin/terms_of_service/{id}/preview",
            get(admin::admin_terms_of_service_preview),
        )
        .route(
            "/api/v1/admin/terms_of_service/{id}/test",
            post(admin::test_admin_terms_of_service),
        )
        .route(
            "/api/v1/admin/terms_of_service/{id}/distribution",
            post(admin::distribute_admin_terms_of_service),
        )
        // Moderation tools Mastodon has only as server-rendered admin pages
        // (eunha's own REST surface; see `moderation-tools-rest-api` in
        // divergences.toml).
        .route(
            "/api/v1/admin/report_notes",
            get(admin::list_report_notes).post(admin::create_report_note),
        )
        .route(
            "/api/v1/admin/report_notes/{id}",
            delete(admin::delete_report_note),
        )
        .route(
            "/api/v1/admin/reports/{id}/history",
            get(admin::report_history),
        )
        .route(
            "/api/v1/admin/reports/{id}/actions",
            post(admin::report_moderation_action),
        )
        .route(
            "/api/v1/admin/account_moderation_notes",
            get(admin::list_account_moderation_notes).post(admin::create_account_moderation_note),
        )
        .route(
            "/api/v1/admin/account_moderation_notes/{id}",
            delete(admin::delete_account_moderation_note),
        )
        .route("/api/v1/admin/action_logs", get(admin::list_action_logs))
        .route(
            "/api/v1/admin/action_logs/filters",
            get(admin::action_log_filters),
        )
        .route(
            "/api/v1/admin/warning_presets",
            get(admin::list_warning_presets).post(admin::create_warning_preset),
        )
        .route(
            "/api/v1/admin/warning_presets/{id}",
            get(admin::get_warning_preset)
                .patch(admin::update_warning_preset)
                .delete(admin::delete_warning_preset),
        )
        .route(
            "/api/v1/admin/username_blocks",
            get(admin::list_username_blocks).post(admin::create_username_block),
        )
        .route(
            "/api/v1/admin/username_blocks/{id}",
            get(admin::get_username_block)
                .patch(admin::update_username_block)
                .delete(admin::delete_username_block),
        )
        .route("/api/v1/admin/disputes/appeals", get(admin::list_appeals))
        .route(
            "/api/v1/admin/disputes/appeals/{id}/approve",
            post(admin::approve_appeal),
        )
        .route(
            "/api/v1/admin/disputes/appeals/{id}/reject",
            post(admin::reject_appeal),
        )
        .route(
            "/api/v1/admin/accounts/{id}/statuses",
            get(admin::list_admin_account_statuses),
        )
        .route(
            "/api/v1/admin/accounts/{id}/statuses/batch",
            post(admin::batch_admin_account_statuses),
        )
        .route(
            "/api/v1/admin/accounts/{id}/statuses/{status_id}",
            get(admin::get_admin_account_status),
        )
        .route(
            "/api/v1/admin/accounts/{id}/relationships",
            get(admin::list_admin_relationships),
        )
        .route(
            "/api/v1/admin/roles",
            get(admin::list_assignable_roles).post(admin::create_admin_role),
        )
        .route(
            "/api/v1/admin/accounts/{id}/role",
            put(admin::change_user_role),
        )
        .route(
            "/api/v1/admin/accounts/{id}/two_factor_authentication",
            delete(admin::disable_user_two_factor),
        )
        .route(
            "/api/v1/admin/accounts/{id}/change_email",
            post(admin::change_user_email),
        )
        .route(
            "/api/v1/admin/accounts/{id}/confirmation",
            post(admin::confirm_user),
        )
        .route(
            "/api/v1/admin/accounts/{id}/confirmation/resend",
            post(admin::resend_user_confirmation),
        )
        .route(
            "/api/v1/admin/accounts/{id}/reset",
            post(admin::reset_user_password),
        )
        // Server administration Mastodon has only as server-rendered admin pages
        .route(
            "/api/v1/admin/settings",
            get(admin::get_admin_settings).patch(admin::update_admin_settings),
        )
        .route(
            "/api/v1/admin/site_uploads/{id}",
            delete(admin::delete_site_upload),
        )
        .route(
            "/api/v1/admin/rules",
            get(admin::list_admin_rules).post(admin::create_admin_rule),
        )
        .route(
            "/api/v1/admin/rules/{id}",
            get(admin::get_admin_rule)
                .patch(admin::update_admin_rule)
                .delete(admin::delete_admin_rule),
        )
        .route(
            "/api/v1/admin/rules/{id}/move_up",
            post(admin::move_admin_rule_up),
        )
        .route(
            "/api/v1/admin/rules/{id}/move_down",
            post(admin::move_admin_rule_down),
        )
        .route(
            "/api/v1/admin/roles/{id}",
            get(admin::get_admin_role)
                .patch(admin::update_admin_role)
                .delete(admin::delete_admin_role),
        )
        .route(
            "/api/v1/admin/announcements",
            get(admin::list_admin_announcements).post(admin::create_admin_announcement),
        )
        .route(
            "/api/v1/admin/announcements/{id}",
            get(admin::get_admin_announcement)
                .patch(admin::update_admin_announcement)
                .delete(admin::delete_admin_announcement),
        )
        .route(
            "/api/v1/admin/announcements/{id}/publish",
            post(admin::publish_admin_announcement),
        )
        .route(
            "/api/v1/admin/announcements/{id}/unpublish",
            post(admin::unpublish_admin_announcement),
        )
        .route(
            "/api/v1/admin/announcements/{id}/preview",
            get(admin::preview_admin_announcement),
        )
        .route(
            "/api/v1/admin/announcements/{id}/test",
            post(admin::test_admin_announcement),
        )
        .route(
            "/api/v1/admin/announcements/{id}/distribution",
            post(admin::distribute_admin_announcement),
        )
        .route("/api/v1/admin/instances", get(admin::list_admin_instances))
        .route(
            "/api/v1/admin/instances/{domain}",
            get(admin::get_admin_instance).delete(admin::purge_admin_instance),
        )
        .route(
            "/api/v1/admin/instances/{domain}/clear_delivery_errors",
            post(admin::clear_instance_delivery_errors),
        )
        .route(
            "/api/v1/admin/instances/{domain}/restart_delivery",
            post(admin::restart_instance_delivery),
        )
        .route(
            "/api/v1/admin/instances/{domain}/stop_delivery",
            post(admin::stop_instance_delivery),
        )
        .route(
            "/api/v1/admin/instances/{domain}/moderation_notes",
            post(admin::create_instance_moderation_note),
        )
        .route(
            "/api/v1/admin/instances/{domain}/moderation_notes/{id}",
            delete(admin::delete_instance_moderation_note),
        )
        .route(
            "/api/v1/admin/export_domain_blocks/export",
            get(admin::export_domain_blocks),
        )
        .route(
            "/api/v1/admin/export_domain_blocks/import",
            post(admin::import_domain_blocks),
        )
        .route(
            "/api/v1/admin/export_domain_allows/export",
            get(admin::export_domain_allows),
        )
        .route(
            "/api/v1/admin/export_domain_allows/import",
            post(admin::import_domain_allows),
        )
        .route(
            "/api/v1/admin/relays",
            get(admin::list_admin_relays).post(admin::create_admin_relay),
        )
        .route(
            "/api/v1/admin/relays/{id}",
            delete(admin::delete_admin_relay),
        )
        .route(
            "/api/v1/admin/relays/{id}/enable",
            post(admin::enable_admin_relay),
        )
        .route(
            "/api/v1/admin/relays/{id}/disable",
            post(admin::disable_admin_relay),
        )
        .route("/api/v1/disputes/strikes", get(disputes::list_strikes))
        .route("/api/v1/disputes/strikes/{id}", get(disputes::get_strike))
        .route(
            "/api/v1/disputes/strikes/{id}/appeal",
            post(disputes::appeal_strike),
        )
        // Account move and aliases
        .route("/api/v1/accounts/move", post(accounts::move_account))
        .route(
            "/api/v1/accounts/redirect",
            post(accounts::create_redirect).delete(accounts::cancel_redirect),
        )
        .route(
            "/api/v1/profile/avatar",
            delete(accounts::delete_profile_avatar),
        )
        .route(
            "/api/v1/profile/header",
            delete(accounts::delete_profile_header),
        )
        .route(
            "/api/v1/profile/aliases",
            get(accounts::list_aliases).post(accounts::create_alias),
        )
        .route(
            "/api/v1/profile/aliases/{id}",
            delete(accounts::delete_alias),
        )
        // Suggestions
        .route("/api/v1/directory", get(accounts::get_directory))
        .route("/api/v1/suggestions", get(accounts::get_suggestions))
        .route(
            "/api/v1/suggestions/{id}",
            delete(accounts::dismiss_suggestion),
        )
        .route("/api/v2/suggestions", get(accounts::get_suggestions_v2))
        // Familiar followers
        .route(
            "/api/v1/accounts/familiar_followers",
            get(accounts::get_familiar_followers),
        )
        // Profile tab display settings
        .route(
            "/api/v1/profile",
            get(accounts::get_profile)
                .patch(accounts::patch_profile)
                .put(accounts::put_profile),
        )
        // Followed tags
        .route("/api/v1/followed_tags", get(tags::list_followed_tags))
        .route("/api/v1/tags/{name}/follow", post(tags::follow_tag))
        .route("/api/v1/tags/{name}/unfollow", post(tags::unfollow_tag))
        .route(
            "/api/v1/tags/{name}/feature",
            post(featured_tags::feature_tag_by_name),
        )
        .route(
            "/api/v1/tags/{name}/unfeature",
            post(featured_tags::unfeature_tag_by_name),
        )
        // Featured tags
        .route(
            "/api/v1/featured_tags",
            get(featured_tags::list_featured_tags).post(featured_tags::feature_tag),
        )
        .route(
            "/api/v1/featured_tags/suggestions",
            get(featured_tags::featured_tag_suggestions),
        )
        .route(
            "/api/v1/featured_tags/{id}",
            delete(featured_tags::unfeature_tag),
        )
        // Filters v1
        .route(
            "/api/v1/filters",
            get(filters::get_filters_v1).post(filters::create_filter_v1),
        )
        .route(
            "/api/v1/filters/{id}",
            get(filters::get_filter_v1)
                .put(filters::update_filter_v1)
                .delete(filters::delete_filter_v1),
        )
        // Filters v2
        .route(
            "/api/v2/filters",
            get(filters::get_filters_v2).post(filters::create_filter_v2),
        )
        .route(
            "/api/v2/filters/{id}",
            get(filters::get_filter_v2)
                .put(filters::update_filter_v2)
                .delete(filters::delete_filter_v2),
        )
        .route(
            "/api/v2/filters/{id}/keywords",
            get(filters::get_filter_keywords).post(filters::create_filter_keyword),
        )
        .route(
            "/api/v2/filter_keywords/{id}",
            get(filters::get_filter_keyword)
                .put(filters::update_filter_keyword)
                .delete(filters::delete_filter_keyword),
        )
        .route(
            "/api/v2/filters/{id}/statuses",
            get(filters::get_filter_statuses).post(filters::add_filter_status),
        )
        .route(
            "/api/v2/filter_statuses/{id}",
            get(filters::get_filter_status).delete(filters::delete_filter_status),
        )
        // Domain blocks (user-level)
        .route(
            "/api/v1/domain_blocks",
            get(domain_blocks::get_domain_blocks)
                .post(domain_blocks::block_domain)
                .delete(domain_blocks::unblock_domain),
        )
        .route(
            "/api/v1/domain_blocks/preview",
            get(domain_blocks::preview_domain_block),
        )
        // Reports
        .route("/api/v1/reports", post(reports::file_report))
        // Push notifications (VAPID + Web Push)
        .route(
            "/api/v1/push/subscription",
            get(push::get_subscription)
                .post(push::create_subscription)
                .put(push::update_subscription)
                .delete(push::delete_subscription),
        )
        .route(
            "/api/v1_alpha/async_refreshes/{id}",
            get(async_refreshes::show_async_refresh),
        )
        // Annual reports
        .route(
            "/api/v1/annual_reports",
            get(annual_reports::list_annual_reports),
        )
        .route(
            "/api/v1/annual_reports/{year}",
            get(annual_reports::get_annual_report),
        )
        .route(
            "/api/v1/annual_reports/{year}/read",
            post(annual_reports::read_annual_report),
        )
        .route(
            "/api/v1/annual_reports/{year}/generate",
            post(annual_reports::generate_annual_report),
        )
        .route(
            "/api/v1/annual_reports/{year}/state",
            get(annual_reports::get_annual_report_state),
        )
        // Scheduled statuses
        .route(
            "/api/v1/scheduled_statuses",
            get(scheduled_statuses::list_scheduled_statuses),
        )
        .route(
            "/api/v1/scheduled_statuses/{id}",
            get(scheduled_statuses::get_scheduled_status)
                .put(scheduled_statuses::update_scheduled_status)
                .delete(scheduled_statuses::delete_scheduled_status),
        )
        .layer(middleware::from_fn(require_auth));

    // File-upload routes carry a generous body limit (25 MB, matching Mastodon).
    // Kept separate so the cap applies only here; everything else inherits the
    // app-wide default in `build_app`.
    let uploads = Router::new()
        .route("/api/v1/media", post(media::upload_media))
        .route("/api/v2/media", post(media::upload_media))
        .route(
            "/api/v2/media/{id}",
            get(media::get_media).put(media::update_media),
        )
        .route(
            "/api/v1/accounts/update_credentials",
            patch(accounts::update_credentials),
        )
        .layer(DefaultBodyLimit::max(25 * 1024 * 1024))
        .layer(middleware::from_fn(require_auth));

    let public = Router::new()
        // Instance info
        .route("/api/v1/instance", get(instance::get_instance_v1))
        .route(
            "/api/v1/instance/extended_description",
            get(instance::get_extended_description),
        )
        .route(
            "/api/v1/instance/privacy_policy",
            get(instance::get_privacy_policy),
        )
        .route(
            "/api/v1/instance/translation_languages",
            get(instance::get_translation_languages),
        )
        .route("/api/v1/instance/rules", get(instance::get_instance_rules))
        .route("/api/v1/instance/peers", get(instance::get_peers))
        .route(
            "/api/v1/instance/activity",
            get(instance::get_instance_activity),
        )
        .route(
            "/api/v1/instance/languages",
            get(instance::get_instance_languages),
        )
        .route(
            "/api/v1/instance/domain_blocks",
            get(instance::get_instance_domain_blocks),
        )
        .route("/api/v2/instance", get(instance::get_instance_v2))
        // App credentials
        .route(
            "/api/v1/apps/verify_credentials",
            get(oauth::verify_app_credentials),
        )
        // Accounts (public)
        .route("/api/v1/accounts", get(accounts::get_accounts_batch))
        .route("/api/v1/accounts/lookup", get(accounts::lookup_account))
        .route("/api/v1/accounts/{id}", get(accounts::get_account))
        // Email subscriptions: no token, as Mastodon asks none.
        .route(
            "/api/v1/accounts/{id}/email_subscriptions",
            post(email_subscriptions::create),
        )
        .route(
            "/email_subscriptions/confirmation",
            get(email_subscriptions::confirmation),
        )
        .route(
            "/unsubscribe",
            get(email_subscriptions::unsubscribe_page).post(email_subscriptions::unsubscribe),
        )
        .route(
            "/api/v1/accounts/{id}/featured_tags",
            get(accounts::get_account_featured_tags),
        )
        .route(
            "/api/v1/accounts/{id}/pins",
            get(accounts::get_account_pins),
        )
        .route(
            "/api/v1/accounts/{id}/statuses",
            get(accounts::get_account_statuses),
        )
        .route(
            "/api/v1/accounts/{id}/followers",
            get(accounts::get_account_followers),
        )
        .route(
            "/api/v1/accounts/{id}/following",
            get(accounts::get_account_following),
        )
        .route(
            "/api/v1/accounts/{id}/endorsements",
            get(accounts::get_endorsements),
        )
        .route("/api/v1/endorsements", get(accounts::get_my_endorsements))
        // Collections (public read)
        .route(
            "/api/v1/accounts/{id}/collections",
            get(collections::account_collections),
        )
        .route(
            "/api/v1_alpha/accounts/{id}/collections",
            get(collections::account_collections),
        )
        .route(
            "/api/v1/collections/{id}",
            get(collections::show_collection),
        )
        .route(
            "/api/v1_alpha/collections/{id}",
            get(collections::show_collection),
        )
        // Statuses (public read)
        .route("/api/v1/statuses/{id}", get(statuses::get_status))
        .route(
            "/api/v1/statuses/{id}/context",
            get(statuses::get_status_context),
        )
        .route(
            "/api/v1/statuses/{id}/favourited_by",
            get(statuses::favourited_by),
        )
        .route(
            "/api/v1/statuses/{id}/reblogged_by",
            get(statuses::reblogged_by),
        )
        .route(
            "/api/v1/statuses/{id}/history",
            get(statuses::get_status_history),
        )
        .route("/api/v1/statuses/{id}/card", get(statuses::get_status_card))
        .route(
            "/api/v1/statuses/{id}/translate",
            post(statuses::translate_status),
        )
        // Polls (public read)
        .route("/api/v1/polls/{id}", get(polls::get_poll))
        // Search
        .route("/api/v2/search", get(search::search))
        // Timelines
        .route("/api/v1/timelines/public", get(timelines::public_timeline))
        .route("/api/v1/timelines/link", get(timelines::link_timeline))
        .route(
            "/api/v1/timelines/tag/{hashtag}",
            get(timelines::tag_timeline),
        )
        // Account registration (Mastodon C2S API)
        .route("/api/v1/accounts", post(signup::api_create_account))
        .route("/auth/signup", get(signup::signup_get))
        // Email confirmation
        .route("/auth/confirm", get(signup::confirm_email))
        // Tags (public)
        .route("/api/v1/tags/{name}", get(tags::get_tag))
        // Trends — tags by uses over seven days, statuses by favourites and
        // boosts over two, links by how many posts carry them.
        .route("/api/v1/trends", get(trends::trending_tags))
        .route("/api/v1/trends/statuses", get(trends::trending_statuses))
        .route("/api/v1/trends/tags", get(trends::trending_tags))
        .route("/api/v1/trends/links", get(trends::trending_links))
        // Custom emojis (public)
        .route(
            "/api/v1/donation_campaigns",
            get(accounts::list_donation_campaigns),
        )
        .route(
            "/api/v1/accounts/{id}/identity_proofs",
            get(accounts::get_account_identity_proofs),
        )
        .route(
            "/api/v1/instance/terms_of_service",
            get(instance::get_terms_of_service),
        )
        .route(
            "/api/v1/instance/terms_of_service/{date}",
            get(instance::get_terms_of_service_by_date),
        )
        .route("/api/oembed", get(oembed::get_oembed))
        .route("/api/v1/peers/search", get(instance::search_peers))
        .route("/api/v1/custom_emojis", get(emojis::list_custom_emojis))
        // Announcements / conversations
        .route(
            "/api/v1/announcements",
            get(announcements::get_announcements),
        )
        .route(
            "/api/v1/announcements/{id}/dismiss",
            post(announcements::dismiss_announcement),
        )
        .route(
            "/api/v1/announcements/{id}/reactions/{name}",
            put(announcements::add_reaction).delete(announcements::remove_reaction),
        )
        .route(
            "/api/v1/conversations",
            get(conversations::get_conversations),
        )
        .route(
            "/api/v1/conversations/{id}",
            delete(conversations::delete_conversation),
        )
        .route(
            "/api/v1/conversations/{id}/read",
            post(conversations::mark_conversation_read),
        )
        .route(
            "/api/v1/conversations/{id}/unread",
            post(conversations::mark_conversation_unread),
        )
        // OAuth
        .route("/api/v1/apps", post(oauth::register_app))
        .route("/api/{server}/login", post(oauth::elk_login))
        .route(
            "/api/{server}/oauth/{origin}",
            get(oauth::elk_oauth_callback),
        )
        .route(
            "/oauth/authorize",
            get(oauth::authorize_form).post(oauth::authorize_submit),
        )
        .route("/oauth/token", post(oauth::issue_token))
        .route("/oauth/revoke", post(oauth::revoke_token));

    Router::new()
        .merge(auth_required)
        .merge(uploads)
        .merge(public)
        .route_layer(middleware::from_fn(mw::api_gates))
}

/// Routes that must NOT be wrapped by CompressionLayer (WebSocket upgrades).
pub fn streaming_router() -> Router {
    Router::new()
        .route("/api/v1/streaming", get(streaming::handler))
        .route(
            "/api/v1/streaming/health",
            get(|| async { axum::http::StatusCode::OK }),
        )
}

async fn empty_object() -> Json<serde_json::Value> {
    Json(serde_json::json!({}))
}

async fn require_auth(
    auth: Option<axum::extract::Extension<mw::AuthenticatedUser>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, crate::error::AppError> {
    if auth.is_none() {
        return Err(crate::error::AppError::Unauthorized);
    }
    Ok(next.run(req).await)
}
