//! `Admin::RelationshipsController` and `RelationshipFilter`: whom an account
//! follows, who follows it, and whom it invited, as a moderator sees them.

use axum::{
    extract::{Extension, Path},
    http::{HeaderMap, HeaderValue, Uri},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use super::super::extractors::{FlexId, Params};
use crate::{
    db::models,
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::role::flag,
    state::AppState,
};

/// `Admin::RelationshipsController::PER_PAGE`.
const PER_PAGE: i64 = 40;

#[derive(Debug, Deserialize)]
pub struct RelationshipParams {
    pub relationship: Option<String>,
    pub status: Option<String>,
    pub by_domain: Option<String>,
    pub activity: Option<String>,
    pub order: Option<String>,
    pub location: Option<String>,
    pub page: Option<FlexId>,
}

fn given(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// `GET /api/v1/admin/accounts/:account_id/relationships`: `RelationshipFilter`
/// over the account, `following` and `recent` unless said otherwise, forty to
/// a page as Mastodon pages it.
pub async fn list_admin_relationships(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(account_id): Path<i64>,
    uri: Uri,
    Params(params): Params<RelationshipParams>,
) -> AppResult<impl IntoResponse> {
    auth.require_scope("admin:read:accounts")?;
    sqlx::query_scalar!("SELECT id FROM accounts WHERE id = $1", account_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    // `authorize @account, :show?`
    super::require_permission(&state, auth.account_id, flag::MANAGE_USERS).await?;

    let relationship = given(&params.relationship).unwrap_or("following");
    let order = given(&params.order).unwrap_or("recent");
    let mut q = sqlx::QueryBuilder::<sqlx::Postgres>::new("SELECT a.* FROM accounts a ");
    // `relationship_scope`.
    match relationship {
        "following" => {
            q.push("JOIN follows f ON f.target_account_id = a.id AND f.account_id = ")
                .push_bind(account_id);
        }
        "followed_by" => {
            q.push("JOIN follows f ON f.account_id = a.id AND f.target_account_id = ")
                .push_bind(account_id);
        }
        "mutual" => {
            q.push("JOIN follows f ON f.account_id = a.id AND f.target_account_id = ")
                .push_bind(account_id)
                .push(" AND a.id IN (SELECT target_account_id FROM follows WHERE account_id = ")
                .push_bind(account_id)
                .push(")");
        }
        "invited" => {
            q.push(
                "JOIN users u ON u.account_id = a.id JOIN invites i ON i.id = u.invite_id \
                 AND i.user_id = (SELECT id FROM users WHERE account_id = ",
            )
            .push_bind(account_id)
            .push(")");
        }
        other => {
            return Err(AppError::BadRequest(format!(
                "Unknown relationship: {other}"
            )))
        }
    }
    // `Account.dormant` joins the stats; `by_recent_status` reads them.
    let dormant = match given(&params.activity) {
        None => false,
        Some("dormant") => true,
        Some(other) => return Err(AppError::BadRequest(format!("Unknown activity: {other}"))),
    };
    if dormant {
        q.push(" JOIN account_stats st ON st.account_id = a.id");
    } else {
        q.push(" LEFT JOIN account_stats st ON st.account_id = a.id");
    }
    q.push(" WHERE true");
    if dormant {
        q.push(
            " AND (st.last_status_at IS NULL OR st.last_status_at < now() - interval '1 month')",
        );
    }
    if let Some(domain) = given(&params.by_domain) {
        q.push(" AND a.domain = ").push_bind(domain.to_owned());
    }
    match given(&params.location) {
        None => {}
        Some("local") => {
            q.push(" AND a.domain IS NULL");
        }
        Some("remote") => {
            q.push(" AND a.domain IS NOT NULL");
        }
        Some(other) => return Err(AppError::BadRequest(format!("Unknown location: {other}"))),
    }
    match given(&params.status) {
        None => {}
        Some("moved") => {
            q.push(" AND a.moved_to_account_id IS NOT NULL");
        }
        Some("primary") => {
            q.push(" AND a.moved_to_account_id IS NULL");
        }
        Some(other) => return Err(AppError::BadRequest(format!("Unknown status: {other}"))),
    }
    // `order_scope`.
    match (order, relationship) {
        ("active", _) => {
            q.push(" ORDER BY st.last_status_at DESC NULLS LAST, a.id DESC");
        }
        ("recent", "invited") => {
            q.push(" ORDER BY a.id DESC");
        }
        ("recent", _) => {
            q.push(" ORDER BY f.id DESC");
        }
        (other, _) => return Err(AppError::BadRequest(format!("Unknown order: {other}"))),
    }
    let page = params.page.map_or(1, |p| p.0.max(1));
    q.push(" LIMIT ")
        .push_bind(PER_PAGE)
        .push(" OFFSET ")
        .push_bind((page - 1) * PER_PAGE);
    let accounts: Vec<models::Account> = q.build_query_as().fetch_all(&state.db).await?;

    let mut result = Vec::with_capacity(accounts.len());
    for account in &accounts {
        result.push(super::build_admin_account(&state, account).await?);
    }
    let mut headers = HeaderMap::new();
    if accounts.len() as i64 == PER_PAGE {
        let query: Vec<(String, String)> =
            url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
                .into_owned()
                .filter(|(k, _)| k != "page")
                .chain(std::iter::once(("page".to_owned(), (page + 1).to_string())))
                .collect();
        let query = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(query)
            .finish();
        let link = format!(
            "<https://{}{}?{query}>; rel=\"next\"",
            state.instance.domain,
            uri.path()
        );
        if let Ok(value) = HeaderValue::from_str(&link) {
            headers.insert(axum::http::header::LINK, value);
        }
    }
    Ok((headers, Json(result)))
}
