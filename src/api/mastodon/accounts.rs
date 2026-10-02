use axum::{
    extract::{Extension, Path, Query, RawQuery},
    http::{HeaderMap, Uri},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use super::status_serialize::*;
use super::{
    convert::{account_from_db, status_from_db},
    types::{Account as ApiAccount, PaginationParams, Preferences, Relationship, SuggestionV2},
};
use crate::{
    db::models::Account,
    error::{AppError, AppResult},
    feed,
    middleware::{AuthenticatedUser, ResolvedInstance},
    push,
    state::AppState,
};

mod endorsements;
pub use endorsements::{endorse_account, get_endorsements, get_my_endorsements, unendorse_account};
mod search;
pub use search::search_accounts;
mod mutes_blocks;
pub use mutes_blocks::{
    block, block_account, get_blocks, get_mutes, mute, mute_account, unblock, unblock_account,
    unmute, unmute_account,
};
mod follow_requests;
pub use follow_requests::{
    authorize, authorize_follow_request, get_follow_requests, reject_follow_request,
};
mod suggestions;
pub use suggestions::{dismiss_suggestion, get_suggestions, get_suggestions_v2};
mod aliases;
pub use aliases::{
    cancel_redirect, create_alias, create_redirect, delete_alias, list_aliases, move_account,
};
mod relationships;
pub use relationships::{
    follow, follow_account, get_account_followers, get_account_following, get_relationships,
    unfollow, unfollow_account, FollowOptions, FollowOutcome,
};
mod credentials;
pub use credentials::{
    delete_profile_avatar, delete_profile_header, get_preferences, get_profile, patch_profile,
    put_profile, update_credentials, verify_credentials,
};

/// Fetch highlighted roles for a local account.  Returns an empty vec for
/// remote accounts (they have no row in `users`).
pub(super) async fn fetch_account_roles(
    state: &AppState,
    account_id: i64,
) -> Vec<crate::api::mastodon::types::AccountRole> {
    let row = sqlx::query!(
        // Only highlighted roles are exposed publicly, matching Mastodon's
        // AccountSerializer#roles (`filter(&:highlighted?)`).
        r#"SELECT ur.id, ur.name, ur.color
           FROM users u
           JOIN user_roles ur ON ur.id = u.role_id
           WHERE u.account_id = $1 AND ur.highlighted"#,
        account_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten();

    match row {
        Some(r) => vec![crate::api::mastodon::types::AccountRole {
            id: r.id.to_string(),
            name: r.name,
            color: r.color,
        }],
        None => vec![],
    }
}

/// Mastodon's `UserRole::EVERYONE_ROLE_ID`: the role every account has unless
/// given another, which Mastodon creates on demand if it is missing.
pub(super) const EVERYONE_ROLE_ID: i64 = -99;

/// Fetch the current role for a local account (used in `CredentialAccount`).
///
/// Mastodon's `User#role` falls back to `UserRole.everyone` rather than
/// returning nothing, so `verify_credentials` always carries a role and a
/// client can read its permissions without special-casing its absence.
///
/// The role named is the user's own, but `permissions` is the *computed* set —
/// `RoleSerializer` emits `computed_permissions`, which unions in the everyone
/// role. Without that an admin reads as unable to invite anyone: `invite_users`
/// lives on the everyone role, and upstream's Admin role does not repeat it.
pub async fn fetch_account_role(state: &AppState, account_id: i64) -> Option<super::types::Role> {
    let row = sqlx::query!(
        r#"SELECT ur.id, ur.name, ur.color, ur.highlighted, ur.collection_limit
           FROM users u
           JOIN user_roles ur ON ur.id = COALESCE(u.role_id, $2)
           WHERE u.account_id = $1"#,
        account_id,
        EVERYONE_ROLE_ID,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()?;

    let (_, permissions) = super::admin::computed_permissions(state, account_id)
        .await
        .ok()?;

    Some(super::types::Role {
        id: row.id.to_string(),
        name: row.name,
        color: row.color,
        permissions: permissions.to_string(),
        highlighted: row.highlighted,
        collection_limit: row.collection_limit,
    })
}

// ── GET /api/v1/accounts/lookup ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LookupQuery {
    pub acct: String,
    pub resolve: Option<bool>,
}

pub async fn lookup_account(
    state: AppState,
    Query(q): Query<LookupQuery>,
) -> AppResult<Json<ApiAccount>> {
    // acct can be "username" (local) or "username@domain" (remote)
    let (username, domain) = match q.acct.split_once('@') {
        Some((user, domain)) => (user.to_lowercase(), Some(domain.to_lowercase())),
        None => (q.acct.to_lowercase(), None),
    };

    let found = match domain {
        None => {
            sqlx::query_as!(
                Account,
                "SELECT * FROM accounts WHERE lower(username) = $1 AND domain IS NULL",
                username,
            )
            .fetch_optional(&state.db)
            .await?
        }

        Some(ref d) => {
            sqlx::query_as!(
                Account,
                "SELECT * FROM accounts WHERE lower(username) = $1 AND lower(domain) = $2",
                username,
                d,
            )
            .fetch_optional(&state.db)
            .await?
        }
    };

    if let Some(account) = found {
        let mut api = account_from_db(&state.urls, &account);
        api.emojis = fetch_account_emojis(&state, &account).await;
        api.roles = fetch_account_roles(&state, account.id).await;
        apply_account_stats(&state, &mut api, account.id).await;
        api.email_subscriptions = crate::email_subscriptions::serialized(&state, &account).await;
        return Ok(Json(api));
    }

    // Not found locally — attempt WebFinger resolution if requested and domain is known
    if q.resolve.unwrap_or(false) {
        if let Some(ref d) = domain {
            if let Ok(uri) =
                crate::federation::webfinger::resolve_allowed(&state, &username, d).await
            {
                let account_id =
                    crate::api::ap::inbox::resolve_or_fetch_remote_account(&state, &uri).await?;
                let account =
                    sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", account_id,)
                        .fetch_one(&state.db)
                        .await?;
                let mut api = account_from_db(&state.urls, &account);
                api.emojis = fetch_account_emojis(&state, &account).await;
                api.roles = fetch_account_roles(&state, account.id).await;
                return Ok(Json(api));
            }
        }
    }

    Err(AppError::NotFound)
}

// ── GET /api/v1/accounts/:id ───────────────────────────────────────────────

pub async fn get_account(state: AppState, Path(id): Path<i64>) -> AppResult<Json<ApiAccount>> {
    let account = fetch_account(&state, id).await?;
    // Local accounts that are unconfirmed or pending approval are invisible (404).
    // A suspended one is not: Mastodon serves the blanked tombstone with
    // `suspended: true`, and after deletion there is no user row left to check.
    if account.domain.is_none() && !account.is_unavailable() {
        let approval_required = crate::settings::registrations_mode(&state)
            .await
            .approval_required();
        let ok = sqlx::query_scalar!(
            r#"SELECT u.confirmed_at IS NOT NULL
                 AND (u.approved OR NOT $2) AS "ok!"
               FROM users u
               WHERE u.account_id = $1"#,
            account.id,
            approval_required,
        )
        .fetch_optional(&state.db)
        .await?;
        match ok {
            None | Some(false) => return Err(AppError::NotFound),
            _ => {}
        }
    }
    let mut api_account = account_from_db(&state.urls, &account);
    api_account.emojis = fetch_account_emojis(&state, &account).await;
    api_account.roles = fetch_account_roles(&state, account.id).await;
    apply_account_stats(&state, &mut api_account, account.id).await;
    api_account.email_subscriptions =
        crate::email_subscriptions::serialized(&state, &account).await;
    if let Some(moved_account_id) = account.moved_to_account_id {
        if let Ok(Some(moved)) = sqlx::query_as!(
            Account,
            "SELECT * FROM accounts WHERE id = $1 LIMIT 1",
            moved_account_id,
        )
        .fetch_optional(&state.db)
        .await
        {
            let mut moved_api = account_from_db(&state.urls, &moved);
            moved_api.emojis = fetch_account_emojis(&state, &moved).await;
            moved_api.roles = fetch_account_roles(&state, moved.id).await;
            apply_account_stats(&state, &mut moved_api, moved.id).await;
            api_account.moved = Some(Box::new(moved_api));
        }
    }
    Ok(Json(api_account))
}

// ── GET /api/v1/accounts/:id/statuses ─────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct StatusesQuery {
    #[serde(flatten)]
    pub pagination: PaginationParams,
    pub only_media: Option<bool>,
    pub exclude_replies: Option<bool>,
    pub exclude_reblogs: Option<bool>,
    pub exclude_direct: Option<bool>,
    pub pinned: Option<bool>,
    pub tagged: Option<String>,
}

pub async fn get_account_statuses(
    state: AppState,
    Path(id): Path<i64>,
    uri: Uri,
    req_headers: HeaderMap,
    Query(q): Query<StatusesQuery>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<impl IntoResponse> {
    let account = fetch_account(&state, id).await?;
    if account.is_unavailable() {
        return Ok((HeaderMap::new(), Json(Vec::<super::types::Status>::new())));
    }
    let viewer_id = auth.as_ref().map(|Extension(a)| a.account_id);

    // If the target has blocked the viewer, deny access (Mastodon returns 403).
    if let Some(vid) = viewer_id {
        if vid != account.id {
            let blocked = sqlx::query_scalar!(
                "SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2",
                account.id,
                vid,
            )
            .fetch_optional(&state.db)
            .await?
            .is_some();
            if blocked {
                return Err(AppError::Forbidden);
            }
        }
    }

    let is_self = viewer_id == Some(account.id);
    let is_follower = if !is_self {
        if let Some(vid) = viewer_id {
            sqlx::query_scalar!(
                "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
                vid, account.id,
            )
            .fetch_one(&state.db)
            .await?
            .unwrap_or(false)
        } else {
            false
        }
    } else {
        false
    };

    if q.pinned == Some(true) {
        let pinned_statuses = sqlx::query_as!(
            crate::db::models::Status,
            r#"SELECT s.* FROM statuses s
               JOIN status_pins sp ON sp.status_id = s.id
               WHERE sp.account_id = $1 AND s.deleted_at IS NULL
                 AND (
                   s.visibility IN (0, 1)
                   OR ($2::boolean = true)
                   OR ($3::boolean = true AND s.visibility = 2)
                 )
               ORDER BY sp.id DESC"#,
            account.id,
            is_self,
            is_follower,
        )
        .fetch_all(&state.db)
        .await?;
        let pin_ids: Vec<i64> = pinned_statuses
            .iter()
            .map(|s| s.reblog_of_id.unwrap_or(s.id))
            .collect();
        let pin_ctxs = if let Some(vid) = viewer_id {
            super::statuses::batch_viewer_contexts(&state, vid, &pin_ids).await?
        } else {
            std::collections::HashMap::new()
        };
        let pin_status_ids: Vec<i64> = pinned_statuses.iter().map(|s| s.id).collect();
        let pin_media_map = batch_status_media(&state, &pin_status_ids).await?;
        let pin_reblog_map = batch_reblog_data(&state, &pinned_statuses).await?;
        let pin_quote_map = batch_quote_data(&state, &pinned_statuses, viewer_id).await?;
        let pin_reblog_ids: Vec<i64> = pin_reblog_map.values().map(|(rs, _, _)| rs.id).collect();
        let mut pin_enrich_ids = pin_status_ids.clone();
        pin_enrich_ids.extend_from_slice(&pin_reblog_ids);
        let pin_tags_map = batch_statuses_tags(&state, &pin_enrich_ids).await?;
        let pin_mentions_map = batch_status_mentions(&state, &pin_enrich_ids).await?;
        let all_pin_statuses: Vec<crate::db::models::Status> = pinned_statuses
            .iter()
            .cloned()
            .chain(pin_reblog_map.values().map(|(rs, _, _)| rs.clone()))
            .collect();
        let pin_emojis_map = batch_status_emojis(&state, &all_pin_statuses).await?;
        let pin_polls_map = batch_status_polls(&state, &pin_enrich_ids, viewer_id).await?;
        let pin_cards_map = batch_status_cards(&state, &pin_enrich_ids, viewer_id).await?;
        let pin_all_accounts_for_emoji: Vec<crate::db::models::Account> = {
            let mut seen = std::collections::HashSet::new();
            std::iter::once(&account)
                .chain(pin_reblog_map.values().map(|(_, ra, _)| ra))
                .filter(|a| seen.insert(a.id))
                .cloned()
                .collect()
        };
        let pin_account_emojis_map =
            batch_account_emojis(&state, &pin_all_accounts_for_emoji).await;
        let pin_account_roles_map = batch_account_roles(&state, &pin_all_accounts_for_emoji).await;
        let mut result = Vec::with_capacity(pinned_statuses.len());
        for s in &pinned_statuses {
            let media = pin_media_map.get(&s.id).cloned().unwrap_or_default();
            let reblog = pin_reblog_map.get(&s.id).cloned();
            let effective_id = s.reblog_of_id.unwrap_or(s.id);
            let ctx = pin_ctxs.get(&effective_id).cloned();
            let mentions = pin_mentions_map.get(&s.id).cloned().unwrap_or_default();
            let rb_mentions = reblog
                .as_ref()
                .and_then(|(rs, _, _)| pin_mentions_map.get(&rs.id))
                .cloned()
                .unwrap_or_default();
            let mut api_status = status_from_db(
                &state.urls,
                s,
                &account,
                media,
                reblog,
                ctx,
                &mentions,
                &rb_mentions,
            );
            api_status.account.emojis = pin_account_emojis_map
                .get(&account.id)
                .cloned()
                .unwrap_or_default();
            api_status.account.roles = pin_account_roles_map
                .get(&account.id)
                .cloned()
                .unwrap_or_default();
            api_status.tags = pin_tags_map.get(&s.id).cloned().unwrap_or_default();
            api_status.mentions = mentions;
            api_status.emojis = pin_emojis_map.get(&s.id).cloned().unwrap_or_default();
            api_status.poll = pin_polls_map.get(&s.id).cloned();
            api_status.card = pin_cards_map.get(&s.id).cloned();
            api_status.quote = pin_quote_map.get(&s.id).cloned();
            if let Some(ref mut rb) = api_status.reblog {
                let rid: i64 = rb.id.parse().unwrap_or(0);
                let rb_id: i64 = rb.account.id.parse().unwrap_or(0);
                rb.account.emojis = pin_account_emojis_map
                    .get(&rb_id)
                    .cloned()
                    .unwrap_or_default();
                rb.account.roles = pin_account_roles_map
                    .get(&rb_id)
                    .cloned()
                    .unwrap_or_default();
                rb.tags = pin_tags_map.get(&rid).cloned().unwrap_or_default();
                rb.mentions = rb_mentions;
                rb.emojis = pin_emojis_map.get(&rid).cloned().unwrap_or_default();
                rb.poll = pin_polls_map.get(&rid).cloned();
                rb.card = pin_cards_map.get(&rid).cloned();
            }
            api_status.pinned = Some(true);
            result.push(api_status);
        }
        hydrate_status_stats(&state, result.iter_mut()).await;
        return Ok((HeaderMap::new(), Json(result)));
    }

    let limit = q.pagination.limit_clamped(20, 40);
    let max_id = q
        .pagination
        .max_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let since_id = q
        .pagination
        .since_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());
    let min_id = q
        .pagination
        .min_id
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok());

    let tagged_lower = q.tagged.as_deref().map(|t| t.to_lowercase());
    let exclude_direct = q.exclude_direct.unwrap_or(false);
    let statuses = if min_id.is_some() {
        sqlx::query_as!(
            crate::db::models::Status,
            r#"SELECT statuses.* FROM statuses
               WHERE account_id = $1
                 AND deleted_at IS NULL
                 AND ($2::bigint IS NULL OR id > $2)
                 AND ($3::boolean IS NOT TRUE OR reblog_of_id IS NULL)
                 AND ($4::boolean IS NOT TRUE OR in_reply_to_id IS NULL OR in_reply_to_account_id = $1)
                 AND (
                   visibility IN (0, 1)
                   OR ($5::boolean = true)
                   OR ($6::boolean = true AND visibility = 2)
                   OR (
                     NOT $10::boolean
                     AND $11::bigint IS NOT NULL
                     AND visibility = 3
                     AND EXISTS (SELECT 1 FROM mentions WHERE status_id = statuses.id AND account_id = $11)
                   )
                 )
                 AND (
                   text != ''
                   OR reblog_of_id IS NOT NULL
                   OR poll_id IS NOT NULL
                   OR EXISTS (SELECT 1 FROM media_attachments WHERE status_id = statuses.id)
                 )
                 AND ($8::boolean IS NOT TRUE OR
                   EXISTS (SELECT 1 FROM media_attachments WHERE status_id = statuses.id)
                 )
                 AND ($9::text IS NULL OR EXISTS (
                   SELECT 1 FROM statuses_tags st
                   JOIN tags t ON t.id = st.tag_id
                   WHERE st.status_id = statuses.id AND t.name = $9
                 ))
                 AND ($11::bigint IS NULL OR reblog_of_id IS NULL OR NOT EXISTS (
                   SELECT 1 FROM blocks b
                   JOIN statuses orig ON orig.id = statuses.reblog_of_id
                   WHERE b.account_id = $11 AND b.target_account_id = orig.account_id
                 ))
                 AND ($11::bigint IS NULL OR reblog_of_id IS NULL OR NOT EXISTS (
                   SELECT 1 FROM account_domain_blocks adb
                   JOIN statuses orig ON orig.id = statuses.reblog_of_id
                   JOIN accounts orig_a ON orig_a.id = orig.account_id
                   WHERE adb.account_id = $11 AND adb.domain = orig_a.domain
                 ))
               ORDER BY id ASC
               LIMIT $7"#,
            account.id,
            min_id,
            q.exclude_reblogs.unwrap_or(false),
            q.exclude_replies.unwrap_or(false),
            is_self,
            is_follower,
            limit,
            q.only_media.unwrap_or(false),
            tagged_lower,
            exclude_direct,
            viewer_id,
        )
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as!(
            crate::db::models::Status,
            r#"SELECT statuses.* FROM statuses
               WHERE account_id = $1
                 AND deleted_at IS NULL
                 AND ($2::bigint IS NULL OR id < $2)
                 AND ($3::bigint IS NULL OR id > $3)
                 AND ($4::boolean IS NOT TRUE OR reblog_of_id IS NULL)
                 AND ($5::boolean IS NOT TRUE OR in_reply_to_id IS NULL OR in_reply_to_account_id = $1)
                 AND (
                   visibility IN (0, 1)
                   OR ($6::boolean = true)
                   OR ($7::boolean = true AND visibility = 2)
                   OR (
                     NOT $11::boolean
                     AND $12::bigint IS NOT NULL
                     AND visibility = 3
                     AND EXISTS (SELECT 1 FROM mentions WHERE status_id = statuses.id AND account_id = $12)
                   )
                 )
                 AND (
                   text != ''
                   OR reblog_of_id IS NOT NULL
                   OR poll_id IS NOT NULL
                   OR EXISTS (SELECT 1 FROM media_attachments WHERE status_id = statuses.id)
                 )
                 AND ($9::boolean IS NOT TRUE OR
                   EXISTS (SELECT 1 FROM media_attachments WHERE status_id = statuses.id)
                 )
                 AND ($10::text IS NULL OR EXISTS (
                   SELECT 1 FROM statuses_tags st
                   JOIN tags t ON t.id = st.tag_id
                   WHERE st.status_id = statuses.id AND t.name = $10
                 ))
                 AND ($12::bigint IS NULL OR reblog_of_id IS NULL OR NOT EXISTS (
                   SELECT 1 FROM blocks b
                   JOIN statuses orig ON orig.id = statuses.reblog_of_id
                   WHERE b.account_id = $12 AND b.target_account_id = orig.account_id
                 ))
                 AND ($12::bigint IS NULL OR reblog_of_id IS NULL OR NOT EXISTS (
                   SELECT 1 FROM account_domain_blocks adb
                   JOIN statuses orig ON orig.id = statuses.reblog_of_id
                   JOIN accounts orig_a ON orig_a.id = orig.account_id
                   WHERE adb.account_id = $12 AND adb.domain = orig_a.domain
                 ))
               ORDER BY id DESC
               LIMIT $8"#,
            account.id,
            max_id,
            since_id,
            q.exclude_reblogs.unwrap_or(false),
            q.exclude_replies.unwrap_or(false),
            is_self,
            is_follower,
            limit,
            q.only_media.unwrap_or(false),
            tagged_lower,
            exclude_direct,
            viewer_id,
        )
        .fetch_all(&state.db)
        .await?
    };
    // A `min_id` page reads newest first too.
    let statuses = crate::api::mastodon::timelines::newest_first(min_id, statuses);

    let filter_map = if let Some(vid) = viewer_id {
        super::timelines::compute_filter_results(&state.db, vid, &statuses).await
    } else {
        std::collections::HashMap::new()
    };

    let effective_ids: Vec<i64> = statuses
        .iter()
        .map(|s| s.reblog_of_id.unwrap_or(s.id))
        .collect();
    let ctxs = if let Some(vid) = viewer_id {
        super::statuses::batch_viewer_contexts(&state, vid, &effective_ids).await?
    } else {
        std::collections::HashMap::new()
    };

    let all_status_ids: Vec<i64> = statuses.iter().map(|s| s.id).collect();
    let media_map = batch_status_media(&state, &all_status_ids).await?;
    let reblog_map = batch_reblog_data(&state, &statuses).await?;
    let quote_map = batch_quote_data(&state, &statuses, viewer_id).await?;
    let reblog_ids: Vec<i64> = reblog_map.values().map(|(rs, _, _)| rs.id).collect();
    let mut enrich_ids = all_status_ids.clone();
    enrich_ids.extend_from_slice(&reblog_ids);
    let tags_map = batch_statuses_tags(&state, &enrich_ids).await?;
    let mentions_map = batch_status_mentions(&state, &enrich_ids).await?;
    let all_statuses_for_emoji: Vec<crate::db::models::Status> = statuses
        .iter()
        .cloned()
        .chain(reblog_map.values().map(|(rs, _, _)| rs.clone()))
        .collect();
    let emojis_map = batch_status_emojis(&state, &all_statuses_for_emoji).await?;
    let polls_map = batch_status_polls(&state, &enrich_ids, viewer_id).await?;
    let cards_map = batch_status_cards(&state, &enrich_ids, viewer_id).await?;

    let all_accounts_for_emoji: Vec<crate::db::models::Account> = {
        let mut seen = std::collections::HashSet::new();
        std::iter::once(&account)
            .chain(reblog_map.values().map(|(_, ra, _)| ra))
            .filter(|a| seen.insert(a.id))
            .cloned()
            .collect()
    };
    let account_emojis_map = batch_account_emojis(&state, &all_accounts_for_emoji).await;
    let statuses_roles_map = batch_account_roles(&state, &all_accounts_for_emoji).await;

    let mut result = Vec::with_capacity(statuses.len());
    for s in &statuses {
        let media = media_map.get(&s.id).cloned().unwrap_or_default();
        let reblog = reblog_map.get(&s.id).cloned();
        let effective_id = s.reblog_of_id.unwrap_or(s.id);
        let ctx = ctxs.get(&effective_id).cloned();
        let mentions = mentions_map.get(&s.id).cloned().unwrap_or_default();
        let rb_mentions = reblog
            .as_ref()
            .and_then(|(rs, _, _)| mentions_map.get(&rs.id))
            .cloned()
            .unwrap_or_default();
        let mut api = status_from_db(
            &state.urls,
            s,
            &account,
            media,
            reblog,
            ctx,
            &mentions,
            &rb_mentions,
        );
        api.account.emojis = account_emojis_map
            .get(&account.id)
            .cloned()
            .unwrap_or_default();
        api.account.roles = statuses_roles_map
            .get(&account.id)
            .cloned()
            .unwrap_or_default();
        api.tags = tags_map.get(&s.id).cloned().unwrap_or_default();
        api.mentions = mentions;
        api.emojis = emojis_map.get(&s.id).cloned().unwrap_or_default();
        api.poll = polls_map.get(&s.id).cloned();
        api.card = cards_map.get(&s.id).cloned();
        api.quote = quote_map.get(&s.id).cloned();
        if let Some(ref mut rb) = api.reblog {
            let rid: i64 = rb.id.parse().unwrap_or(0);
            let rb_id: i64 = rb.account.id.parse().unwrap_or(0);
            rb.account.emojis = account_emojis_map.get(&rb_id).cloned().unwrap_or_default();
            rb.account.roles = statuses_roles_map.get(&rb_id).cloned().unwrap_or_default();
            rb.tags = tags_map.get(&rid).cloned().unwrap_or_default();
            rb.mentions = rb_mentions;
            rb.emojis = emojis_map.get(&rid).cloned().unwrap_or_default();
            rb.poll = polls_map.get(&rid).cloned();
            rb.card = cards_map.get(&rid).cloned();
        }
        if let Some(filter_json) = filter_map.get(&s.id) {
            if let Some(arr) = filter_json.as_array() {
                if !arr.is_empty() {
                    api.filtered = Some(arr.clone());
                }
            }
        }
        result.push(api);
    }
    hydrate_status_stats(&state, result.iter_mut()).await;

    let bounds = result
        .first()
        .zip(result.last())
        .map(|(n, o)| (n.id.as_str(), o.id.as_str()));
    let resp_headers = super::link_headers(&req_headers, &uri, bounds);
    Ok((resp_headers, Json(result)))
}

// ── GET /api/v1/accounts/:id/followers ────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct FollowersQuery {
    #[serde(flatten)]
    pub pagination: PaginationParams,
}

// ── GET /api/v1/accounts/:id/pins ─────────────────────────────────────────

pub async fn get_account_pins(
    state: AppState,
    Path(id): Path<i64>,
    auth: Option<Extension<AuthenticatedUser>>,
) -> AppResult<Json<Vec<super::types::Status>>> {
    let account = fetch_account(&state, id).await?;
    if account.is_unavailable() {
        return Ok(Json(vec![]));
    }
    let viewer_id = auth.as_ref().map(|Extension(a)| a.account_id);

    // Block check
    if let Some(vid) = viewer_id {
        if vid != account.id {
            let blocked = sqlx::query_scalar!(
                "SELECT 1 FROM blocks WHERE account_id = $1 AND target_account_id = $2",
                account.id,
                vid,
            )
            .fetch_optional(&state.db)
            .await?
            .is_some();
            if blocked {
                return Err(AppError::Forbidden);
            }
        }
    }

    let is_self = viewer_id == Some(account.id);
    let is_follower = if !is_self {
        if let Some(vid) = viewer_id {
            sqlx::query_scalar!(
                "SELECT EXISTS(SELECT 1 FROM follows WHERE account_id = $1 AND target_account_id = $2)",
                vid, account.id,
            ).fetch_one(&state.db).await?.unwrap_or(false)
        } else {
            false
        }
    } else {
        false
    };

    let pinned_statuses = sqlx::query_as!(
        crate::db::models::Status,
        r#"SELECT s.* FROM statuses s
           JOIN status_pins sp ON sp.status_id = s.id
           WHERE sp.account_id = $1 AND s.deleted_at IS NULL
             AND (
               s.visibility IN (0, 1)
               OR ($2::boolean = true)
               OR ($3::boolean = true AND s.visibility = 2)
             )
           ORDER BY sp.id DESC"#,
        account.id,
        is_self,
        is_follower,
    )
    .fetch_all(&state.db)
    .await?;

    let pin_filter_map = if let Some(vid) = viewer_id {
        super::timelines::compute_filter_results(&state.db, vid, &pinned_statuses).await
    } else {
        std::collections::HashMap::new()
    };

    let pin_status_ids: Vec<i64> = pinned_statuses.iter().map(|s| s.id).collect();
    let pin_ids: Vec<i64> = pinned_statuses
        .iter()
        .map(|s| s.reblog_of_id.unwrap_or(s.id))
        .collect();
    let pin_ctxs = if let Some(vid) = viewer_id {
        super::statuses::batch_viewer_contexts(&state, vid, &pin_ids).await?
    } else {
        std::collections::HashMap::new()
    };
    let pin_media_map = batch_status_media(&state, &pin_status_ids).await?;
    let pin_reblog_map = batch_reblog_data(&state, &pinned_statuses).await?;
    let pin_quote_map = batch_quote_data(&state, &pinned_statuses, viewer_id).await?;
    let pin_reblog_ids: Vec<i64> = pin_reblog_map.values().map(|(rs, _, _)| rs.id).collect();
    let mut pin_enrich_ids = pin_status_ids.clone();
    pin_enrich_ids.extend_from_slice(&pin_reblog_ids);
    let pin_tags_map = batch_statuses_tags(&state, &pin_enrich_ids).await?;
    let pin_mentions_map = batch_status_mentions(&state, &pin_enrich_ids).await?;
    let all_pin_statuses: Vec<crate::db::models::Status> = pinned_statuses
        .iter()
        .cloned()
        .chain(pin_reblog_map.values().map(|(rs, _, _)| rs.clone()))
        .collect();
    let pin_emojis_map = batch_status_emojis(&state, &all_pin_statuses).await?;
    let pin_polls_map = batch_status_polls(&state, &pin_enrich_ids, viewer_id).await?;
    let pin_cards_map = batch_status_cards(&state, &pin_enrich_ids, viewer_id).await?;
    let pin_all_accounts_for_emoji: Vec<crate::db::models::Account> = {
        let mut seen = std::collections::HashSet::new();
        std::iter::once(&account)
            .chain(pin_reblog_map.values().map(|(_, ra, _)| ra))
            .filter(|a| seen.insert(a.id))
            .cloned()
            .collect()
    };
    let pin_account_emojis_map = batch_account_emojis(&state, &pin_all_accounts_for_emoji).await;
    let pin_account_roles_map = batch_account_roles(&state, &pin_all_accounts_for_emoji).await;

    let mut result = Vec::with_capacity(pinned_statuses.len());
    for s in &pinned_statuses {
        let media = pin_media_map.get(&s.id).cloned().unwrap_or_default();
        let reblog = pin_reblog_map.get(&s.id).cloned();
        let effective_id = s.reblog_of_id.unwrap_or(s.id);
        let ctx = pin_ctxs.get(&effective_id).cloned();
        let mentions = pin_mentions_map.get(&s.id).cloned().unwrap_or_default();
        let rb_mentions = reblog
            .as_ref()
            .and_then(|(rs, _, _)| pin_mentions_map.get(&rs.id))
            .cloned()
            .unwrap_or_default();
        let mut api_status = status_from_db(
            &state.urls,
            s,
            &account,
            media,
            reblog,
            ctx,
            &mentions,
            &rb_mentions,
        );
        api_status.account.emojis = pin_account_emojis_map
            .get(&account.id)
            .cloned()
            .unwrap_or_default();
        api_status.account.roles = pin_account_roles_map
            .get(&account.id)
            .cloned()
            .unwrap_or_default();
        api_status.tags = pin_tags_map.get(&s.id).cloned().unwrap_or_default();
        api_status.mentions = mentions;
        api_status.emojis = pin_emojis_map.get(&s.id).cloned().unwrap_or_default();
        api_status.poll = pin_polls_map.get(&s.id).cloned();
        api_status.card = pin_cards_map.get(&s.id).cloned();
        api_status.quote = pin_quote_map.get(&s.id).cloned();
        if let Some(ref mut rb) = api_status.reblog {
            let rid: i64 = rb.id.parse().unwrap_or(0);
            let rb_id: i64 = rb.account.id.parse().unwrap_or(0);
            rb.account.emojis = pin_account_emojis_map
                .get(&rb_id)
                .cloned()
                .unwrap_or_default();
            rb.account.roles = pin_account_roles_map
                .get(&rb_id)
                .cloned()
                .unwrap_or_default();
            rb.tags = pin_tags_map.get(&rid).cloned().unwrap_or_default();
            rb.mentions = rb_mentions;
            rb.emojis = pin_emojis_map.get(&rid).cloned().unwrap_or_default();
            rb.poll = pin_polls_map.get(&rid).cloned();
            rb.card = pin_cards_map.get(&rid).cloned();
        }
        if let Some(filter_json) = pin_filter_map.get(&s.id) {
            if let Some(arr) = filter_json.as_array() {
                if !arr.is_empty() {
                    api_status.filtered = Some(arr.clone());
                }
            }
        }
        api_status.pinned = Some(true);
        result.push(api_status);
    }
    Ok(Json(result))
}

// ── GET /api/v1/accounts/search ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AccountSearchQuery {
    pub q: Option<String>,
    pub limit: Option<String>,
    pub offset: Option<String>,
    pub resolve: Option<super::extractors::FlexBool>,
    pub following: Option<super::extractors::FlexBool>,
}

// ── Helpers ────────────────────────────────────────────────────────────────

pub async fn fetch_account(state: &AppState, id: i64) -> AppResult<Account> {
    sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)
}

async fn batch_build_relationships(
    state: &AppState,
    source_id: i64,
    target_ids: &[i64],
) -> AppResult<Vec<Relationship>> {
    struct FollowRow {
        show_reblogs: bool,
        notify: bool,
        languages: Option<Vec<String>>,
    }
    struct MuteRow {
        hide_notifications: bool,
        expires_at: Option<chrono::NaiveDateTime>,
    }

    // Accepted follows (outgoing)
    let follows_out = sqlx::query!(
        "SELECT target_account_id, show_reblogs, notify, languages FROM follows WHERE account_id = $1 AND target_account_id = ANY($2::bigint[])",
        source_id, target_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let follows_out_map: std::collections::HashMap<i64, _> = follows_out
        .into_iter()
        .map(|r| {
            (
                r.target_account_id,
                FollowRow {
                    show_reblogs: r.show_reblogs,
                    notify: r.notify,
                    languages: r.languages.filter(|l| !l.is_empty()),
                },
            )
        })
        .collect();

    // Pending follow requests (outgoing)
    let follow_requests_out: std::collections::HashSet<i64> = sqlx::query_scalar!(
        "SELECT target_account_id FROM follow_requests WHERE account_id = $1 AND target_account_id = ANY($2::bigint[])",
        source_id, target_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    // Accepted follows (incoming)
    let followed_by_set: std::collections::HashSet<i64> = sqlx::query_scalar!(
        "SELECT account_id FROM follows WHERE target_account_id = $1 AND account_id = ANY($2::bigint[])",
        source_id, target_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    // Pending follow requests (incoming)
    let requested_by_set: std::collections::HashSet<i64> = sqlx::query_scalar!(
        "SELECT account_id FROM follow_requests WHERE target_account_id = $1 AND account_id = ANY($2::bigint[])",
        source_id, target_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    let blocks_out: std::collections::HashSet<i64> = sqlx::query_scalar!(
        "SELECT target_account_id FROM blocks WHERE account_id = $1 AND target_account_id = ANY($2::bigint[])",
        source_id, target_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    let blocks_in: std::collections::HashSet<i64> = sqlx::query_scalar!(
        "SELECT account_id FROM blocks WHERE target_account_id = $1 AND account_id = ANY($2::bigint[])",
        source_id, target_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    let mutes = sqlx::query!(
        "SELECT target_account_id, hide_notifications, expires_at FROM mutes WHERE account_id = $1 AND target_account_id = ANY($2::bigint[]) AND (expires_at IS NULL OR expires_at > now())",
        source_id, target_ids,
    )
    .fetch_all(&state.db)
    .await?;
    let mutes_map: std::collections::HashMap<i64, MuteRow> = mutes
        .into_iter()
        .map(|r| {
            (
                r.target_account_id,
                MuteRow {
                    hide_notifications: r.hide_notifications,
                    expires_at: r.expires_at,
                },
            )
        })
        .collect();

    let target_domains: std::collections::HashMap<i64, Option<String>> = sqlx::query!(
        "SELECT id, domain FROM accounts WHERE id = ANY($1::bigint[])",
        target_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| (r.id, r.domain))
    .collect();

    let domains_to_check: Vec<String> = target_domains.values().filter_map(|d| d.clone()).collect();
    let domain_blocked_set: std::collections::HashSet<String> = if domains_to_check.is_empty() {
        Default::default()
    } else {
        sqlx::query_scalar!(
            "SELECT domain FROM account_domain_blocks WHERE account_id = $1 AND domain = ANY($2)",
            source_id,
            &domains_to_check,
        )
        .fetch_all(&state.db)
        .await?
        .into_iter()
        .collect()
    };

    let notes: std::collections::HashMap<i64, String> = sqlx::query!(
        "SELECT target_account_id, comment FROM account_notes WHERE account_id = $1 AND target_account_id = ANY($2::bigint[])",
        source_id, target_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| (r.target_account_id, r.comment))
    .collect();

    let endorsed_set: std::collections::HashSet<i64> = sqlx::query_scalar!(
        "SELECT target_account_id FROM account_pins WHERE account_id = $1 AND target_account_id = ANY($2::bigint[])",
        source_id, target_ids,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .collect();

    let mut results = Vec::with_capacity(target_ids.len());
    for &target_id in target_ids {
        let follow = follows_out_map.get(&target_id);
        let mute = mutes_map.get(&target_id);
        let domain = target_domains.get(&target_id).and_then(|d| d.clone());
        let domain_blocking = domain.is_some_and(|d| domain_blocked_set.contains(&d));
        results.push(Relationship {
            id: target_id.to_string(),
            following: follow.is_some(),
            showing_reblogs: follow.is_some_and(|f| f.show_reblogs),
            notifying: follow.is_some_and(|f| f.notify),
            languages: follow.and_then(|f| f.languages.clone()),
            followed_by: followed_by_set.contains(&target_id),
            blocking: blocks_out.contains(&target_id),
            blocked_by: blocks_in.contains(&target_id),
            muting: mute.is_some(),
            muting_notifications: mute.is_some_and(|m| m.hide_notifications),
            muting_expires_at: mute
                .and_then(|m| m.expires_at)
                .map(super::convert::mastodon_date),
            requested: follow_requests_out.contains(&target_id),
            requested_by: requested_by_set.contains(&target_id),
            domain_blocking,
            endorsed: endorsed_set.contains(&target_id),
            note: notes.get(&target_id).cloned().unwrap_or_default(),
        });
    }
    Ok(results)
}

pub(super) async fn build_relationship(
    state: &AppState,
    source_id: i64,
    target_id: i64,
) -> AppResult<Relationship> {
    // Check accepted follow (source → target)
    let follow = sqlx::query!(
        "SELECT show_reblogs, notify, languages FROM follows WHERE account_id = $1 AND target_account_id = $2",
        source_id, target_id
    )
    .fetch_optional(&state.db)
    .await?;

    // Check pending follow request (source → target)
    let requested = sqlx::query!(
        "SELECT 1 as exists FROM follow_requests WHERE account_id = $1 AND target_account_id = $2",
        source_id,
        target_id
    )
    .fetch_optional(&state.db)
    .await?
    .is_some();

    let followed_by = sqlx::query!(
        "SELECT 1 as exists FROM follows WHERE account_id = $1 AND target_account_id = $2",
        target_id,
        source_id
    )
    .fetch_optional(&state.db)
    .await?
    .is_some();

    let blocking = sqlx::query!(
        "SELECT 1 as exists FROM blocks WHERE account_id = $1 AND target_account_id = $2",
        source_id,
        target_id
    )
    .fetch_optional(&state.db)
    .await?
    .is_some();

    let blocked_by = sqlx::query!(
        "SELECT 1 as exists FROM blocks WHERE account_id = $1 AND target_account_id = $2",
        target_id,
        source_id
    )
    .fetch_optional(&state.db)
    .await?
    .is_some();

    let requested_by = sqlx::query!(
        "SELECT 1 as exists FROM follow_requests WHERE account_id = $1 AND target_account_id = $2",
        target_id,
        source_id
    )
    .fetch_optional(&state.db)
    .await?
    .is_some();

    let muting = sqlx::query!(
        "SELECT hide_notifications, expires_at FROM mutes WHERE account_id = $1 AND target_account_id = $2 AND (expires_at IS NULL OR expires_at > now())",
        source_id, target_id
    )
    .fetch_optional(&state.db)
    .await?;

    // Check if source has domain-blocked target's domain
    let target_domain = sqlx::query_scalar!("SELECT domain FROM accounts WHERE id = $1", target_id)
        .fetch_optional(&state.db)
        .await?
        .flatten();

    let domain_blocking = if let Some(domain) = target_domain {
        sqlx::query!(
            "SELECT 1 as exists FROM account_domain_blocks WHERE account_id = $1 AND domain = $2",
            source_id,
            domain
        )
        .fetch_optional(&state.db)
        .await?
        .is_some()
    } else {
        false
    };

    let note = sqlx::query_scalar!(
        "SELECT comment FROM account_notes WHERE account_id = $1 AND target_account_id = $2",
        source_id,
        target_id
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or_default();

    let showing_reblogs = follow.as_ref().is_some_and(|f| f.show_reblogs);
    let notifying = follow.as_ref().is_some_and(|f| f.notify);
    let languages = follow
        .as_ref()
        .and_then(|f| f.languages.clone().filter(|l| !l.is_empty()));
    let muting_expires_at = muting
        .as_ref()
        .and_then(|m| m.expires_at)
        .map(super::convert::mastodon_date);

    Ok(Relationship {
        id: target_id.to_string(),
        following: follow.is_some(),
        showing_reblogs,
        notifying,
        languages,
        followed_by,
        blocking,
        blocked_by,
        muting: muting.is_some(),
        muting_notifications: muting.is_some_and(|m| m.hide_notifications),
        muting_expires_at,
        requested,
        requested_by,
        domain_blocking,
        endorsed: sqlx::query!(
            "SELECT 1 AS e FROM account_pins WHERE account_id = $1 AND target_account_id = $2",
            source_id,
            target_id
        )
        .fetch_optional(&state.db)
        .await?
        .is_some(),
        note,
    })
}

#[derive(Debug, Deserialize)]
pub struct NoteForm {
    pub comment: Option<String>,
}

pub async fn set_account_note(
    state: AppState,
    Path(target_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(form): Json<NoteForm>,
) -> AppResult<Json<Relationship>> {
    auth.require_scope("write:accounts")?;
    let comment = form.comment.unwrap_or_default();
    // Mastodon AccountNote::COMMENT_SIZE_LIMIT.
    if comment.chars().count() > 2_000 {
        return Err(AppError::Unprocessable(
            "Validation failed: Comment is too long (maximum is 2000 characters)".into(),
        ));
    }
    if comment.trim().is_empty() {
        sqlx::query!(
            "DELETE FROM account_notes WHERE account_id = $1 AND target_account_id = $2",
            auth.account_id,
            target_id,
        )
        .execute(&state.db)
        .await?;
    } else {
        sqlx::query!(
            r#"INSERT INTO account_notes (account_id, target_account_id, comment, created_at, updated_at)
               VALUES ($1, $2, $3, now(), now())
               ON CONFLICT (account_id, target_account_id)
               DO UPDATE SET comment = EXCLUDED.comment, updated_at = now()"#,
            auth.account_id, target_id, comment,
        )
        .execute(&state.db)
        .await?;
    }

    build_relationship(&state, auth.account_id, target_id)
        .await
        .map(Json)
}

// ── POST /api/v1/accounts/:id/remove_from_followers ───────────────────────

pub async fn remove_from_followers(
    state: AppState,
    Path(requester_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Relationship>> {
    auth.require_scope("write:follows")?;
    let deleted = sqlx::query!(
        "DELETE FROM follows WHERE account_id = $1 AND target_account_id = $2 RETURNING uri",
        requester_id,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?;

    if let Some(ref row) = deleted {
        // `AccountStat`'s `update_index('accounts', :account)`.
        crate::search::elasticsearch::indexing::accounts(&state, &[requester_id, auth.account_id])
            .await;
        crate::counters::on_follow_removed(&state.db, requester_id, auth.account_id).await?;

        // Tell a removed remote follower they're no longer following us
        // (Mastodon RemoveFromFollowersService → Reject(Follow)).
        if let Some(follow_uri) = row.uri.clone().filter(|s| !s.is_empty()) {
            let remover = fetch_account(&state, auth.account_id).await?;
            let follower = fetch_account(&state, requester_id).await?;
            if follower.domain.is_some()
                && crate::federation::keypair::has_signing_key(&state, remover.id)
                    .await
                    .unwrap_or(false)
            {
                let actor_url =
                    crate::federation::tag::account_uri_of(&state.instance.domain, &remover);
                let key_id = format!("{actor_url}#main-key");
                let reject_id = format!(
                    "https://{}/activities/{}",
                    state.instance.domain,
                    crate::snowflake::next_id()
                );
                if let Ok(activity) = crate::federation::activity::reject_follow(
                    &reject_id,
                    &actor_url,
                    &follow_uri,
                    follower.uri.as_deref().unwrap_or_default(),
                    &actor_url,
                ) {
                    let inbox = if !follower.shared_inbox_url.is_empty() {
                        follower.shared_inbox_url.clone()
                    } else {
                        follower.inbox_url.clone()
                    };
                    if !inbox.is_empty() {
                        if let Err(e) = crate::federation::delivery::deliver_to_inboxes(
                            &state,
                            activity,
                            vec![inbox],
                            key_id,
                        )
                        .await
                        {
                            tracing::warn!(error = %e, "failed to enqueue Reject(Follow) for removed follower");
                        }
                    }
                }
            }
        }
    }

    build_relationship(&state, auth.account_id, requester_id)
        .await
        .map(Json)
}

// ── GET /api/v1/accounts/:id/featured_tags ───────────────────────────────

pub async fn get_account_featured_tags(
    state: AppState,
    Extension(crate::middleware::ResolvedInstance(instance)): Extension<
        crate::middleware::ResolvedInstance,
    >,
    Path(id): Path<i64>,
) -> AppResult<Json<Vec<super::types::FeaturedTag>>> {
    let domain = &instance.domain;
    let rows = sqlx::query!(
        r#"SELECT ft.id, t.name, ft.statuses_count, ft.last_status_at, a.username, a.domain
           FROM featured_tags ft
           JOIN tags t ON t.id = ft.tag_id
           JOIN accounts a ON a.id = ft.account_id
           WHERE ft.account_id = $1
           ORDER BY ft.id"#,
        id,
    )
    .fetch_all(&state.db)
    .await?;
    let tags = rows
        .into_iter()
        .map(|r| {
            let url = if let Some(ref acct_domain) = r.domain {
                format!(
                    "https://{}/@{}@{}/tagged/{}",
                    domain, r.username, acct_domain, r.name
                )
            } else {
                format!("https://{}/@{}/tagged/{}", domain, r.username, r.name)
            };
            super::types::FeaturedTag {
                id: r.id.to_string(),
                name: r.name.clone(),
                url,
                statuses_count: r.statuses_count.to_string(),
                last_status_at: r.last_status_at.map(|t| t.format("%Y-%m-%d").to_string()),
            }
        })
        .collect();
    Ok(Json(tags))
}

// ── GET /api/v1/accounts/familiar_followers ──────────────────────────────

pub async fn get_familiar_followers(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    RawQuery(qs): RawQuery,
) -> AppResult<Json<Vec<super::types::FamiliarFollowers>>> {
    auth.require_scope("read:follows")?;
    let mut seen = std::collections::HashSet::new();
    let ids: Vec<i64> = url::form_urlencoded::parse(qs.as_deref().unwrap_or("").as_bytes())
        .filter(|(k, _)| k == "id[]" || k == "id")
        .filter_map(|(_, v)| v.parse::<i64>().ok())
        .filter(|id| seen.insert(*id))
        .collect();

    let mut result = Vec::with_capacity(ids.len());
    for target_id in &ids {
        // Accounts the viewer follows that also follow the target. When the
        // target hides their followers (hide_collections), Mastodon reveals no
        // familiar followers for it.
        let target_hides = sqlx::query_scalar!(
            r#"SELECT COALESCE(hide_collections, false) FROM accounts WHERE id = $1"#,
            target_id,
        )
        .fetch_optional(&state.db)
        .await?
        .flatten()
        .unwrap_or(false);

        let accounts = if target_hides {
            Vec::new()
        } else {
            sqlx::query_as!(
                crate::db::models::Account,
                r#"SELECT a.* FROM accounts a
                   JOIN follows f1 ON f1.account_id = a.id AND f1.target_account_id = $1
                   JOIN follows f2 ON f2.account_id = $2 AND f2.target_account_id = a.id
                   WHERE a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                   LIMIT 10"#,
                target_id,
                auth.account_id,
            )
            .fetch_all(&state.db)
            .await
            .unwrap_or_default()
        };

        result.push(super::types::FamiliarFollowers {
            id: target_id.to_string(),
            accounts: batch_accounts_to_api(&state, &accounts).await,
        });
    }
    Ok(Json(result))
}

// ── GET /api/v1/directory ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DirectoryQuery {
    pub offset: Option<i64>,
    pub limit: Option<i64>,
    pub order: Option<String>,
    pub local: Option<bool>,
}

pub async fn get_directory(
    state: AppState,
    Query(q): Query<DirectoryQuery>,
) -> AppResult<Json<Vec<ApiAccount>>> {
    // `Api::V1::DirectoriesController#require_enabled!`.
    if !crate::settings::boolean(&state, "profile_directory").await {
        return Err(AppError::NotFound);
    }
    let limit = q.limit.unwrap_or(40).clamp(1, 80);
    let offset = q.offset.unwrap_or(0).max(0);
    let local_only = q.local.unwrap_or(true);
    let order = q.order.as_deref().unwrap_or("active");

    let accounts = if order == "new" {
        sqlx::query_as!(
            Account,
            r#"SELECT * FROM accounts
               WHERE discoverable = true
                 AND suspended_at IS NULL AND requested_deletion_at IS NULL
                 AND silenced_at IS NULL
                 AND (NOT $1::bool OR domain IS NULL)
               ORDER BY created_at DESC
               LIMIT $2 OFFSET $3"#,
            local_only,
            limit,
            offset,
        )
        .fetch_all(&state.db)
        .await?
    } else {
        sqlx::query_as!(
            Account,
            r#"SELECT a.* FROM accounts a
               WHERE a.discoverable = true
                 AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
                 AND a.silenced_at IS NULL
                 AND (NOT $1::bool OR a.domain IS NULL)
               ORDER BY (
                   SELECT MAX(s.created_at) FROM statuses s
                   WHERE s.account_id = a.id AND s.deleted_at IS NULL
               ) DESC NULLS LAST
               LIMIT $2 OFFSET $3"#,
            local_only,
            limit,
            offset,
        )
        .fetch_all(&state.db)
        .await?
    };

    Ok(Json(batch_accounts_to_api(&state, &accounts).await))
}

// ── GET /api/v1/accounts (batch lookup) ──────────────────────────────────

pub async fn get_accounts_batch(
    state: AppState,
    RawQuery(qs): RawQuery,
) -> AppResult<Json<Vec<ApiAccount>>> {
    // serde_urlencoded treats id[]=v1&id[]=v2 as a duplicate field → 400.
    // Parse with form_urlencoded which correctly returns each pair separately.
    let ids: Vec<i64> = url::form_urlencoded::parse(qs.as_deref().unwrap_or("").as_bytes())
        .filter(|(k, _)| k == "id[]" || k == "id")
        .filter_map(|(_, v)| v.parse::<i64>().ok())
        .collect();

    if ids.is_empty() {
        return Ok(Json(vec![]));
    }
    let accounts = sqlx::query_as!(
        crate::db::models::Account,
        "SELECT * FROM accounts WHERE id = ANY($1::bigint[]) ORDER BY created_at DESC",
        &ids,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(batch_accounts_to_api(&state, &accounts).await))
}

// ── GET /api/v1/accounts/:id/lists ───────────────────────────────────────

pub async fn get_account_lists(
    state: AppState,
    Path(target_id): Path<i64>,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<super::types::List>>> {
    auth.require_scope("read:lists")?;
    let rows = sqlx::query!(
        r#"SELECT l.id, l.title, l.exclusive,
                  CASE l.replies_policy WHEN 0 THEN 'list' WHEN 1 THEN 'followed' WHEN 2 THEN 'none' ELSE 'list' END AS "replies_policy!"
           FROM lists l
           JOIN list_accounts la ON la.list_id = l.id
           WHERE l.account_id = $1 AND la.account_id = $2
           ORDER BY l.id"#,
        auth.account_id,
        target_id,
    )
    .fetch_all(&state.db)
    .await?;

    let lists = rows
        .into_iter()
        .map(|r| super::types::List {
            id: r.id.to_string(),
            title: r.title,
            replies_policy: r.replies_policy,
            exclusive: r.exclusive,
        })
        .collect();

    Ok(Json(lists))
}

// ── Tag / mention fetchers ─────────────────────────────────────────────────

/// Extract `:shortcode:` patterns from account profile fields and look them up.
pub async fn fetch_account_emojis(state: &AppState, a: &Account) -> Vec<super::types::CustomEmoji> {
    let mut combined = format!("{} {}", a.display_name, a.note);
    if let Some(fields) = a.fields.as_ref().and_then(|f| f.as_array()) {
        for f in fields {
            if let (Some(n), Some(v)) = (f["name"].as_str(), f["value"].as_str()) {
                combined.push(' ');
                combined.push_str(n);
                combined.push(' ');
                combined.push_str(v);
            }
        }
    }
    let mut shortcodes: Vec<String> = Vec::new();
    let mut rest = combined.as_str();
    while let Some(start) = rest.find(':') {
        rest = &rest[start + 1..];
        if let Some(end) = rest.find(':') {
            let code = &rest[..end];
            if !code.is_empty() && code.chars().all(|c| c.is_alphanumeric() || c == '_') {
                shortcodes.push(code.to_string());
            }
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    if shortcodes.is_empty() {
        return vec![];
    }
    let rows = sqlx::query!(
        r#"SELECT shortcode, image_remote_url, visible_in_picker
           FROM custom_emojis
           WHERE shortcode = ANY($1) AND domain IS NULL AND NOT disabled"#,
        &shortcodes,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    rows.into_iter()
        .map(|r| {
            let url = r.image_remote_url.unwrap_or_default();
            super::types::CustomEmoji {
                shortcode: r.shortcode,
                url: url.clone(),
                static_url: url,
                visible_in_picker: r.visible_in_picker,
                category: None,
                featured: None,
            }
        })
        .collect()
}

/// Extract emoji shortcodes from account profile text.
fn extract_account_shortcodes(a: &Account) -> Vec<String> {
    let mut combined = format!("{} {}", a.display_name, a.note);
    if let Some(fields) = a.fields.as_ref().and_then(|f| f.as_array()) {
        for f in fields {
            if let (Some(n), Some(v)) = (f["name"].as_str(), f["value"].as_str()) {
                combined.push(' ');
                combined.push_str(n);
                combined.push(' ');
                combined.push_str(v);
            }
        }
    }
    let mut codes = Vec::new();
    let mut rest = combined.as_str();
    while let Some(start) = rest.find(':') {
        rest = &rest[start + 1..];
        if let Some(end) = rest.find(':') {
            let code = &rest[..end];
            if !code.is_empty() && code.chars().all(|c| c.is_alphanumeric() || c == '_') {
                codes.push(code.to_string());
            }
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    codes
}

/// Batch-fetch account profile emojis for multiple accounts in one DB query.
pub async fn batch_account_emojis(
    state: &AppState,
    accounts: &[Account],
) -> std::collections::HashMap<i64, Vec<super::types::CustomEmoji>> {
    if accounts.is_empty() {
        return std::collections::HashMap::new();
    }

    // Collect all shortcodes per account
    let mut account_shortcodes: std::collections::HashMap<i64, Vec<String>> =
        std::collections::HashMap::new();
    let mut all_shortcodes_set: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for a in accounts {
        let codes = extract_account_shortcodes(a);
        if !codes.is_empty() {
            all_shortcodes_set.extend(codes.iter().cloned());
            account_shortcodes.insert(a.id, codes);
        }
    }

    if all_shortcodes_set.is_empty() {
        return std::collections::HashMap::new();
    }

    let all_shortcodes: Vec<String> = all_shortcodes_set.into_iter().collect();

    let rows = sqlx::query!(
        r#"SELECT shortcode, image_remote_url, visible_in_picker
           FROM custom_emojis
           WHERE disabled = false
             AND domain IS NULL
             AND shortcode = ANY($1)"#,
        &all_shortcodes as &[String],
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();

    // Build a map: shortcode → CustomEmoji
    let emoji_lookup: std::collections::HashMap<String, super::types::CustomEmoji> = rows
        .into_iter()
        .map(|r| {
            let url = r.image_remote_url.unwrap_or_default();
            let emoji = super::types::CustomEmoji {
                shortcode: r.shortcode.clone(),
                url: url.clone(),
                static_url: url,
                visible_in_picker: r.visible_in_picker,
                category: None,
                featured: None,
            };
            (r.shortcode, emoji)
        })
        .collect();

    // Build result map: account_id → emojis
    let mut result = std::collections::HashMap::new();
    for a in accounts {
        if let Some(codes) = account_shortcodes.get(&a.id) {
            let mut seen = std::collections::HashSet::new();
            let emojis: Vec<_> = codes
                .iter()
                .filter(|code| seen.insert(*code))
                .filter_map(|code| emoji_lookup.get(code).cloned())
                .collect();
            if !emojis.is_empty() {
                result.insert(a.id, emojis);
            }
        }
    }
    result
}

/// Batch-fetch role badges for a set of accounts. Only local accounts (domain IS NULL)
/// can have roles. Returns a map of account_id → role list (empty for non-admin/moderator).
pub async fn batch_account_roles(
    state: &AppState,
    accounts: &[Account],
) -> std::collections::HashMap<i64, Vec<super::types::AccountRole>> {
    let local_ids: Vec<i64> = accounts
        .iter()
        .filter(|a| a.domain.is_none())
        .map(|a| a.id)
        .collect();
    if local_ids.is_empty() {
        return std::collections::HashMap::new();
    }
    sqlx::query!(
        // Only highlighted roles are exposed publicly, matching Mastodon's
        // AccountSerializer#roles (`filter(&:highlighted?)`).
        r#"SELECT u.account_id, ur.id AS "role_id!", ur.name AS "role_name!", ur.color AS "role_color!"
           FROM users u
           JOIN user_roles ur ON ur.id = u.role_id
           WHERE u.account_id = ANY($1::bigint[]) AND ur.highlighted"#,
        &local_ids,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        let roles = vec![super::types::AccountRole {
            id: r.role_id.to_string(),
            name: r.role_name,
            color: r.role_color,
        }];
        (r.account_id, roles)
    })
    .collect()
}

/// Batch-fetch `account_stats` for the given account ids.
/// Returns a map of `account_id` → `(statuses_count, following_count, followers_count)`.
/// Accounts with no stats row are absent from the map (callers default to 0).
pub async fn batch_account_stats(
    state: &AppState,
    account_ids: &[i64],
) -> std::collections::HashMap<i64, (i64, i64, i64)> {
    if account_ids.is_empty() {
        return std::collections::HashMap::new();
    }
    sqlx::query!(
        "SELECT account_id, statuses_count, following_count, followers_count
         FROM account_stats WHERE account_id = ANY($1::bigint[])",
        account_ids,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        (
            r.account_id,
            (r.statuses_count, r.following_count, r.followers_count),
        )
    })
    .collect()
}

/// `Account#user_prefers_noindex?` for each local account among `account_ids`:
/// the user's `noindex` setting, or `Setting.noindex` when they never chose.
pub async fn batch_noindex(
    state: &AppState,
    account_ids: &[i64],
) -> std::collections::HashMap<i64, bool> {
    if account_ids.is_empty() {
        return std::collections::HashMap::new();
    }
    let site_default = crate::settings::boolean(state, "noindex").await;
    sqlx::query!(
        "SELECT account_id, settings FROM users WHERE account_id = ANY($1::bigint[])",
        account_ids,
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        (
            r.account_id,
            crate::accounts::user_setting_bool(r.settings.as_deref(), "noindex", site_default),
        )
    })
    .collect()
}

/// Convert a slice of DB accounts to API accounts with profile emojis and roles populated.
/// [`batch_accounts_to_api`] for one account.
pub async fn account_to_api(state: &AppState, account: &Account) -> super::types::Account {
    batch_accounts_to_api(state, std::slice::from_ref(account))
        .await
        .pop()
        .unwrap_or_else(|| super::convert::account_from_db(&state.urls, account))
}

pub async fn batch_accounts_to_api(
    state: &AppState,
    accounts: &[Account],
) -> Vec<super::types::Account> {
    let emojis_map = batch_account_emojis(state, accounts).await;
    let roles_map = batch_account_roles(state, accounts).await;
    let ids: Vec<i64> = accounts.iter().map(|a| a.id).collect();
    let stats_map = batch_account_stats(state, &ids).await;
    let noindex_map = batch_noindex(state, &ids).await;
    // `AccountSerializer#email_subscriptions`, while the feature is enabled.
    let email_subscriptions = if crate::email_subscriptions::enabled(state).await {
        let local: Vec<i64> = accounts
            .iter()
            .filter(|a| a.domain.is_none())
            .map(|a| a.id)
            .collect();
        Some(crate::email_subscriptions::offering(state, &local).await)
    } else {
        None
    };
    let mut api_accounts: Vec<super::types::Account> = accounts
        .iter()
        .map(|a| {
            let mut api = super::convert::account_from_db(&state.urls, a);
            api.emojis = emojis_map.get(&a.id).cloned().unwrap_or_default();
            api.roles = roles_map.get(&a.id).cloned().unwrap_or_default();
            api.email_subscriptions = email_subscriptions
                .as_ref()
                .map(|offering| offering.contains(&a.id));
            if let Some(&noindex) = noindex_map.get(&a.id) {
                api.noindex = Some(noindex);
            }
            if let Some(&(s, fg, fr)) = stats_map.get(&a.id) {
                api.statuses_count = s;
                api.following_count = fg;
                api.followers_count = fr;
            }
            api
        })
        .collect();
    super::formatting::link_profile_mentions(state, api_accounts.iter_mut()).await;
    api_accounts
}

/// Read a user's stored preferences from `users.settings` (a JSON object).
pub async fn user_settings_json(state: &AppState, account_id: i64) -> serde_json::Value {
    let raw = sqlx::query!(
        "SELECT settings FROM users WHERE account_id = $1",
        account_id
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .and_then(|r| r.settings);
    raw.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// A user's default posting preferences, read from `users.settings`.
pub struct UserDefaults {
    pub privacy: String,
    pub sensitive: bool,
    pub language: Option<String>,
    pub quote_policy: String,
}

pub async fn user_defaults(state: &AppState, account_id: i64) -> UserDefaults {
    let s = user_settings_json(state, account_id).await;
    let locked = sqlx::query_scalar::<_, bool>("SELECT locked FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .unwrap_or(false);
    UserDefaults {
        privacy: s
            .get("default_privacy")
            .or_else(|| s.get("privacy"))
            .and_then(|v| v.as_str())
            .unwrap_or(if locked { "private" } else { "public" })
            .to_string(),
        sensitive: s
            .get("default_sensitive")
            .or_else(|| s.get("web.default_sensitive"))
            .or_else(|| s.get("sensitive"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        language: s
            .get("default_language")
            .or_else(|| s.get("language"))
            .and_then(|v| v.as_str())
            .map(String::from),
        quote_policy: s
            .get("default_quote_policy")
            .or_else(|| s.get("quote_policy"))
            .and_then(|v| v.as_str())
            .unwrap_or("public")
            .to_string(),
    }
}

/// Populate an account entity's `statuses_count` / `following_count` /
/// `followers_count` from the `account_stats` table.
pub async fn apply_account_stats(
    state: &AppState,
    api: &mut super::types::Account,
    account_id: i64,
) {
    if let Ok(Some(st)) = sqlx::query!(
        "SELECT statuses_count, following_count, followers_count
         FROM account_stats WHERE account_id = $1",
        account_id,
    )
    .fetch_optional(&state.db)
    .await
    {
        api.statuses_count = st.statuses_count;
        api.following_count = st.following_count;
        api.followers_count = st.followers_count;
    }
    if let Some(&noindex) = batch_noindex(state, &[account_id]).await.get(&account_id) {
        api.noindex = Some(noindex);
    }
    super::formatting::link_profile_mentions(state, std::iter::once(api)).await;
}

// ── DELETE /api/v1/accounts ────────────────────────────────────────────────

/// Self-service account deletion, mirroring Mastodon's
/// `Settings::DeletesController#destroy`: pass the challenge, suspend the
/// account, then hand it to `DeleteAccountService` with the username reserved
/// and the user record (with the email and the rest of the PII) destroyed.
pub async fn delete_account(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    body: Option<Json<serde_json::Value>>,
) -> AppResult<axum::http::StatusCode> {
    auth.require_scope("write:accounts")?;
    let field = |name: &str| -> String {
        body.as_ref()
            .and_then(|b| b.get(name))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    let user = sqlx::query!(
        r#"SELECT u.encrypted_password, a.username, a.suspended_at, a.requested_deletion_at
           FROM users u JOIN accounts a ON a.id = u.account_id
           WHERE u.account_id = $1"#,
        auth.account_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::Unauthorized)?;

    // `require_not_suspended!`
    if user.suspended_at.is_some() || user.requested_deletion_at.is_some() {
        return Err(AppError::Forbidden);
    }

    // `challenge_passed?`: the password, or the username for accounts that have
    // none (OAuth-only sign-ins).
    if user.encrypted_password.is_empty() {
        if field("username") != user.username {
            return Err(AppError::Unauthorized);
        }
    } else {
        crate::crypto::verify_password(&field("password"), &user.encrypted_password).await?;
    }

    // `Account#mark_deleted!`.
    crate::delete_account::mark_deleted(&state, auth.account_id).await?;

    // Mastodon hands the purge to `AccountDeletionWorker`; eunha runs it on a
    // task for the same reason (an account can own a lot of content), except
    // under the tests' synchronous-fanout switch.
    let account_id = auth.account_id;
    let options = crate::delete_account::Options::self_service();
    if crate::feed::sync_fanout() {
        crate::delete_account::call(&state, account_id, options).await?;
    } else {
        let state = state.clone();
        crate::tenants::spawn(async move {
            if let Err(e) = crate::delete_account::call(&state, account_id, options).await {
                tracing::error!(account_id, error = %e, "account deletion failed");
            }
        });
    }

    Ok(axum::http::StatusCode::OK)
}

// ── GET /api/v1/donation_campaigns ───────────────────────────────────────

pub async fn list_donation_campaigns() -> Json<Vec<serde_json::Value>> {
    Json(vec![])
}

// ── GET /api/v1/accounts/:id/identity_proofs ─────────────────────────────

pub async fn get_account_identity_proofs(Path(_id): Path<i64>) -> Json<Vec<serde_json::Value>> {
    Json(vec![])
}
