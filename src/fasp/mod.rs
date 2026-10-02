//! Fediverse Auxiliary Service Providers: Mastodon's `Fasp` models, behind
//! its experimental `fasp` feature (`[instance] experimental_features`).
//!
//! A FASP is a service a server delegates work to — searching accounts it
//! does not know yet, recommending whom to follow, watching what trends — and
//! shares public data with. A provider registers itself
//! (`POST /api/fasp/registration`, [`api`]), an administrator confirms it and
//! picks the capabilities to use (`/api/v1/admin/fasp`), and from then on
//! each side signs what it sends the other ([`request`], [`signature`]) with
//! the Ed25519 keys exchanged at registration ([`keys`]). What is announced
//! to providers, and what is asked of them, runs in the background
//! ([`workers`]).
//!
//! The tables are Mastodon's: `fasp_providers`, `fasp_subscriptions`,
//! `fasp_backfill_requests`, `fasp_debug_callbacks` and
//! `fasp_follow_recommendations`.

pub mod api;
pub mod events;
pub mod keys;
pub mod request;
pub mod signature;
pub mod workers;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::state::AppState;

/// `Fasp::DATA_CATEGORIES`.
pub const DATA_CATEGORIES: &[&str] = &["account", "content"];

/// `Fasp::Subscription::TYPES`.
pub const SUBSCRIPTION_TYPES: &[&str] = &["lifecycle", "trends"];

/// `Fasp::Provider::RETRY_INTERVAL`: how long after its last failure an
/// unavailable provider is tried again anyway.
const RETRY_INTERVAL_SECONDS: i64 = 60 * 60;

/// `Mastodon::Feature.fasp_enabled?`.
pub fn enabled(state: &AppState) -> bool {
    state.instance.fasp_enabled()
}

/// A `fasp_providers` row.
#[derive(Debug, Clone)]
pub struct Provider {
    pub id: i64,
    pub confirmed: bool,
    pub name: String,
    pub base_url: String,
    pub sign_in_url: Option<String>,
    pub remote_identifier: String,
    pub provider_public_key_pem: String,
    pub server_private_key_pem: String,
    pub capabilities: Value,
    pub privacy_policy: Option<Value>,
    pub contact_email: Option<String>,
    pub fediverse_account: Option<String>,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
    pub delivery_last_failed_at: Option<chrono::NaiveDateTime>,
}

/// `Fasp::Capability`: one entry of a provider's `capabilities`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capability {
    pub id: String,
    #[serde(default, deserialize_with = "string_or_number")]
    pub version: String,
    #[serde(default)]
    pub enabled: bool,
}

/// `attribute :version, :string`, which casts a number to its string.
fn string_or_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => other.to_string(),
    })
}

impl Provider {
    pub async fn find(state: &AppState, id: i64) -> sqlx::Result<Option<Self>> {
        sqlx::query_as!(Provider, "SELECT * FROM fasp_providers WHERE id = $1", id)
            .fetch_optional(&state.db)
            .await
    }

    /// `Fasp::Provider.with_capability(name)`: providers that list `name`
    /// as enabled, confirmed or not.
    pub async fn with_capability(state: &AppState, name: &str) -> sqlx::Result<Vec<Self>> {
        let wanted = serde_json::json!([{ "id": name, "enabled": true }]);
        sqlx::query_as!(
            Provider,
            "SELECT * FROM fasp_providers WHERE capabilities @> $1::jsonb ORDER BY id",
            wanted
        )
        .fetch_all(&state.db)
        .await
    }

    /// `#capabilities`.
    pub fn capabilities(&self) -> Vec<Capability> {
        match &self.capabilities {
            Value::Array(items) => items
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect(),
            _ => vec![],
        }
    }

    /// `#capability_enabled?`.
    pub fn capability_enabled(&self, name: &str) -> bool {
        self.confirmed
            && self
                .capabilities()
                .iter()
                .any(|c| c.id == name && c.enabled)
    }

    /// `#url(path)`: the provider's base URL with `path` after it.
    pub fn url(&self, path: &str) -> String {
        let base = if path.starts_with('/') {
            self.base_url.strip_suffix('/').unwrap_or(&self.base_url)
        } else {
            &self.base_url
        };
        format!("{base}{path}")
    }

    /// The host the provider's failures are tracked under.
    pub fn host(&self) -> Option<String> {
        url::Url::parse(&self.base_url)
            .ok()
            .as_ref()
            .and_then(crate::federation::delivery_failures::host)
    }

    /// The seed this server signs for this provider with.
    pub fn server_key(&self) -> anyhow::Result<[u8; 32]> {
        Ok(keys::parse_private_key_pem(&self.server_private_key_pem)?.0)
    }

    /// `#server_public_key_base64`.
    pub fn server_public_key_base64(&self) -> anyhow::Result<String> {
        use base64::Engine as _;
        let (_, public) = keys::parse_private_key_pem(&self.server_private_key_pem)?;
        Ok(base64::engine::general_purpose::STANDARD.encode(public))
    }

    /// `#provider_public_key_raw`.
    pub fn provider_public_key(&self) -> anyhow::Result<[u8; 32]> {
        keys::public_key_from_pem(&self.provider_public_key_pem)
    }

    /// `#provider_public_key_fingerprint`.
    pub fn provider_public_key_fingerprint(&self) -> Option<String> {
        self.provider_public_key()
            .ok()
            .map(|raw| keys::fingerprint(&raw))
    }

    /// `#available?`: the provider's host is not marked unavailable, or its
    /// last failure is an hour old and it is worth another try.
    pub async fn available(&self, state: &AppState) -> bool {
        if self.host_available(state).await {
            return true;
        }
        sqlx::query_scalar!(
            r#"SELECT EXISTS (
                 SELECT 1 FROM fasp_providers
                  WHERE id = $1
                    AND delivery_last_failed_at < now() - make_interval(secs => $2)
               ) AS "e!""#,
            self.id,
            RETRY_INTERVAL_SECONDS as f64,
        )
        .fetch_one(&state.db)
        .await
        .unwrap_or(false)
    }

    /// `DeliveryFailureTracker#available?`, asked of the table.
    async fn host_available(&self, state: &AppState) -> bool {
        let Some(host) = self.host() else {
            return true;
        };
        !sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM unavailable_domains WHERE domain = $1) AS "e!""#,
            host
        )
        .fetch_one(&state.db)
        .await
        .unwrap_or(false)
    }

    /// `#update_availability!`: when the provider last failed, if its host
    /// is marked unavailable, and nothing if it is not.
    pub async fn update_availability(&self, state: &AppState) {
        let available = self.host_available(state).await;
        let result = sqlx::query!(
            r#"UPDATE fasp_providers
                  SET delivery_last_failed_at = CASE WHEN $2 THEN NULL ELSE now() END,
                      updated_at = now()
                WHERE id = $1 AND NOT ($2 AND delivery_last_failed_at IS NULL)"#,
            self.id,
            available,
        )
        .execute(&state.db)
        .await;
        if let Err(error) = result {
            tracing::warn!(provider = self.id, %error, "could not record a FASP's availability");
        }
    }
}

/// `ActivityPub::TagManager#uri_for(account)`.
pub async fn account_uri(state: &AppState, account_id: i64) -> sqlx::Result<Option<String>> {
    let row = sqlx::query!(
        "SELECT id, domain, uri, id_scheme, username FROM accounts WHERE id = $1",
        account_id
    )
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(|a| {
        if a.domain.is_some() {
            a.uri.unwrap_or_default()
        } else {
            crate::federation::tag::account_uri(
                &state.instance.domain,
                a.id,
                a.id_scheme,
                &a.username,
            )
        }
    }))
}

/// `ActivityPub::TagManager#uri_for(status)`: the stored URI, or for a local
/// status stored without one, the URI it is served at — a boost's activity's.
pub async fn status_uri(state: &AppState, status_id: i64) -> sqlx::Result<Option<String>> {
    let row = sqlx::query!(
        r#"SELECT s.id, s.uri, s.local, s.reblog_of_id, a.id AS account_id, a.id_scheme,
                  a.username, a.domain
             FROM statuses s JOIN accounts a ON a.id = s.account_id
            WHERE s.id = $1"#,
        status_id
    )
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(|s| {
        local_status_uri(
            state,
            &s.uri,
            s.domain.is_none(),
            s.reblog_of_id.is_some(),
            s.id,
            s.account_id,
            s.id_scheme,
            &s.username,
        )
    }))
}

#[allow(clippy::too_many_arguments)]
fn local_status_uri(
    state: &AppState,
    stored: &Option<String>,
    local: bool,
    reblog: bool,
    id: i64,
    account_id: i64,
    id_scheme: Option<i32>,
    username: &str,
) -> String {
    match stored.as_deref().filter(|u| !u.is_empty()) {
        Some(uri) => uri.to_owned(),
        None if local => {
            let uri = crate::federation::tag::status_uri(
                &state.instance.domain,
                account_id,
                id_scheme,
                username,
                id,
            );
            if reblog {
                format!("{uri}/activity")
            } else {
                uri
            }
        }
        None => String::new(),
    }
}

/// The request header a client sends when it polls again after an
/// `AsyncRefresh`, which must not start the work once more.
const ASYNC_REFRESH_ID_HEADER: &str = "mastodon-async-refresh-id";

/// `Api::V2::SuggestionsController#schedule_fasp_retrieval`: unless this
/// request follows up an earlier one, or a retrieval is still running, ask
/// providers for follow recommendations for `account_id` in the background,
/// and return the `Mastodon-Async-Refresh` header to answer with.
pub async fn schedule_follow_recommendations(
    state: &AppState,
    account_id: i64,
    headers: &axum::http::HeaderMap,
) -> Option<String> {
    if !enabled(state) || headers.contains_key(ASYNC_REFRESH_ID_HEADER) {
        return None;
    }
    let key = workers::follow_recommendation_refresh_key(account_id);
    if crate::async_refresh::AsyncRefresh::new(state, &key)
        .await
        .is_running()
    {
        return None;
    }
    let refresh = crate::async_refresh::AsyncRefresh::create(state, &key, false).await;
    workers::follow_recommendation_async(state, account_id).await;
    refresh.header_value(state, 3)
}

/// `Api::V2::SearchController#handle_fasp_requests` and the
/// `Fasp::AccountSearchWorker` that `AccountSearchService` starts with it:
/// for a search that looks for accounts, unless it follows up an earlier
/// one or the same search is still running, ask providers for accounts
/// matching it in the background, and return the `Mastodon-Async-Refresh`
/// header to answer with.
///
/// `query` is the `q` parameter as sent, which keys the refresh; providers
/// are asked for it as the account search reads it, trimmed and without a
/// leading `@`.
pub async fn schedule_account_search(
    state: &AppState,
    query: &str,
    search_type: Option<&str>,
    resolve: bool,
    headers: &axum::http::HeaderMap,
) -> Option<String> {
    if !enabled(state) || query.trim().is_empty() || headers.contains_key(ASYNC_REFRESH_ID_HEADER) {
        return None;
    }
    // `SearchService`: only a search that is not resolving a URL, and looks
    // for accounts, reaches `AccountSearchService`.
    let trimmed = query.trim();
    let url_query = resolve && (trimmed.starts_with("http://") || trimmed.starts_with("https://"));
    if url_query
        || !matches!(
            search_type.filter(|t| !t.is_empty()),
            None | Some("accounts")
        )
    {
        return None;
    }
    let key = workers::account_search_refresh_key(query);
    if crate::async_refresh::AsyncRefresh::new(state, &key)
        .await
        .is_running()
    {
        return None;
    }
    let refresh = crate::async_refresh::AsyncRefresh::create(state, &key, false).await;
    // `SearchService`'s quote normalization, then `AccountSearchService`'s
    // `strip` and `gsub(/\A@/, '')`.
    let normalized: String = trimmed
        .chars()
        .map(|c| {
            if "“”„«»「」『』《》".contains(c) {
                '"'
            } else {
                c
            }
        })
        .collect();
    let normalized = normalized.trim();
    let term = normalized
        .strip_prefix('@')
        .unwrap_or(normalized)
        .to_owned();
    workers::account_search_async(state, term, key).await;
    refresh.header_value(state, 3)
}

/// `Api::V2::SearchController`'s `before_action :handle_fasp_requests`, as a
/// layer around `GET /api/v2/search`: start the provider search the query
/// calls for, and put its `Mastodon-Async-Refresh` header on a successful
/// answer.
pub async fn search_hook(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Some(state) = request.extensions().get::<AppState>().cloned() else {
        return next.run(request).await;
    };
    if !enabled(&state) {
        return next.run(request).await;
    }
    let params: std::collections::HashMap<String, String> =
        serde_urlencoded::from_str(request.uri().query().unwrap_or("")).unwrap_or_default();
    let signed_in = request
        .extensions()
        .get::<crate::middleware::AuthenticatedUser>()
        .is_some();
    let resolve = params.get("resolve").is_some_and(|v| truthy(v));
    // An anonymous search that pages or resolves is refused before this runs
    // (`#query_pagination_error`, `#remote_resolve_error`).
    if !signed_in && (resolve || params.get("offset").is_some_and(|v| !v.is_empty())) {
        return next.run(request).await;
    }
    let header = match params.get("q") {
        Some(q) => {
            schedule_account_search(
                &state,
                q,
                params.get("type").map(String::as_str),
                resolve,
                request.headers(),
            )
            .await
        }
        None => None,
    };
    let mut response = next.run(request).await;
    if let Some(value) = header.filter(|_| response.status().is_success()) {
        if let Ok(value) = value.parse() {
            response
                .headers_mut()
                .insert(crate::async_refresh::HEADER, value);
        }
    }
    response
}

/// `ActiveModel::Type::Boolean`'s cast of a parameter, as `truthy_param?`
/// reads it.
fn truthy(value: &str) -> bool {
    !value.is_empty() && !matches!(value, "0" | "f" | "F" | "false" | "FALSE" | "off" | "OFF")
}
