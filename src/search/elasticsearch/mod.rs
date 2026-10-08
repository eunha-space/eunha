//! The Elasticsearch (or OpenSearch) backend: Mastodon's Chewy indexes, the
//! queries its search services send, and the indexing that keeps the
//! indexes current.
//!
//! The client is a thin layer over the REST API with reqwest — the handful
//! of endpoints Chewy uses (index create and delete, mappings and settings,
//! `_bulk`, `_search`, scroll) need nothing more. Like Mastodon's
//! `SearchStoplight`, it stops sending searches for five minutes after ten
//! failures in a row; a search that is not sent falls back as a failed one
//! does: accounts and hashtags to the database, posts to nothing.

pub mod deploy;
pub mod documents;
pub mod indexes;
pub mod indexing;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use serde_json::{json, Value};

use crate::config::ElasticsearchConfig;
use crate::db::models::Account;
use crate::state::AppState;

pub use indexes::Index;

/// `SearchStoplight::STOPLIGHT_THRESHOLD`.
const STOPLIGHT_THRESHOLD: u32 = 10;
/// `SearchStoplight::STOPLIGHT_COOL_OFF_TIME`.
const STOPLIGHT_COOL_OFF: Duration = Duration::from_secs(5 * 60);

/// A connection to the cluster an instance searches.
pub struct Client {
    http: reqwest::Client,
    base: String,
    user: Option<String>,
    pass: Option<String>,
    prefix: Option<String>,
    preset: Option<String>,
    query_timeout: String,
    failures: AtomicU32,
    red_until: Mutex<Option<Instant>>,
}

/// An error answer from the cluster.
#[derive(Debug)]
pub struct ServerError {
    pub status: reqwest::StatusCode,
    pub body: String,
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "search server answered {}: {}", self.status, self.body)
    }
}

impl std::error::Error for ServerError {}

impl Client {
    pub fn new(config: &ElasticsearchConfig) -> anyhow::Result<Self> {
        // `Faraday.ignore_env_proxy = true`: the search server is never
        // reached through a proxy.
        let mut builder = reqwest::Client::builder()
            .user_agent(crate::version::USER_AGENT)
            .no_proxy()
            .timeout(Duration::from_secs(60));
        if let Some(path) = config.ca_file.as_deref().filter(|p| !p.is_empty()) {
            let pem = std::fs::read(path).with_context(|| format!("reading ES CA file {path}"))?;
            builder = builder.add_root_certificate(
                reqwest::Certificate::from_pem(&pem).context("parsing the ES CA file")?,
            );
        }
        Ok(Self {
            http: builder.build()?,
            base: config.base_url(),
            user: config.user.clone().filter(|u| !u.is_empty()),
            pass: config.pass.clone().filter(|p| !p.is_empty()),
            prefix: config.prefix.clone().filter(|p| !p.is_empty()),
            preset: config.preset.clone().filter(|p| !p.is_empty()),
            query_timeout: config.query_timeout.clone(),
            failures: AtomicU32::new(0),
            red_until: Mutex::new(None),
        })
    }

    /// Chewy's `index_name`: the prefix and the base name joined with `_`.
    pub fn index_name(&self, base: &str) -> String {
        match &self.prefix {
            Some(prefix) => format!("{prefix}_{base}"),
            None => base.to_string(),
        }
    }

    pub fn preset(&self) -> Option<&str> {
        self.preset.as_deref()
    }

    pub fn query_timeout(&self) -> &str {
        &self.query_timeout
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let url = format!("{}/{}", self.base, path.trim_start_matches('/'));
        let request = self.http.request(method, url);
        match &self.user {
            Some(user) => request.basic_auth(user, self.pass.as_deref()),
            None => request,
        }
    }

    async fn finish(response: reqwest::Response) -> anyhow::Result<Value> {
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(ServerError { status, body }.into());
        }
        if body.is_empty() {
            return Ok(Value::Null);
        }
        Ok(serde_json::from_str(&body)?)
    }

    /// One JSON request, its JSON answer.
    pub async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> anyhow::Result<Value> {
        let mut request = self.request(method, path);
        if let Some(body) = body {
            request = request.json(body);
        }
        Self::finish(request.send().await?).await
    }

    /// Whether `index` exists.
    pub async fn exists(&self, index: &str) -> anyhow::Result<bool> {
        let response = self.request(reqwest::Method::HEAD, index).send().await?;
        match response.status() {
            s if s.is_success() => Ok(true),
            reqwest::StatusCode::NOT_FOUND => Ok(false),
            status => Err(ServerError {
                status,
                body: String::new(),
            }
            .into()),
        }
    }

    /// `_bulk`, one action and its document per pair of lines. Fails when
    /// any action failed, other than deleting a document that was not
    /// there.
    pub async fn bulk(&self, index: &str, lines: &[Value]) -> anyhow::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let mut body = String::new();
        for line in lines {
            body.push_str(&serde_json::to_string(line)?);
            body.push('\n');
        }
        let response = self
            .request(reqwest::Method::POST, &format!("{index}/_bulk"))
            .header(reqwest::header::CONTENT_TYPE, "application/x-ndjson")
            .body(body)
            .send()
            .await?;
        let answer = Self::finish(response).await?;
        if answer["errors"].as_bool() == Some(true) {
            let failed: Vec<&Value> = answer["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| item.as_object()?.values().next())
                .filter(|result| result.get("error").is_some())
                .collect();
            if let Some(first) = failed.first() {
                anyhow::bail!(
                    "{} bulk actions on {index} failed, the first with {}",
                    failed.len(),
                    first["error"]
                );
            }
        }
        Ok(())
    }

    /// `_search` on `indexes`.
    pub async fn search(&self, indexes: &[String], body: &Value) -> anyhow::Result<Value> {
        self.send(
            reqwest::Method::POST,
            &format!("{}/_search", indexes.join(",")),
            Some(body),
        )
        .await
    }

    /// `elastic_stoplight_wrapper.run`: the search, unless the light is red.
    /// `None` when it is, or when the search failed.
    async fn guarded<T>(
        &self,
        search: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> Option<T> {
        if let Some(until) = *self.red_until.lock().unwrap_or_else(|e| e.into_inner()) {
            if Instant::now() < until {
                return None;
            }
        }
        match search.await {
            Ok(found) => {
                self.failures.store(0, Ordering::Relaxed);
                Some(found)
            }
            Err(error) => {
                let failures = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::warn!(%error, failures, "search server request failed");
                if failures >= STOPLIGHT_THRESHOLD {
                    *self.red_until.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(Instant::now() + STOPLIGHT_COOL_OFF);
                }
                None
            }
        }
    }
}

/// The ids of the hits, in order, and which index each came from.
fn hits(answer: &Value) -> Vec<(String, String)> {
    answer["hits"]["hits"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|hit| {
            Some((
                hit["_id"].as_str()?.to_string(),
                hit["_index"].as_str().unwrap_or("").to_string(),
            ))
        })
        .collect()
}

fn hit_ids(answer: &Value) -> Vec<i64> {
    hits(answer)
        .into_iter()
        .filter_map(|(id, _)| id.parse().ok())
        .collect()
}

/// Rows loaded for `ids`, in the order of `ids`, without the missing ones
/// (`objects.compact`).
fn in_order<T>(ids: &[i64], rows: Vec<T>, id: impl Fn(&T) -> i64) -> Vec<T> {
    let mut by_id: std::collections::HashMap<i64, T> =
        rows.into_iter().map(|row| (id(&row), row)).collect();
    ids.iter().filter_map(|i| by_id.remove(i)).collect()
}

/// Who `account_id` follows, and itself: `following_ids`.
async fn following_ids(state: &AppState, account_id: i64) -> anyhow::Result<Vec<i64>> {
    let mut ids: Vec<i64> =
        sqlx::query_scalar("SELECT target_account_id FROM follows WHERE account_id = $1")
            .bind(account_id)
            .fetch_all(&state.db)
            .await?;
    ids.push(account_id);
    Ok(ids)
}

/// `AccountSearchService#from_elasticsearch`: `None` without Elasticsearch,
/// or when the search failed, for the database to answer instead.
pub async fn accounts(
    state: &AppState,
    terms: &str,
    viewer: Option<i64>,
    options: &crate::search::accounts::Options,
    limit: i64,
) -> Option<Vec<Account>> {
    let client = state.search.as_ref()?;
    client
        .guarded(async {
            let core = if options.use_searchable_text {
                // `FullQueryBuilder#core_query`.
                json!({ "dis_max": {
                    "queries": [
                        { "match": { "username": { "query": terms, "analyzer": "word_join_analyzer" } } },
                        { "match": { "display_name": { "query": terms, "analyzer": "word_join_analyzer" } } },
                        { "multi_match": {
                            "query": terms,
                            "type": "best_fields",
                            "fields": ["text", "text.*"],
                            "operator": "and",
                        } },
                    ],
                    "tie_breaker": 0.5,
                } })
            } else {
                // `AutocompleteQueryBuilder#core_query`.
                json!({ "dis_max": { "queries": [
                    { "multi_match": { "query": terms, "type": "most_fields", "fields": ["username", "username.*"] } },
                    { "multi_match": { "query": terms, "type": "most_fields", "fields": ["display_name", "display_name.*"] } },
                ] } })
            };
            let mut must = vec![core];
            let mut should = vec![];
            if let Some(viewer) = viewer {
                let ids = following_ids(state, viewer).await?;
                if options.following {
                    must.push(json!({ "terms": { "id": ids } }));
                } else {
                    should.push(json!({ "terms": { "id": ids, "boost": 100 } }));
                }
            }
            let body = json!({
                "query": { "bool": {
                    "must": { "function_score": {
                        "query": { "bool": { "must": must, "must_not": [] } },
                        "functions": [{ "script_score": { "script": {
                            "source": "Math.log10((Math.max(doc['followers_count'].value, 0) + 1))",
                        } } }],
                    } },
                    "should": should,
                } },
                "size": limit,
                "from": options.offset,
                "timeout": client.query_timeout(),
            });
            let answer = client
                .search(&[client.index_name(Index::Accounts.base_name())], &body)
                .await?;
            let ids = hit_ids(&answer);
            // `objects`, through the index scope, `Account.searchable`.
            let rows = sqlx::query_as::<_, Account>(
                "SELECT accounts.* FROM accounts \
                 LEFT JOIN users ON users.account_id = accounts.id \
                 WHERE accounts.id = ANY($1) \
                   AND accounts.suspended_at IS NULL AND accounts.moved_to_account_id IS NULL \
                   AND (accounts.domain IS NOT NULL OR (users.approved AND users.confirmed_at IS NOT NULL))",
            )
            .bind(&ids)
            .fetch_all(&state.db)
            .await?;
            Ok(in_order(&ids, rows, |a| a.id))
        })
        .await
}

/// `TagSearchService#from_elasticsearch`.
pub async fn tags(
    state: &AppState,
    query: &str,
    options: &crate::search::tags::Options,
) -> Option<Vec<crate::search::tags::FoundTag>> {
    let client = state.search.as_ref()?;
    client
        .guarded(async {
            let mut filter = vec![];
            if options.exclude_unreviewed {
                filter.push(json!({ "bool": { "should": [
                    { "term": { "reviewed": { "value": true } } },
                    { "match": { "name": { "query": query } } },
                ] } }));
            }
            let body = json!({
                "query": { "bool": {
                    "must": [{ "function_score": {
                        "query": { "multi_match": {
                            "query": query,
                            "fields": ["name.edge_ngram", "name"],
                            "type": "most_fields",
                            "operator": "and",
                        } },
                        "functions": [
                            { "field_value_factor": { "field": "usage", "modifier": "log2p", "missing": 0 } },
                            { "gauss": { "last_status_at": { "scale": "7d", "offset": "14d", "decay": 0.5 } } },
                        ],
                        "boost_mode": "multiply",
                    } }],
                    "filter": filter,
                } },
                "size": options.limit,
                "from": options.offset,
            });
            let answer = client
                .search(&[client.index_name(Index::Tags.base_name())], &body)
                .await?;
            let ids = hit_ids(&answer);
            let rows = sqlx::query_as::<_, crate::search::tags::FoundTag>(
                "SELECT id, name, coalesce(display_name, name) AS display_name FROM tags \
                 WHERE id = ANY($1) AND (listable = TRUE OR listable IS NULL)",
            )
            .bind(&ids)
            .fetch_all(&state.db)
            .await?;
            let mut results = in_order(&ids, rows, |t| t.id);
            // `ensure_exact_match`: the tag that is the query goes first.
            if options.offset == 0 {
                let normalized = crate::search::tags::normalize(query);
                let exact = match results
                    .iter()
                    .position(|t| t.name.to_lowercase() == normalized)
                {
                    Some(i) => Some(results.remove(i)),
                    None => crate::search::tags::find_normalized(state, query).await?,
                };
                if let Some(exact) = exact {
                    results.retain(|t| t.id != exact.id);
                    results.insert(0, exact);
                }
            }
            Ok(results)
        })
        .await
}

/// The peers search on `InstancesIndex`; `None` when Elasticsearch is off.
///
/// Upstream queries the index directly, outside the stoplight the other
/// searches go through, so a failure is the caller's to raise rather than a
/// reason to ask the database.
pub async fn peers(state: &AppState, domain: &str) -> Option<anyhow::Result<Vec<String>>> {
    let client = state.search.as_ref()?;
    let body = json!({
        "query": { "function_score": {
            "query": { "prefix": { "domain": domain } },
            "field_value_factor": { "field": "accounts_count", "modifier": "log2p" },
        } },
        "size": crate::search::peers::LIMIT,
        "_source": ["domain"],
    });
    Some(
        client
            .search(&[client.index_name(Index::Instances.base_name())], &body)
            .await
            .map(|answer| {
                answer["hits"]["hits"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|hit| hit["_source"]["domain"].as_str().map(str::to_owned))
                    .collect()
            }),
    )
}

/// What a post search was asked for beyond the query.
#[derive(Debug, Clone, Default)]
pub struct StatusOptions {
    pub limit: i64,
    pub offset: i64,
    pub account_id: Option<i64>,
    pub min_id: Option<i64>,
    pub max_id: Option<i64>,
}

/// Why a post search answered with an error rather than results.
#[derive(Debug)]
pub enum StatusSearchError {
    /// `account_id` names no account: `ActiveRecord::RecordNotFound`, a 404.
    AccountNotFound,
    /// `Date::Error`, a 422.
    InvalidDate,
    App(crate::error::AppError),
}

impl From<sqlx::Error> for StatusSearchError {
    fn from(e: sqlx::Error) -> Self {
        StatusSearchError::App(e.into())
    }
}

impl From<crate::error::AppError> for StatusSearchError {
    fn from(e: crate::error::AppError) -> Self {
        StatusSearchError::App(e)
    }
}

/// `Mastodon::Snowflake.to_time(id).iso8601`.
fn snowflake_iso8601(id: i64) -> String {
    let seconds = (id >> 16) / 1000;
    chrono::DateTime::from_timestamp(seconds, 0)
        .unwrap_or_default()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// `StatusesSearchService#call`: the ids of the posts found, in order, after
/// `StatusFilter`. Empty without Elasticsearch, when the query does not
/// parse, or when the search fails.
pub async fn statuses(
    state: &AppState,
    query: &str,
    viewer: i64,
    options: &StatusOptions,
) -> Result<Vec<i64>, StatusSearchError> {
    let Some(client) = state.search.as_ref() else {
        return Ok(vec![]);
    };
    // `convert_deprecated_options!`.
    let mut query = query.trim().to_string();
    let mut syntax = vec![];
    if let Some(account_id) = options.account_id {
        let acct: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT username, domain FROM accounts WHERE id = $1")
                .bind(account_id)
                .fetch_optional(&state.db)
                .await?;
        let (username, domain) = acct.ok_or(StatusSearchError::AccountNotFound)?;
        syntax.push(match domain {
            Some(domain) => format!("from:@{username}@{domain}"),
            None => format!("from:@{username}"),
        });
    }
    if let Some(min_id) = options.min_id {
        syntax.push(format!("after:\"{}\"", snowflake_iso8601(min_id)));
    }
    if let Some(max_id) = options.max_id {
        syntax.push(format!("before:\"{}\"", snowflake_iso8601(max_id)));
    }
    if !syntax.is_empty() {
        query = format!("{query} {}", syntax.join(" ")).trim().to_string();
    }

    use crate::search::query::{parse, Query, QueryError};
    let parsed = match parse(&query).and_then(Query::new) {
        Ok(parsed) => parsed,
        Err(QueryError::InvalidDate) => return Err(StatusSearchError::InvalidDate),
        Err(QueryError::ParseFailed) => return Ok(vec![]),
        Err(QueryError::Unsupported(e)) => {
            return Err(StatusSearchError::App(crate::error::AppError::Unrescued(e)))
        }
    };

    // `from:` handles, looked up before the request is built.
    let mut from_ids = std::collections::HashMap::new();
    for term in parsed.from_terms() {
        let id = if term == "me" {
            viewer
        } else {
            let handle = term.strip_prefix('@').unwrap_or(&term);
            let mut parts = handle.split('@');
            let username = parts.next().unwrap_or("");
            let domain = parts
                .next()
                .filter(|d| !crate::search::is_local_domain(state, d));
            crate::search::accounts::find_remote(state, username, domain)
                .await?
                .map_or(-1, |a| a.id)
        };
        from_ids.insert(term, id);
    }
    let time_zone: Option<String> =
        sqlx::query_scalar("SELECT time_zone FROM users WHERE account_id = $1")
            .bind(viewer)
            .fetch_optional(&state.db)
            .await?
            .flatten();
    let time_zone = time_zone
        .as_deref()
        .filter(|tz| !tz.is_empty())
        .and_then(crate::time_zones::find)
        .map_or_else(|| "UTC".to_string(), |tz| tz.name().to_string());

    let indexes: Vec<String> = parsed
        .indexes()
        .iter()
        .map(|base| client.index_name(base))
        .collect();
    let body = json!({
        "query": parsed.request(
            viewer,
            &time_zone,
            |base| client.index_name(base),
            |term| from_ids.get(term).copied().unwrap_or(-1),
        ),
        "timeout": client.query_timeout(),
        "collapse": { "field": "id" },
        "sort": [{ "id": { "order": "desc" } }],
        "size": options.limit,
        "from": options.offset,
    });
    let Some(answer) = client.guarded(client.search(&indexes, &body)).await else {
        return Ok(vec![]);
    };
    let statuses_index = client.index_name(Index::Statuses.base_name());
    let mut ordered = vec![];
    let mut from_statuses_index = vec![];
    for (id, index) in hits(&answer) {
        let Ok(id) = id.parse::<i64>() else { continue };
        if index == statuses_index {
            from_statuses_index.push(id);
        }
        ordered.push(id);
    }
    // `objects` through each index's scope, then `StatusFilter#filtered?`.
    let kept: Vec<i64> = sqlx::query_scalar(
        "SELECT s.id FROM statuses s JOIN accounts a ON a.id = s.account_id \
         WHERE s.id = ANY($1) AND s.deleted_at IS NULL AND s.reblog_of_id IS NULL \
           AND (s.id = ANY($3) OR (s.visibility = 0 AND a.indexable)) \
           AND (s.account_id = $2 OR ( \
             a.suspended_at IS NULL AND a.requested_deletion_at IS NULL \
             AND (CASE \
               WHEN s.visibility IN (3, 4) THEN \
                 EXISTS (SELECT 1 FROM mentions m WHERE m.status_id = s.id AND m.account_id = $2) \
               WHEN s.visibility = 2 THEN \
                 EXISTS (SELECT 1 FROM follows f WHERE f.account_id = $2 AND f.target_account_id = s.account_id) \
                 OR EXISTS (SELECT 1 FROM mentions m WHERE m.status_id = s.id AND m.account_id = $2) \
               ELSE NOT EXISTS (SELECT 1 FROM blocks b WHERE b.account_id = s.account_id AND b.target_account_id = $2) \
             END) \
             AND NOT EXISTS (SELECT 1 FROM blocks b WHERE b.account_id = $2 AND b.target_account_id = s.account_id) \
             AND NOT (a.domain IS NOT NULL AND EXISTS ( \
               SELECT 1 FROM account_domain_blocks d WHERE d.account_id = $2 AND d.domain = a.domain)) \
             AND NOT EXISTS (SELECT 1 FROM mutes mu WHERE mu.account_id = $2 AND mu.target_account_id = s.account_id) \
             AND NOT (a.silenced_at IS NOT NULL AND NOT EXISTS ( \
               SELECT 1 FROM follows f WHERE f.account_id = $2 AND f.target_account_id = s.account_id)) \
           ))",
    )
    .bind(&ordered)
    .bind(viewer)
    .bind(&from_statuses_index)
    .fetch_all(&state.db)
    .await?;
    let kept: std::collections::HashSet<i64> = kept.into_iter().collect();
    Ok(ordered.into_iter().filter(|id| kept.contains(id)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_names_take_the_prefix() {
        let mut config = ElasticsearchConfig::default();
        let client = Client::new(&config).unwrap();
        assert_eq!(client.index_name("accounts"), "accounts");
        config.prefix = Some("eunha".into());
        let client = Client::new(&config).unwrap();
        assert_eq!(
            client.index_name("public_statuses"),
            "eunha_public_statuses"
        );
    }

    #[test]
    fn base_urls() {
        let mut config = ElasticsearchConfig::default();
        assert_eq!(config.base_url(), "http://localhost:9200");
        config.host = "https://search.example/".into();
        config.port = 443;
        assert_eq!(config.base_url(), "https://search.example:443");
    }

    #[test]
    fn snowflake_times() {
        // 2022-11-01T00:00:00Z, 1667260800000 ms, shifted into an id.
        assert_eq!(
            snowflake_iso8601(1_667_260_800_000 << 16),
            "2022-11-01T00:00:00Z"
        );
    }

    #[tokio::test]
    async fn the_light_turns_red_after_ten_failures() {
        let client = Client::new(&ElasticsearchConfig::default()).unwrap();
        for _ in 0..STOPLIGHT_THRESHOLD {
            assert!(client
                .guarded(async { anyhow::Result::<()>::Err(anyhow::anyhow!("down")) })
                .await
                .is_none());
        }
        let mut ran = false;
        let _ = client
            .guarded(async {
                ran = true;
                anyhow::Ok(())
            })
            .await;
        assert!(!ran, "a red light sends nothing");
    }
}
