//! The invite tree: who invited whom among an instance's local members.
//!
//! Mastodon: none

use axum::{
    extract::Query,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Extension,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};

use crate::{
    api::mastodon::{admin, convert},
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

/// One place in the tree. A member is named; a placeholder stands where a
/// member is not shown, so the people below it keep their branch without
/// anyone learning who it was.
#[derive(Debug, Serialize)]
pub struct TreeMember {
    /// The account id for a member; an id local to this response for a
    /// placeholder, which says nothing about the account behind it.
    pub id: String,
    /// `member`, `former_member` (an inviter who is no longer a member:
    /// deleted, suspended or otherwise gone) or `hidden_member` (a member who
    /// hides their network, `accounts.hide_collections`).
    pub kind: &'static str,
    /// Who invited them, as an `id` in this response; none for a root.
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acct: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// The still avatar, which the web client shows unless GIFs auto-play.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar_static: Option<String>,
    /// When the member joined (their user row's `created_at`), ISO 8601.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub joined_at: Option<String>,
    /// Why a root has no parent; never names a hidden inviter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_reason: Option<&'static str>,
}

#[derive(Debug, Serialize)]
pub struct InviteTreeResponse {
    /// Every place in the requested view, parents before their children and
    /// otherwise in joining order. A flat list, so neither end recurses to
    /// read a long chain.
    pub members: Vec<TreeMember>,
    /// Local members on the instance, whichever view was asked for.
    pub total: usize,
}

#[derive(Debug, Deserialize)]
pub struct TreeQuery {
    /// `branch` for the viewer's ancestors and everyone below them; anything
    /// else for the whole instance.
    #[serde(default)]
    scope: Option<String>,
}

struct Row {
    id: i64,
    username: String,
    display_name: String,
    avatar_file_name: Option<String>,
    avatar_remote_url: Option<String>,
    avatar_content_type: Option<String>,
    created_at: chrono::NaiveDateTime,
    hidden: bool,
    invited_by: Option<i64>,
}

/// GET /api/eunha/v1/invite_tree
///
/// eunha-specific endpoint (Mastodon records `users.invite_id` but has no
/// invite-tree API). Any signed-in local member may view it. Answers
/// `If-None-Match` with 304, since the tree changes far less often than it is
/// opened.
pub async fn invite_tree(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    Query(query): Query<TreeQuery>,
    headers: HeaderMap,
) -> AppResult<Response> {
    // Require an authenticated local user (app-only tokens have no user_id).
    let viewer = match auth {
        Some(Extension(auth)) if auth.user_id.is_some() => auth.account_id,
        _ => return Err(AppError::Unauthorized),
    };
    // Staff who hand out invites see the tree whole; they can see every
    // account in moderation anyway.
    let sees_hidden = admin::require_permission(&state, viewer, admin::perm::MANAGE_INVITES)
        .await
        .is_ok();

    // The inviter is reached via users.invite_id -> invites.user_id -> users ->
    // accounts (Mastodon's `invite.user.account` path), falling back to
    // `eunha.invite_lineage` for accounts whose inviter deleted its user record
    // (which takes its `invites` — and so that link — with it). Only genuine
    // members: confirmed, admin-approved, and neither suspended nor deleted.
    let rows: Vec<Row> = sqlx::query_as::<
        _,
        (
            i64,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            chrono::NaiveDateTime,
            bool,
            Option<i64>,
        ),
    >(
        r#"SELECT a.id, a.username, a.display_name,
                  a.avatar_file_name, a.avatar_remote_url, a.avatar_content_type,
                  u.created_at, COALESCE(a.hide_collections, false),
                  COALESCE(inv_a.id, il.inviter_account_id)
           FROM users u
           JOIN accounts a ON a.id = u.account_id
           LEFT JOIN invites i ON i.id = u.invite_id
           LEFT JOIN users inv_u ON inv_u.id = i.user_id
           LEFT JOIN accounts inv_a ON inv_a.id = inv_u.account_id
           LEFT JOIN eunha.invite_lineage il ON il.account_id = a.id
           WHERE a.domain IS NULL
             AND u.approved
             AND u.confirmed_at IS NOT NULL
             AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
           ORDER BY u.created_at ASC, a.id ASC"#,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| Row {
        id: r.0,
        username: r.1,
        display_name: r.2,
        avatar_file_name: r.3,
        avatar_remote_url: r.4,
        avatar_content_type: r.5,
        created_at: r.6,
        hidden: r.7,
        invited_by: r.8,
    })
    .collect();
    let total = rows.len();

    let mut members = build(&state, rows, viewer, sees_hidden);
    if query.scope.as_deref() == Some("branch") {
        members = branch(members, &viewer.to_string());
    }
    let body = serde_json::to_vec(&InviteTreeResponse { members, total })
        .map_err(|e| AppError::Internal(e.into()))?;

    let etag = format!("W/\"{}\"", hex::encode(&Sha256::digest(&body)[..16]));
    let mut response = if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|tags| tags.split(',').any(|t| t.trim() == etag))
    {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, "application/json")], body).into_response()
    };
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, value);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    Ok(response)
}

/// Lay the rows out as a forest, parents first, without recursion.
fn build(state: &AppState, rows: Vec<Row>, viewer: i64, sees_hidden: bool) -> Vec<TreeMember> {
    let present: HashSet<i64> = rows.iter().map(|r| r.id).collect();
    let mut by_id: HashMap<i64, Row> = HashMap::with_capacity(rows.len());
    let order: Vec<i64> = rows.iter().map(|r| r.id).collect();

    // An inviter who is not a member any more becomes one placeholder, so
    // their invitees stay together under it.
    let mut former: HashMap<i64, String> = HashMap::new();
    let mut children: HashMap<i64, Vec<i64>> = HashMap::new();
    let mut former_children: Vec<(String, Vec<i64>)> = Vec::new();
    let mut roots: Vec<i64> = Vec::new();
    for row in &rows {
        match row.invited_by {
            // Inviting yourself is damaged lineage; the last pass places it.
            Some(pid) if pid == row.id => {}
            Some(pid) if present.contains(&pid) => {
                children.entry(pid).or_default().push(row.id);
            }
            Some(pid) => {
                let next = former.len() + 1;
                let key = former
                    .entry(pid)
                    .or_insert_with(|| format!("former-{next}"));
                match former_children.iter_mut().find(|(k, _)| k == key) {
                    Some((_, list)) => list.push(row.id),
                    None => former_children.push((key.clone(), vec![row.id])),
                }
            }
            None => roots.push(row.id),
        }
    }
    for row in rows {
        by_id.insert(row.id, row);
    }

    // Placeholder ids for hidden members, in joining order.
    let mut hidden_ids: HashMap<i64, String> = HashMap::new();
    for id in &order {
        let row = &by_id[id];
        if row.hidden && !sees_hidden && row.id != viewer {
            let n = hidden_ids.len() + 1;
            hidden_ids.insert(row.id, format!("hidden-{n}"));
        }
    }
    let public_id = |id: i64| {
        hidden_ids
            .get(&id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    };

    let mut out: Vec<TreeMember> = Vec::with_capacity(order.len() + former.len());
    let mut visited: HashSet<i64> = HashSet::with_capacity(order.len());
    let mut queue: VecDeque<i64> = VecDeque::new();
    let emit = |id: i64,
                parent_id: Option<String>,
                root_reason: Option<&'static str>,
                out: &mut Vec<TreeMember>| {
        let row = &by_id[&id];
        out.push(if hidden_ids.contains_key(&id) {
            TreeMember {
                id: public_id(id),
                kind: "hidden_member",
                parent_id,
                username: None,
                acct: None,
                display_name: None,
                avatar: None,
                avatar_static: None,
                joined_at: None,
                root_reason,
            }
        } else {
            TreeMember {
                id: id.to_string(),
                kind: "member",
                parent_id,
                username: Some(row.username.clone()),
                acct: Some(row.username.clone()),
                display_name: Some(row.display_name.clone()),
                avatar: Some(convert::account_avatar_url_parts(
                    &state.urls,
                    id,
                    row.avatar_file_name.as_deref(),
                    row.avatar_remote_url.as_deref(),
                )),
                avatar_static: Some(convert::account_avatar_static_url_parts(
                    &state.urls,
                    id,
                    row.avatar_file_name.as_deref(),
                    row.avatar_remote_url.as_deref(),
                    row.avatar_content_type.as_deref(),
                )),
                joined_at: Some(convert::mastodon_date(row.created_at)),
                root_reason,
            }
        });
    };
    // Breadth-first from each start, so every parent precedes its children.
    let mut walk = |start: i64,
                    start_parent: Option<String>,
                    reason: Option<&'static str>,
                    out: &mut Vec<TreeMember>,
                    visited: &mut HashSet<i64>| {
        if !visited.insert(start) {
            return;
        }
        emit(start, start_parent, reason, out);
        queue.push_back(start);
        while let Some(id) = queue.pop_front() {
            for &child in children.get(&id).map(Vec::as_slice).unwrap_or(&[]) {
                if visited.insert(child) {
                    emit(child, Some(public_id(id)), None, out);
                    queue.push_back(child);
                }
            }
        }
    };

    // Roots and former members' branches interleave in joining order.
    // (member, parent id, root reason, former member placed first)
    type Start = (i64, Option<String>, Option<&'static str>, Option<String>);
    let mut starts: Vec<Start> = Vec::new();
    for &id in &roots {
        starts.push((id, None, Some("no_recorded_inviter"), None));
    }
    for (key, kids) in &former_children {
        for &id in kids {
            starts.push((id, Some(key.clone()), None, Some(key.clone())));
        }
    }
    let position: HashMap<i64, usize> = order.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    starts.sort_by_key(|(id, ..)| position[id]);
    let mut placed_former: HashSet<String> = HashSet::new();
    for (id, start_parent, reason, former_key) in starts {
        if let Some(key) = former_key {
            if placed_former.insert(key.clone()) {
                out.push(TreeMember {
                    id: key,
                    kind: "former_member",
                    parent_id: None,
                    username: None,
                    acct: None,
                    display_name: None,
                    avatar: None,
                    avatar_static: None,
                    joined_at: None,
                    root_reason: None,
                });
            }
        }
        walk(id, start_parent, reason, &mut out, &mut visited);
    }
    // Imported or damaged lineage may contain a cycle without a root. Break
    // it where it starts in joining order, so no member silently disappears.
    for &id in &order {
        walk(
            id,
            None,
            Some("lineage_unavailable"),
            &mut out,
            &mut visited,
        );
    }
    out
}

/// The viewer's ancestors and everyone below the viewer, from a full list.
fn branch(members: Vec<TreeMember>, viewer: &str) -> Vec<TreeMember> {
    let index: HashMap<&str, usize> = members
        .iter()
        .enumerate()
        .map(|(i, m)| (m.id.as_str(), i))
        .collect();
    let Some(&me) = index.get(viewer) else {
        return Vec::new();
    };
    let mut keep: HashSet<usize> = HashSet::new();
    // Up: the viewer's line back to a root.
    let mut at = Some(me);
    while let Some(i) = at {
        if !keep.insert(i) {
            break;
        }
        at = members[i]
            .parent_id
            .as_deref()
            .and_then(|p| index.get(p).copied());
    }
    // Down: parents precede children, so one pass from the viewer finds them.
    let mut below: HashSet<&str> = HashSet::from([viewer]);
    for (i, m) in members.iter().enumerate().skip(me + 1) {
        if m.parent_id.as_deref().is_some_and(|p| below.contains(p)) {
            below.insert(m.id.as_str());
            keep.insert(i);
        }
    }
    let keep: HashSet<usize> = keep;
    members
        .into_iter()
        .enumerate()
        .filter_map(|(i, m)| keep.contains(&i).then_some(m))
        .collect()
}
