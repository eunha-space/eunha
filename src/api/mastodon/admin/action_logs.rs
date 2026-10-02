//! `Admin::ActionLogsController` and `Admin::ActionLogFilter`: the audit log,
//! each entry worded as Mastodon's admin page words it.

use axum::{
    extract::{Extension, Path},
    http::{HeaderMap, Uri},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use super::super::extractors::{FlexId, Params};
use super::super::types::Account as ApiAccount;
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::role::flag,
    state::AppState,
};

/// `Admin::ActionLogFilter::ACTION_TYPE_MAP`: the filter's action types, each
/// the `(target_type, action)` it stands for.
pub const ACTION_TYPE_MAP: &[(&str, &str, &str)] = &[
    ("approve_appeal", "Appeal", "approve"),
    ("reject_appeal", "Appeal", "reject"),
    ("assigned_to_self_report", "Report", "assigned_to_self"),
    ("change_email_user", "User", "change_email"),
    ("change_role_user", "User", "change_role"),
    ("confirm_user", "User", "confirm"),
    ("approve_user", "User", "approve"),
    ("reject_user", "User", "reject"),
    ("create_account_warning", "AccountWarning", "create"),
    ("create_announcement", "Announcement", "create"),
    ("create_custom_emoji", "CustomEmoji", "create"),
    ("create_domain_allow", "DomainAllow", "create"),
    ("create_domain_block", "DomainBlock", "create"),
    ("create_email_domain_block", "EmailDomainBlock", "create"),
    ("create_ip_block", "IpBlock", "create"),
    ("create_relay", "Relay", "create"),
    ("create_unavailable_domain", "UnavailableDomain", "create"),
    ("create_user_role", "UserRole", "create"),
    (
        "create_canonical_email_block",
        "CanonicalEmailBlock",
        "create",
    ),
    ("demote_user", "User", "demote"),
    ("destroy_announcement", "Announcement", "destroy"),
    ("destroy_custom_emoji", "CustomEmoji", "destroy"),
    ("destroy_domain_allow", "DomainAllow", "destroy"),
    ("destroy_domain_block", "DomainBlock", "destroy"),
    ("destroy_ip_block", "IpBlock", "destroy"),
    ("destroy_relay", "Relay", "destroy"),
    ("destroy_email_domain_block", "EmailDomainBlock", "destroy"),
    ("destroy_instance", "Instance", "destroy"),
    ("destroy_unavailable_domain", "UnavailableDomain", "destroy"),
    ("destroy_status", "Status", "destroy"),
    ("destroy_user_role", "UserRole", "destroy"),
    (
        "destroy_canonical_email_block",
        "CanonicalEmailBlock",
        "destroy",
    ),
    ("disable_2fa_user", "User", "disable_2fa"),
    ("disable_custom_emoji", "CustomEmoji", "disable"),
    ("disable_user", "User", "disable"),
    ("disable_relay", "Relay", "disable"),
    ("enable_custom_emoji", "CustomEmoji", "enable"),
    ("enable_user", "User", "enable"),
    ("enable_relay", "Relay", "enable"),
    ("memorialize_account", "Account", "memorialize"),
    ("promote_user", "User", "promote"),
    ("publish_terms_of_service", "TermsOfService", "publish"),
    ("remove_avatar_user", "User", "remove_avatar"),
    ("reopen_report", "Report", "reopen"),
    ("resend_user", "User", "resend"),
    ("reset_password_user", "User", "reset_password"),
    ("resolve_report", "Report", "resolve"),
    ("sensitive_account", "Account", "sensitive"),
    ("silence_account", "Account", "silence"),
    ("suspend_account", "Account", "suspend"),
    ("unassigned_report", "Report", "unassigned"),
    ("unsensitive_account", "Account", "unsensitive"),
    ("unsilence_account", "Account", "unsilence"),
    ("unsuspend_account", "Account", "unsuspend"),
    ("update_announcement", "Announcement", "update"),
    ("update_custom_emoji", "CustomEmoji", "update"),
    ("update_report", "Report", "update"),
    ("update_status", "Status", "update"),
    ("update_user_role", "UserRole", "update"),
    ("update_ip_block", "IpBlock", "update"),
    ("unblock_email_account", "Account", "unblock_email"),
    ("update_tag", "Tag", "update"),
    ("create_username_block", "UsernameBlock", "create"),
    ("update_username_block", "UsernameBlock", "update"),
    ("destroy_username_block", "UsernameBlock", "destroy"),
];

/// `admin.action_logs.action_types`, the filter's labels.
const ACTION_TYPE_LABELS: &[(&str, &str)] = &[
    ("approve_appeal", "Approve Appeal"),
    ("approve_user", "Approve User"),
    ("assigned_to_self_report", "Assign Report"),
    ("change_email_user", "Change Email for User"),
    ("change_role_user", "Change Role of User"),
    ("confirm_user", "Confirm User"),
    ("create_account_warning", "Create Warning"),
    ("create_announcement", "Create Announcement"),
    ("create_canonical_email_block", "Create Email Block"),
    ("create_custom_emoji", "Create Custom Emoji"),
    ("create_domain_allow", "Create Domain Allow"),
    ("create_domain_block", "Create Domain Block"),
    ("create_email_domain_block", "Create Email Domain Block"),
    ("create_ip_block", "Create IP rule"),
    ("create_relay", "Create Relay"),
    ("create_unavailable_domain", "Create Unavailable Domain"),
    ("create_user_role", "Create Role"),
    ("create_username_block", "Create Username Rule"),
    ("demote_user", "Demote User"),
    ("destroy_announcement", "Delete Announcement"),
    ("destroy_canonical_email_block", "Delete Email Block"),
    ("destroy_custom_emoji", "Delete Custom Emoji"),
    ("destroy_domain_allow", "Delete Domain Allow"),
    ("destroy_domain_block", "Delete Domain Block"),
    ("destroy_email_domain_block", "Delete Email Domain Block"),
    ("destroy_instance", "Purge Domain"),
    ("destroy_ip_block", "Delete IP rule"),
    ("destroy_relay", "Delete Relay"),
    ("destroy_status", "Delete Post"),
    ("destroy_unavailable_domain", "Delete Unavailable Domain"),
    ("destroy_user_role", "Destroy Role"),
    ("destroy_username_block", "Delete Username Rule"),
    ("disable_2fa_user", "Disable 2FA"),
    ("disable_custom_emoji", "Disable Custom Emoji"),
    ("disable_relay", "Disable Relay"),
    (
        "disable_sign_in_token_auth_user",
        "Disable Email Token Authentication for User",
    ),
    ("disable_user", "Disable User"),
    ("enable_custom_emoji", "Enable Custom Emoji"),
    ("enable_relay", "Enable Relay"),
    (
        "enable_sign_in_token_auth_user",
        "Enable Email Token Authentication for User",
    ),
    ("enable_user", "Enable User"),
    ("memorialize_account", "Memorialize Account"),
    ("promote_user", "Promote User"),
    ("publish_terms_of_service", "Publish Terms of Service"),
    ("reject_appeal", "Reject Appeal"),
    ("reject_user", "Reject User"),
    ("remove_avatar_user", "Remove Avatar"),
    ("reopen_report", "Reopen Report"),
    ("resend_user", "Resend Confirmation Mail"),
    ("reset_password_user", "Reset Password"),
    ("resolve_report", "Resolve Report"),
    ("sensitive_account", "Force-Sensitive Account"),
    ("silence_account", "Limit Account"),
    ("suspend_account", "Suspend Account"),
    ("unassigned_report", "Unassign Report"),
    ("unblock_email_account", "Unblock email address"),
    ("unsensitive_account", "Undo Force-Sensitive Account"),
    ("unsilence_account", "Undo Limit Account"),
    ("unsuspend_account", "Unsuspend Account"),
    ("update_announcement", "Update Announcement"),
    ("update_custom_emoji", "Update Custom Emoji"),
    ("update_domain_block", "Update Domain Block"),
    ("update_ip_block", "Update IP rule"),
    ("update_report", "Update Report"),
    ("update_status", "Update Post"),
    ("update_user_role", "Update Role"),
    ("update_username_block", "Update Username Rule"),
];

/// `admin.action_logs.actions`, keyed by `"#{action}_#{target_type.underscore}"`.
const ACTIONS: &[(&str, &str)] = &[
    (
        "approve_appeal",
        "%{name} approved moderation decision appeal from %{target}",
    ),
    ("approve_user", "%{name} approved sign-up from %{target}"),
    (
        "assigned_to_self_report",
        "%{name} assigned report %{target} to themselves",
    ),
    (
        "change_email_user",
        "%{name} changed the email address of user %{target}",
    ),
    ("change_role_user", "%{name} changed role of %{target}"),
    (
        "confirm_user",
        "%{name} confirmed email address of user %{target}",
    ),
    (
        "create_account_warning",
        "%{name} sent a warning to %{target}",
    ),
    (
        "create_announcement",
        "%{name} created new announcement %{target}",
    ),
    (
        "create_canonical_email_block",
        "%{name} blocked email with the hash %{target}",
    ),
    (
        "create_custom_emoji",
        "%{name} uploaded new emoji %{target}",
    ),
    (
        "create_domain_allow",
        "%{name} allowed federation with domain %{target}",
    ),
    ("create_domain_block", "%{name} blocked domain %{target}"),
    (
        "create_email_domain_block",
        "%{name} blocked email domain %{target}",
    ),
    ("create_ip_block", "%{name} created rule for IP %{target}"),
    ("create_relay", "%{name} created a relay %{target}"),
    (
        "create_unavailable_domain",
        "%{name} stopped delivery to domain %{target}",
    ),
    ("create_user_role", "%{name} created %{target} role"),
    (
        "create_username_block",
        "%{name} added rule for usernames containing %{target}",
    ),
    ("demote_user", "%{name} demoted user %{target}"),
    (
        "destroy_announcement",
        "%{name} deleted announcement %{target}",
    ),
    (
        "destroy_canonical_email_block",
        "%{name} unblocked email with the hash %{target}",
    ),
    (
        "destroy_collection",
        "%{name} removed collection by %{target}",
    ),
    ("destroy_custom_emoji", "%{name} deleted emoji %{target}"),
    (
        "destroy_domain_allow",
        "%{name} disallowed federation with domain %{target}",
    ),
    ("destroy_domain_block", "%{name} unblocked domain %{target}"),
    (
        "destroy_email_domain_block",
        "%{name} unblocked email domain %{target}",
    ),
    ("destroy_instance", "%{name} purged domain %{target}"),
    ("destroy_ip_block", "%{name} deleted rule for IP %{target}"),
    ("destroy_relay", "%{name} deleted the relay %{target}"),
    ("destroy_status", "%{name} removed post by %{target}"),
    (
        "destroy_unavailable_domain",
        "%{name} resumed delivery to domain %{target}",
    ),
    ("destroy_user_role", "%{name} deleted %{target} role"),
    (
        "destroy_username_block",
        "%{name} removed rule for usernames containing %{target}",
    ),
    (
        "disable_2fa_user",
        "%{name} disabled two factor requirement for user %{target}",
    ),
    ("disable_custom_emoji", "%{name} disabled emoji %{target}"),
    ("disable_relay", "%{name} disabled the relay %{target}"),
    (
        "disable_sign_in_token_auth_user",
        "%{name} disabled email token authentication for %{target}",
    ),
    ("disable_user", "%{name} disabled login for user %{target}"),
    ("enable_custom_emoji", "%{name} enabled emoji %{target}"),
    ("enable_relay", "%{name} enabled the relay %{target}"),
    (
        "enable_sign_in_token_auth_user",
        "%{name} enabled email token authentication for %{target}",
    ),
    ("enable_user", "%{name} enabled login for user %{target}"),
    (
        "memorialize_account",
        "%{name} turned %{target}'s account into a memoriam page",
    ),
    ("promote_user", "%{name} promoted user %{target}"),
    (
        "publish_terms_of_service",
        "%{name} published updates to the terms of service",
    ),
    (
        "reject_appeal",
        "%{name} rejected moderation decision appeal from %{target}",
    ),
    ("reject_user", "%{name} rejected sign-up from %{target}"),
    ("remove_avatar_user", "%{name} removed %{target}'s avatar"),
    ("reopen_report", "%{name} reopened report %{target}"),
    (
        "resend_user",
        "%{name} resent confirmation email for %{target}",
    ),
    (
        "reset_password_user",
        "%{name} reset password of user %{target}",
    ),
    ("resolve_report", "%{name} resolved report %{target}"),
    (
        "sensitive_account",
        "%{name} marked %{target}'s media as sensitive",
    ),
    ("silence_account", "%{name} limited %{target}'s account"),
    ("suspend_account", "%{name} suspended %{target}'s account"),
    ("unassigned_report", "%{name} unassigned report %{target}"),
    (
        "unblock_email_account",
        "%{name} unblocked %{target}'s email address",
    ),
    (
        "unsensitive_account",
        "%{name} unmarked %{target}'s media as sensitive",
    ),
    (
        "unsilence_account",
        "%{name} undid limit of %{target}'s account",
    ),
    (
        "unsuspend_account",
        "%{name} unsuspended %{target}'s account",
    ),
    (
        "update_announcement",
        "%{name} updated announcement %{target}",
    ),
    (
        "update_collection",
        "%{name} updated collection by %{target}",
    ),
    ("update_custom_emoji", "%{name} updated emoji %{target}"),
    (
        "update_domain_block",
        "%{name} updated domain block for %{target}",
    ),
    ("update_ip_block", "%{name} changed rule for IP %{target}"),
    ("update_report", "%{name} updated report %{target}"),
    ("update_status", "%{name} updated post by %{target}"),
    ("update_tag", "%{name} changed %{target} settings to: "),
    ("update_user_role", "%{name} changed %{target} role"),
    (
        "update_username_block",
        "%{name} updated rule for usernames containing %{target}",
    ),
];

/// `Admin::ActionLogFilter::INSTANCE_TARGET_TYPES`.
const INSTANCE_TARGET_TYPES: &[&str] = &[
    "DomainBlock",
    "DomainAllow",
    "Instance",
    "UnavailableDomain",
];

/// `admin.action_logs.deleted_account`.
const DELETED_ACCOUNT: &str = "deleted account";
/// `admin.action_logs.unavailable_instance`.
const UNAVAILABLE_INSTANCE: &str = "(domain name unavailable)";

/// `String#underscore` of a Rails class name: `AccountWarning` is
/// `account_warning`.
fn underscore(class_name: &str) -> String {
    let mut out = String::with_capacity(class_name.len() + 4);
    for (i, c) in class_name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `truncate` with its defaults: thirty characters at most, the last three
/// of them `...` when it had to cut.
fn truncate(text: &str) -> String {
    if text.chars().count() <= 30 {
        text.to_owned()
    } else {
        format!("{}...", text.chars().take(27).collect::<String>())
    }
}

/// What `log_target` shows for an entry: its words and where they link. A
/// link is a path in the web client, or for a post its own address.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LogTarget {
    pub text: String,
    pub href: Option<String>,
}

/// One `admin_action_logs` row, as the log renders it.
pub struct LogRow {
    pub id: i64,
    pub account_id: i64,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<i64>,
    pub human_identifier: Option<String>,
    pub route_param: Option<String>,
    pub permalink: Option<String>,
    pub recorded_changes: Option<serde_json::Value>,
    pub created_at: chrono::NaiveDateTime,
}

fn present(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|v| !v.trim().is_empty())
}

/// `Admin::ActionLogsHelper#log_target`, with the links pointing where the
/// web client has the page Mastodon links to, and nowhere when it has none.
pub fn log_target(row: &LogRow) -> Option<LogTarget> {
    let target_id = row.target_id.map(|id| id.to_string()).unwrap_or_default();
    let human = row.human_identifier.clone().unwrap_or_default();
    let link = |text: String, href: String| {
        Some(LogTarget {
            text,
            href: Some(href),
        })
    };
    let plain = |text: String| Some(LogTarget { text, href: None });
    match row.target_type.as_deref()? {
        "Account" => link(
            present(&row.human_identifier)
                .unwrap_or(DELETED_ACCOUNT)
                .to_owned(),
            format!("/admin/accounts/{target_id}"),
        ),
        "User" => match present(&row.route_param) {
            Some(account_id) => link(human, format!("/admin/accounts/{account_id}")),
            None => plain(DELETED_ACCOUNT.into()),
        },
        "UsernameBlock" => link(human, "/admin/username_blocks".into()),
        "Report" => link(
            format!("#{}", present(&row.human_identifier).unwrap_or(&target_id)),
            format!("/admin/reports/{target_id}"),
        ),
        "Instance" | "DomainBlock" | "DomainAllow" | "UnavailableDomain" => {
            match present(&row.human_identifier) {
                Some(domain) => plain(domain.to_owned()),
                None => plain(UNAVAILABLE_INSTANCE.into()),
            }
        }
        "Status" | "Collection" => Some(LogTarget {
            text: human,
            href: row.permalink.clone(),
        }),
        "AccountWarning" => link(human, format!("/disputes/strikes/{target_id}")),
        "Announcement" => plain(truncate(&human)),
        "UserRole" | "IpBlock" | "EmailDomainBlock" | "CustomEmoji" | "Relay" => plain(human),
        "CanonicalEmailBlock" => plain(human.chars().take(7).collect()),
        "Appeal" => match present(&row.route_param) {
            Some(strike_id) => link(human, format!("/disputes/strikes/{strike_id}")),
            None => plain(DELETED_ACCOUNT.into()),
        },
        "Tag" => link(human, "/admin/tags".into()),
        _ => None,
    }
}

/// `chain_multiple_translations`: what a hashtag's `usable`, `trendable` and
/// `listable` were set to, from `recorded_changes`.
fn recorded_changes_text(row: &LogRow) -> Option<String> {
    if row.target_type.as_deref() != Some("Tag") {
        return None;
    }
    let changes = row.recorded_changes.as_ref()?.as_object()?;
    // `admin.trends.tags.*`.
    let words = [
        ("usable", "Can be used", "Cannot be used"),
        (
            "trendable",
            "Can appear under trends",
            "Won't appear under trends",
        ),
        ("listable", "Can be suggested", "Won't be suggested"),
    ];
    let parts: Vec<&str> = words
        .iter()
        .filter_map(|(key, yes, no)| match changes.get(*key)? {
            serde_json::Value::Null => None,
            serde_json::Value::Bool(true) => Some(*yes),
            _ => Some(*no),
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// The `admin.action_logs.actions.*_html` template for an entry, with
/// `%{name}` and `%{target}` still in it.
pub fn template(action: &str, target_type: Option<&str>) -> String {
    let key = format!("{action}_{}", underscore(target_type.unwrap_or_default()));
    ACTIONS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, t)| (*t).to_owned())
        .unwrap_or_else(|| format!("%{{name}} {action} %{{target}}"))
}

/// An audit log entry as eunha serves it.
#[derive(Debug, Serialize)]
pub struct ActionLog {
    pub id: String,
    pub action: String,
    /// The `Admin::ActionLogFilter` action type this entry falls under.
    pub action_type: Option<&'static str>,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub human_identifier: Option<String>,
    pub route_param: Option<String>,
    pub permalink: Option<String>,
    pub recorded_changes: Option<serde_json::Value>,
    pub created_at: String,
    pub account: Option<ApiAccount>,
    /// The sentence Mastodon's log shows, with `%{name}` and `%{target}` in
    /// it for the client to fill.
    pub template: String,
    pub target: Option<LogTarget>,
    /// What a hashtag was set to, for an `update` of one.
    pub changes: Option<String>,
    /// The whole sentence as plain text.
    pub text: String,
}

/// One entry, rendered for `account`, the one who acted.
pub fn render(row: LogRow, account: Option<ApiAccount>) -> ActionLog {
    let template = template(&row.action, row.target_type.as_deref());
    let target = log_target(&row);
    let changes = recorded_changes_text(&row);
    let name = account
        .as_ref()
        .map(|a| a.username.clone())
        .unwrap_or_default();
    let mut text = template
        .replace("%{name}", &name)
        .replace("%{target}", target.as_ref().map_or("", |t| t.text.as_str()));
    if let Some(changes) = &changes {
        if !text.ends_with(' ') {
            text.push(' ');
        }
        text.push_str(changes);
    }
    let action_type = ACTION_TYPE_MAP
        .iter()
        .find(|(_, kind, action)| {
            Some(*kind) == row.target_type.as_deref() && *action == row.action
        })
        .map(|(key, _, _)| *key);
    ActionLog {
        id: row.id.to_string(),
        action: row.action,
        action_type,
        target_type: row.target_type,
        target_id: row.target_id.map(|id| id.to_string()),
        human_identifier: row.human_identifier,
        route_param: row.route_param,
        permalink: row.permalink,
        recorded_changes: row.recorded_changes,
        created_at: super::super::convert::mastodon_date(row.created_at),
        account,
        template,
        target,
        changes,
        text: text.trim().to_owned(),
    }
}

async fn render_all(state: &AppState, rows: Vec<LogRow>) -> AppResult<Vec<ActionLog>> {
    let mut accounts: std::collections::HashMap<i64, Option<ApiAccount>> = Default::default();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let account = match accounts.get(&row.account_id) {
            Some(account) => account.clone(),
            None => {
                let account = super::api_account(state, row.account_id).await?;
                accounts.insert(row.account_id, account.clone());
                account
            }
        };
        out.push(render(row, account));
    }
    Ok(out)
}

// ── GET /api/v1/admin/action_logs ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ActionLogParams {
    pub account_id: Option<String>,
    pub action_type: Option<String>,
    pub target_account_id: Option<String>,
    pub target_domain: Option<String>,
    pub target_tag: Option<String>,
    pub limit: Option<FlexId>,
    pub max_id: Option<FlexId>,
    pub since_id: Option<FlexId>,
    pub min_id: Option<FlexId>,
}

fn given(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

/// `Admin::ActionLogFilter#results`, newest first, paginated by id.
pub async fn list_action_logs(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    uri: Uri,
    req_headers: HeaderMap,
    Params(params): Params<ActionLogParams>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("admin:read")?;
    // `authorize :audit_log, :index?`
    super::require_permission(&state, auth.account_id, flag::VIEW_AUDIT_LOG).await?;

    let mut q = sqlx::QueryBuilder::<sqlx::Postgres>::new(
        r#"SELECT id, account_id, action, target_type, target_id, human_identifier,
                  route_param, permalink, recorded_changes, created_at
           FROM admin_action_logs WHERE true"#,
    );
    if let Some(account_id) = given(&params.account_id) {
        q.push(" AND account_id = ")
            .push_bind(account_id.parse::<i64>().unwrap_or(0));
    }
    // An action type the map does not know narrows nothing, as `where(nil)`.
    if let Some(action_type) = given(&params.action_type) {
        if let Some((_, kind, action)) = ACTION_TYPE_MAP.iter().find(|(k, _, _)| *k == action_type)
        {
            q.push(" AND target_type = ")
                .push_bind(*kind)
                .push(" AND action = ")
                .push_bind(*action);
        }
    }
    if let Some(target) = given(&params.target_account_id) {
        // `where(target: [account, account.user].compact)`.
        let target = target.parse::<i64>().unwrap_or(0);
        q.push(" AND ((target_type = 'Account' AND target_id = ")
            .push_bind(target)
            .push(
                ") OR (target_type = 'User' AND target_id IN (SELECT id FROM users WHERE account_id = ",
            )
            .push_bind(target)
            .push(")))");
    }
    if let Some(domain) = given(&params.target_domain) {
        let domain = super::federation::normalize_domain(&domain).unwrap_or(domain);
        q.push(" AND human_identifier = ")
            .push_bind(domain)
            .push(" AND target_type = ANY(")
            .push_bind(INSTANCE_TARGET_TYPES)
            .push(")");
    }
    if let Some(tag) = given(&params.target_tag) {
        q.push(" AND human_identifier = ").push_bind(tag);
    }
    let page = super::PageParams {
        limit: params.limit,
        max_id: params.max_id,
        since_id: params.since_id,
        min_id: params.min_id,
    };
    if let Some(FlexId(max_id)) = page.max_id {
        q.push(" AND id < ").push_bind(max_id);
    }
    if let Some(FlexId(since_id)) = page.since_id {
        q.push(" AND id > ").push_bind(since_id);
    }
    let min_id = page.min_id.map(|i| i.0);
    if let Some(min_id) = min_id {
        q.push(" AND id > ")
            .push_bind(min_id)
            .push(" ORDER BY id ASC");
    } else {
        q.push(" ORDER BY id DESC");
    }
    // Kaminari's `default_per_page`.
    q.push(" LIMIT ").push_bind(page.limit(40, 80));

    let mut rows: Vec<LogRow> = q
        .build()
        .fetch_all(&state.db)
        .await?
        .iter()
        .map(from_pg_row)
        .collect::<Result<_, sqlx::Error>>()?;
    if min_id.is_some() {
        rows.reverse();
    }
    let result = render_all(&state, rows).await?;
    let bounds = result
        .first()
        .zip(result.last())
        .map(|(n, o)| (n.id.as_str(), o.id.as_str()));
    let headers = super::super::link_headers(&req_headers, &uri, bounds);
    Ok((headers, Json(result)))
}

fn from_pg_row(r: &sqlx::postgres::PgRow) -> Result<LogRow, sqlx::Error> {
    use sqlx::Row;
    Ok(LogRow {
        id: r.try_get("id")?,
        account_id: r.try_get("account_id")?,
        action: r.try_get("action")?,
        target_type: r.try_get("target_type")?,
        target_id: r.try_get("target_id")?,
        human_identifier: r.try_get("human_identifier")?,
        route_param: r.try_get("route_param")?,
        permalink: r.try_get("permalink")?,
        recorded_changes: r.try_get("recorded_changes")?,
        created_at: r.try_get("created_at")?,
    })
}

// ── GET /api/v1/admin/action_logs/filters ─────────────────────────────────

#[derive(Debug, Serialize)]
pub struct FilterChoice {
    pub key: String,
    pub label: String,
}

#[derive(Debug, Serialize)]
pub struct ActionLogFilters {
    /// `Account.auditable`, by username.
    pub accounts: Vec<FilterChoice>,
    /// `sorted_action_log_types`.
    pub action_types: Vec<FilterChoice>,
}

/// What the log's two selects offer: the accounts that have acted, and the
/// action types by label.
pub async fn action_log_filters(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<ActionLogFilters>> {
    auth.require_scope("admin:read")?;
    super::require_permission(&state, auth.account_id, flag::VIEW_AUDIT_LOG).await?;
    let accounts = sqlx::query!(
        r#"SELECT id, username FROM accounts
           WHERE id IN (SELECT DISTINCT account_id FROM admin_action_logs)
           ORDER BY username ASC"#,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| FilterChoice {
        key: r.id.to_string(),
        label: r.username,
    })
    .collect();
    let mut action_types: Vec<FilterChoice> = ACTION_TYPE_MAP
        .iter()
        .map(|(key, _, _)| FilterChoice {
            key: (*key).to_owned(),
            label: ACTION_TYPE_LABELS
                .iter()
                .find(|(k, _)| k == key)
                .map_or(*key, |(_, l)| *l)
                .to_owned(),
        })
        .collect();
    action_types.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(Json(ActionLogFilters {
        accounts,
        action_types,
    }))
}

// ── GET /api/v1/admin/reports/:id/history ─────────────────────────────────

/// `Report#history`: what was logged about the report, its account, its
/// posts, and the strikes it led to, newest first.
pub async fn report_history(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Vec<ActionLog>>> {
    auth.require_scope("admin:read:reports")?;
    let report = sqlx::query!(
        "SELECT target_account_id, status_ids FROM reports WHERE id = $1",
        id
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    // `authorize @report, :show?`
    super::require_permission(&state, auth.account_id, flag::MANAGE_REPORTS).await?;
    let rows = sqlx::query_as!(
        LogRow,
        r#"SELECT id, account_id, action, target_type, target_id, human_identifier,
                  route_param, permalink, recorded_changes, created_at
           FROM admin_action_logs
           WHERE (target_type = 'Report' AND target_id = $1)
              OR (target_type = 'Account' AND target_id = $2)
              OR (target_type = 'Status' AND target_id = ANY($3))
              OR (target_type = 'AccountWarning'
                  AND target_id IN (SELECT id FROM account_warnings WHERE report_id = $1))
           ORDER BY id DESC"#,
        id,
        report.target_account_id,
        &report.status_ids,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(render_all(&state, rows).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(action: &str, target_type: &str, human: Option<&str>, route: Option<&str>) -> LogRow {
        LogRow {
            id: 1,
            account_id: 1,
            action: action.into(),
            target_type: Some(target_type.into()),
            target_id: Some(7),
            human_identifier: human.map(Into::into),
            route_param: route.map(Into::into),
            permalink: None,
            recorded_changes: None,
            created_at: chrono::NaiveDateTime::default(),
        }
    }

    #[test]
    fn underscores_class_names() {
        assert_eq!(underscore("AccountWarning"), "account_warning");
        assert_eq!(underscore("IpBlock"), "ip_block");
        assert_eq!(underscore("User"), "user");
    }

    #[test]
    fn words_entries_as_mastodon_does() {
        let r = render(row("suspend", "Account", Some("bob"), None), None);
        assert_eq!(r.template, "%{name} suspended %{target}'s account");
        assert_eq!(r.text, "suspended bob's account");
        assert_eq!(r.target.unwrap().href.as_deref(), Some("/admin/accounts/7"));

        let r = render(row("resolve", "Report", Some("7"), None), None);
        assert_eq!(r.text, "resolved report #7");
        assert_eq!(r.action_type, Some("resolve_report"));

        let r = render(row("disable", "User", Some("bob"), None), None);
        assert_eq!(r.target.unwrap().text, "deleted account");

        let r = render(row("approve", "Appeal", Some("bob"), Some("12")), None);
        assert_eq!(
            r.target.unwrap().href.as_deref(),
            Some("/disputes/strikes/12")
        );

        let r = render(
            row(
                "create",
                "CanonicalEmailBlock",
                Some("0123456789abcdef"),
                None,
            ),
            None,
        );
        assert_eq!(r.target.unwrap().text, "0123456");
    }

    #[test]
    fn every_action_type_has_a_label_and_a_sentence() {
        for (key, kind, action) in ACTION_TYPE_MAP {
            // Upstream's locale has no `action_types.update_tag`; its filter
            // shows the missing translation, and eunha the key.
            if *key != "update_tag" {
                assert!(ACTION_TYPE_LABELS.iter().any(|(k, _)| k == key), "{key}");
            }
            assert!(
                ACTIONS
                    .iter()
                    .any(|(k, _)| *k == format!("{action}_{}", underscore(kind))),
                "{key}"
            );
        }
    }
}
