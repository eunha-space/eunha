//! Mastodon's `Fasp::*Worker`s: announcing to providers what changed, and
//! asking them for what they know.
//!
//! Each is a job on the `fasp` queue (crate::jobs), retried on Sidekiq's
//! schedule as many times as its worker allows — five for announcements and
//! backfills, none for searches and recommendations. A provider is only
//! called while it is confirmed and
//! available (`Fasp::BaseWorker#with_provider`); a request that cannot reach
//! it is retried only while it stays available, and its availability is
//! recorded after every attempt.

use std::future::Future;
use std::time::Duration;

use serde_json::{json, Value};

use super::{request, Provider};
use crate::state::AppState;

/// `Fasp::BaseWorker`'s `sidekiq_options queue: 'fasp'`, with the worker's
/// own `retry`.
const fn options(retries: u32) -> crate::jobs::Options {
    crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Fasp)
        .retry(retries)
}

/// `Fasp::FollowRecommendation::MAX_AGE`.
const FOLLOW_RECOMMENDATION_MAX_AGE_SECONDS: f64 = 24.0 * 60.0 * 60.0;

fn failed(error: request::Error) -> anyhow::Error {
    anyhow::anyhow!("{error}")
}

/// `Fasp::BaseWorker#with_provider`.
async fn with_provider<Fut>(
    state: &AppState,
    provider: &Provider,
    work: Fut,
) -> Result<(), request::Error>
where
    Fut: Future<Output = Result<(), request::Error>>,
{
    if !provider.confirmed || !provider.available(state).await {
        return Ok(());
    }
    let outcome = match work.await {
        Err(error) if error.is_connection() && !provider.available(state).await => Ok(()),
        other => other,
    };
    provider.update_availability(state).await;
    outcome
}

/// A subscription and its provider.
struct Subscribed {
    subscription_id: i64,
    provider: Provider,
    threshold_timeframe: Option<i32>,
    threshold_shares: Option<i32>,
    threshold_likes: Option<i32>,
    threshold_replies: Option<i32>,
}

async fn subscriptions(
    state: &AppState,
    category: &str,
    subscription_type: &str,
) -> Result<Vec<Subscribed>, request::Error> {
    let rows = sqlx::query!(
        r#"SELECT id, fasp_provider_id, threshold_timeframe, threshold_shares,
                  threshold_likes, threshold_replies
             FROM fasp_subscriptions
            WHERE category = $1 AND subscription_type = $2
            ORDER BY id"#,
        category,
        subscription_type,
    )
    .fetch_all(&state.db)
    .await
    .map_err(anyhow::Error::from)?;
    let mut found = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(provider) = Provider::find(state, row.fasp_provider_id)
            .await
            .map_err(anyhow::Error::from)?
        else {
            continue;
        };
        found.push(Subscribed {
            subscription_id: row.id,
            provider,
            threshold_timeframe: row.threshold_timeframe,
            threshold_shares: row.threshold_shares,
            threshold_likes: row.threshold_likes,
            threshold_replies: row.threshold_replies,
        });
    }
    Ok(found)
}

/// The announcement a subscription is sent.
fn subscription_announcement(
    subscription_id: i64,
    category: &str,
    event_type: &str,
    uri: &str,
) -> Value {
    json!({
        "source": { "subscription": { "id": subscription_id.to_string() } },
        "category": category,
        "eventType": event_type,
        "objectUris": [uri],
    })
}

// ── Lifecycle events ─────────────────────────────────────────────────────

/// `Fasp::AnnounceAccountLifecycleEventWorker.perform_async(uri, event_type)`.
pub async fn announce_account_lifecycle_event(state: &AppState, uri: String, event_type: &str) {
    crate::jobs::push(
        state,
        AnnounceAccountLifecycleEventWorker {
            uri,
            event_type: event_type.to_owned(),
        },
    )
    .await;
}

/// `Fasp::AnnounceContentLifecycleEventWorker.perform_async(uri, event_type)`.
pub async fn announce_content_lifecycle_event(state: &AppState, uri: String, event_type: &str) {
    crate::jobs::push(
        state,
        AnnounceContentLifecycleEventWorker {
            uri,
            event_type: event_type.to_owned(),
        },
    )
    .await;
}

/// `Fasp::AnnounceAccountLifecycleEventWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct AnnounceAccountLifecycleEventWorker {
    pub uri: String,
    pub event_type: String,
}

impl crate::jobs::Job for AnnounceAccountLifecycleEventWorker {
    const KIND: &'static str = "Fasp::AnnounceAccountLifecycleEventWorker";
    const OPTIONS: crate::jobs::Options = options(5);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        announce_lifecycle_event(state, "account", &self.uri, &self.event_type)
            .await
            .map_err(failed)
    }
}

/// `Fasp::AnnounceContentLifecycleEventWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct AnnounceContentLifecycleEventWorker {
    pub uri: String,
    pub event_type: String,
}

impl crate::jobs::Job for AnnounceContentLifecycleEventWorker {
    const KIND: &'static str = "Fasp::AnnounceContentLifecycleEventWorker";
    const OPTIONS: crate::jobs::Options = options(5);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        announce_lifecycle_event(state, "content", &self.uri, &self.event_type)
            .await
            .map_err(failed)
    }
}

async fn announce_lifecycle_event(
    state: &AppState,
    category: &str,
    uri: &str,
    event_type: &str,
) -> Result<(), request::Error> {
    for subscribed in subscriptions(state, category, "lifecycle").await? {
        let body = subscription_announcement(subscribed.subscription_id, category, event_type, uri);
        with_provider(state, &subscribed.provider, async {
            request::post(
                state,
                &subscribed.provider,
                "/data_sharing/v0/announcements",
                Some(&body),
            )
            .await
            .map(drop)
        })
        .await?;
    }
    Ok(())
}

// ── Trends ───────────────────────────────────────────────────────────────

/// What made a status a trend candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrendSource {
    Favourite,
    Reblog,
    Reply,
}

impl TrendSource {
    fn name(self) -> &'static str {
        match self {
            Self::Favourite => "favourite",
            Self::Reblog => "reblog",
            Self::Reply => "reply",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        match name {
            "favourite" => Some(Self::Favourite),
            "reblog" => Some(Self::Reblog),
            "reply" => Some(Self::Reply),
            _ => None,
        }
    }
}

/// `Fasp::AnnounceTrendWorker.perform_async(status_id, trend_source)`.
pub async fn announce_trend(state: &AppState, status_id: i64, source: TrendSource) {
    crate::jobs::push(
        state,
        AnnounceTrendWorker {
            status_id,
            trend_source: source.name().to_owned(),
        },
    )
    .await;
}

/// `Fasp::AnnounceTrendWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct AnnounceTrendWorker {
    pub status_id: i64,
    pub trend_source: String,
}

impl crate::jobs::Job for AnnounceTrendWorker {
    const KIND: &'static str = "Fasp::AnnounceTrendWorker";
    const OPTIONS: crate::jobs::Options = options(5);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        let Some(source) = TrendSource::parse(&self.trend_source) else {
            return Ok(());
        };
        announce_trend_now(state, self.status_id, source)
            .await
            .map_err(failed)
    }
}

async fn announce_trend_now(
    state: &AppState,
    status_id: i64,
    source: TrendSource,
) -> Result<(), request::Error> {
    // A status that is gone has nothing to announce.
    let Some(status) = sqlx::query!(
        r#"SELECT a.indexable FROM statuses s JOIN accounts a ON a.id = s.account_id
        WHERE s.id = $1 AND s.deleted_at IS NULL"#,
        status_id
    )
    .fetch_optional(&state.db)
    .await
    .map_err(anyhow::Error::from)?
    else {
        return Ok(());
    };
    if !status.indexable {
        return Ok(());
    }
    let uri = super::status_uri(state, status_id)
        .await
        .map_err(anyhow::Error::from)?
        .unwrap_or_default();
    for subscribed in subscriptions(state, "content", "trends").await? {
        with_provider(state, &subscribed.provider, async {
            if trending(state, &subscribed, status_id, source).await? {
                let body = subscription_announcement(
                    subscribed.subscription_id,
                    "content",
                    "trending",
                    &uri,
                );
                request::post(
                    state,
                    &subscribed.provider,
                    "/data_sharing/v0/announcements",
                    Some(&body),
                )
                .await?;
            }
            Ok(())
        })
        .await?;
    }
    Ok(())
}

/// `#trending?`: whether the status has had at least the subscription's
/// threshold of favourites, boosts or replies within its timeframe.
async fn trending(
    state: &AppState,
    subscribed: &Subscribed,
    status_id: i64,
    source: TrendSource,
) -> Result<bool, request::Error> {
    let threshold = match source {
        TrendSource::Favourite => subscribed.threshold_likes,
        TrendSource::Reblog => subscribed.threshold_shares,
        TrendSource::Reply => subscribed.threshold_replies,
    };
    // A subscription made without a threshold has none to reach; Mastodon's
    // worker fails comparing against it.
    let (Some(threshold), Some(timeframe)) = (threshold, subscribed.threshold_timeframe) else {
        return Ok(false);
    };
    let count = match source {
        TrendSource::Favourite => {
            sqlx::query_scalar!(
                r#"SELECT count(*) AS "n!" FROM favourites
                WHERE status_id = $1 AND created_at >= now() - make_interval(mins => $2)"#,
                status_id,
                timeframe,
            )
            .fetch_one(&state.db)
            .await
        }
        TrendSource::Reblog => {
            sqlx::query_scalar!(
                r#"SELECT count(*) AS "n!" FROM statuses
                WHERE reblog_of_id = $1 AND deleted_at IS NULL
                  AND created_at >= now() - make_interval(mins => $2)"#,
                status_id,
                timeframe,
            )
            .fetch_one(&state.db)
            .await
        }
        TrendSource::Reply => {
            sqlx::query_scalar!(
                r#"SELECT count(*) AS "n!" FROM statuses
                WHERE in_reply_to_id = $1 AND deleted_at IS NULL
                  AND created_at >= now() - make_interval(mins => $2)"#,
                status_id,
                timeframe,
            )
            .fetch_one(&state.db)
            .await
        }
    }
    .map_err(anyhow::Error::from)?;
    Ok(count >= i64::from(threshold))
}

// ── Backfill ─────────────────────────────────────────────────────────────

/// `Fasp::BackfillWorker.perform_async(backfill_request_id)`.
pub async fn backfill_async(state: &AppState, backfill_request_id: i64) {
    crate::jobs::push(
        state,
        BackfillWorker {
            backfill_request_id,
        },
    )
    .await;
}

/// `Fasp::BackfillWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct BackfillWorker {
    pub backfill_request_id: i64,
}

impl crate::jobs::Job for BackfillWorker {
    const KIND: &'static str = "Fasp::BackfillWorker";
    const OPTIONS: crate::jobs::Options = options(5);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        backfill(state, self.backfill_request_id)
            .await
            .map_err(failed)
    }
}

/// `Fasp::BackfillWorker#perform`: announce the request's next batch, then
/// move its cursor past it or mark it fulfilled.
pub async fn backfill(state: &AppState, backfill_request_id: i64) -> Result<(), request::Error> {
    let Some(row) = sqlx::query!(
        "SELECT id, category, max_count, cursor, fasp_provider_id FROM fasp_backfill_requests WHERE id = $1",
        backfill_request_id
    )
    .fetch_optional(&state.db)
    .await
    .map_err(anyhow::Error::from)?
    else {
        return Ok(());
    };
    let Some(provider) = Provider::find(state, row.fasp_provider_id)
        .await
        .map_err(anyhow::Error::from)?
    else {
        return Ok(());
    };
    let cursor = row
        .cursor
        .as_deref()
        .and_then(|c| c.trim().parse::<i64>().ok());
    with_provider(state, &provider, async {
        let objects = next_objects(state, &row.category, i64::from(row.max_count), cursor).await?;
        let more = match objects.last() {
            Some((last, _)) => more_objects_available(state, &row.category, *last).await?,
            None => false,
        };
        let body = json!({
            "source": { "backfillRequest": { "id": row.id.to_string() } },
            "category": row.category,
            "objectUris": objects.iter().map(|(_, uri)| uri).collect::<Vec<_>>(),
            "moreObjectsAvailable": more,
        });
        request::post(state, &provider, "/data_sharing/v0/announcements", Some(&body)).await?;
        // `#advance!`.
        match objects.last() {
            Some((last, _)) if more => {
                sqlx::query!(
                    "UPDATE fasp_backfill_requests SET cursor = $2, updated_at = now() WHERE id = $1",
                    row.id,
                    last.to_string(),
                )
                .execute(&state.db)
                .await
            }
            _ => {
                sqlx::query!(
                    "UPDATE fasp_backfill_requests SET fulfilled = true, updated_at = now() WHERE id = $1",
                    row.id,
                )
                .execute(&state.db)
                .await
            }
        }
        .map_err(anyhow::Error::from)?;
        Ok(())
    })
    .await
}

/// `Account.discoverable.without_instance_actor`, as SQL over `accounts a`.
const DISCOVERABLE_ACCOUNTS: &str = r#"
    FROM accounts a JOIN account_stats st ON st.account_id = a.id
    WHERE (a.domain IS NOT NULL OR EXISTS (
            SELECT 1 FROM users u WHERE u.account_id = a.id
              AND u.approved AND u.confirmed_at IS NOT NULL))
      AND a.suspended_at IS NULL AND a.requested_deletion_at IS NULL
      AND a.moved_to_account_id IS NULL
      AND a.silenced_at IS NULL AND a.discoverable
      AND a.id <> -99"#;

/// `Status.indexable`: public statuses, not boosts, by indexable accounts.
const INDEXABLE_STATUSES: &str = r#"
    FROM statuses s JOIN accounts a ON a.id = s.account_id
    WHERE s.deleted_at IS NULL AND s.reblog_of_id IS NULL
      AND s.visibility = 0 AND a.indexable"#;

/// `Fasp::BackfillRequest#next_objects` with `#next_uris`: up to `limit` of
/// the category, newest first, before the cursor.
async fn next_objects(
    state: &AppState,
    category: &str,
    limit: i64,
    cursor: Option<i64>,
) -> Result<Vec<(i64, String)>, request::Error> {
    match category {
        "account" => {
            let sql = format!(
                "SELECT a.id {DISCOVERABLE_ACCOUNTS} AND ($1::bigint IS NULL OR a.id < $1)
                 ORDER BY a.id DESC LIMIT $2"
            );
            let ids: Vec<i64> = sqlx::query_scalar(&sql)
                .bind(cursor)
                .bind(limit)
                .fetch_all(&state.db)
                .await
                .map_err(anyhow::Error::from)?;
            let mut found = Vec::with_capacity(ids.len());
            for id in ids {
                let uri = super::account_uri(state, id)
                    .await
                    .map_err(anyhow::Error::from)?
                    .unwrap_or_default();
                found.push((id, uri));
            }
            Ok(found)
        }
        "content" => {
            let sql = format!(
                "SELECT s.id {INDEXABLE_STATUSES} AND ($1::bigint IS NULL OR s.id < $1)
                 ORDER BY s.id DESC LIMIT $2"
            );
            let ids: Vec<i64> = sqlx::query_scalar(&sql)
                .bind(cursor)
                .bind(limit)
                .fetch_all(&state.db)
                .await
                .map_err(anyhow::Error::from)?;
            let mut found = Vec::with_capacity(ids.len());
            for id in ids {
                let uri = super::status_uri(state, id)
                    .await
                    .map_err(anyhow::Error::from)?
                    .unwrap_or_default();
                found.push((id, uri));
            }
            Ok(found)
        }
        _ => Ok(vec![]),
    }
}

/// `Fasp::BackfillRequest#more_objects_available?`.
async fn more_objects_available(
    state: &AppState,
    category: &str,
    last: i64,
) -> Result<bool, request::Error> {
    let sql = match category {
        "account" => format!("SELECT EXISTS (SELECT 1 {DISCOVERABLE_ACCOUNTS} AND a.id < $1)"),
        "content" => format!("SELECT EXISTS (SELECT 1 {INDEXABLE_STATUSES} AND s.id < $1)"),
        _ => return Ok(false),
    };
    Ok(sqlx::query_scalar(&sql)
        .bind(last)
        .fetch_one(&state.db)
        .await
        .map_err(anyhow::Error::from)?)
}

// ── Account search ───────────────────────────────────────────────────────

/// The `AsyncRefresh` key of an account search for `query`
/// (`"fasp:account_search:#{Digest::MD5.base64digest(params[:q])}"`).
pub fn account_search_refresh_key(query: &str) -> String {
    use base64::Engine as _;
    use md5::Digest as _;
    let digest = md5::Md5::digest(query.as_bytes());
    format!(
        "fasp:account_search:{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

/// `Fasp::AccountSearchWorker.perform_async(query)`: ask every provider that
/// searches accounts for `query`, and fetch each account it names that this
/// server does not know, counting them in the refresh under `refresh_key`.
pub async fn account_search_async(state: &AppState, query: String, refresh_key: String) {
    crate::jobs::push(state, AccountSearchWorker { query, refresh_key }).await;
}

/// `Fasp::AccountSearchWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct AccountSearchWorker {
    pub query: String,
    /// The `AsyncRefresh` the search counts its results in, keyed from the
    /// query as it was sent.
    pub refresh_key: String,
}

impl crate::jobs::Job for AccountSearchWorker {
    const KIND: &'static str = "Fasp::AccountSearchWorker";
    const OPTIONS: crate::jobs::Options = options(0);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        let refresh = crate::async_refresh::FinishOnDrop::new(state, &self.refresh_key);
        let result = account_search(state, &self.query, &self.refresh_key).await;
        refresh.finish().await;
        result.map_err(failed)
    }
}

async fn account_search(
    state: &AppState,
    query: &str,
    refresh_key: &str,
) -> Result<(), request::Error> {
    if !super::enabled(state) {
        return Ok(());
    }
    let providers = Provider::with_capability(state, "account_search")
        .await
        .map_err(anyhow::Error::from)?;
    // `{ term: query, limit: 10 }.to_query`, whose keys are sorted.
    let params = serde_urlencoded::to_string([("limit", "10"), ("term", query)])
        .map_err(anyhow::Error::from)?;
    for provider in providers {
        with_provider(state, &provider, async {
            let uris = request::get(
                state,
                &provider,
                &format!("/account_search/v0/search?{params}"),
            )
            .await?;
            for uri in uri_list(uris) {
                if known_account(state, &uri).await? {
                    continue;
                }
                if fetch_account(state, &uri).await.is_some() {
                    crate::async_refresh::increment_result_count(state, refresh_key, 1).await;
                }
            }
            Ok(())
        })
        .await?;
    }
    Ok(())
}

/// The URIs a provider answered with.
fn uri_list(answer: Option<Value>) -> Vec<String> {
    match answer {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect(),
        _ => vec![],
    }
}

/// `Account.where(uri:).any?`.
async fn known_account(state: &AppState, uri: &str) -> Result<bool, request::Error> {
    Ok(sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM accounts WHERE uri = $1) AS "e!""#,
        uri
    )
    .fetch_one(&state.db)
    .await
    .map_err(anyhow::Error::from)?)
}

/// `ActivityPub::FetchRemoteActorService#call(uri)`: the account, if it
/// could be fetched.
async fn fetch_account(state: &AppState, uri: &str) -> Option<i64> {
    match crate::api::ap::inbox::resolve_or_fetch_remote_account(state, uri).await {
        Ok(id) => Some(id),
        Err(error) => {
            tracing::debug!(uri, ?error, "could not fetch an account a FASP named");
            None
        }
    }
}

// ── Follow recommendations ───────────────────────────────────────────────

/// The `AsyncRefresh` key of an account's follow recommendations.
pub fn follow_recommendation_refresh_key(account_id: i64) -> String {
    format!("fasp:follow_recommendation:{account_id}")
}

/// `Fasp::FollowRecommendationWorker.perform_async(account_id)`: ask every
/// provider that recommends follows whom `account_id` might follow, fetch
/// each account it names that this server does not know, and keep those as
/// the account's recommendations.
pub async fn follow_recommendation_async(state: &AppState, account_id: i64) {
    crate::jobs::push(state, FollowRecommendationWorker { account_id }).await;
}

/// `Fasp::FollowRecommendationWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct FollowRecommendationWorker {
    pub account_id: i64,
}

impl crate::jobs::Job for FollowRecommendationWorker {
    const KIND: &'static str = "Fasp::FollowRecommendationWorker";
    const OPTIONS: crate::jobs::Options = options(0);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        let refresh_key = follow_recommendation_refresh_key(self.account_id);
        let refresh = crate::async_refresh::FinishOnDrop::new(state, &refresh_key);
        let result = follow_recommendation(state, self.account_id, &refresh_key).await;
        refresh.finish().await;
        result.map_err(failed)
    }
}

async fn follow_recommendation(
    state: &AppState,
    account_id: i64,
    refresh_key: &str,
) -> Result<(), request::Error> {
    if !super::enabled(state) {
        return Ok(());
    }
    let Some(account_uri) = super::account_uri(state, account_id)
        .await
        .map_err(anyhow::Error::from)?
    else {
        return Ok(());
    };
    let providers = Provider::with_capability(state, "follow_recommendation")
        .await
        .map_err(anyhow::Error::from)?;
    let params = serde_urlencoded::to_string([("accountUri", account_uri.as_str())])
        .map_err(anyhow::Error::from)?;
    for provider in providers {
        with_provider(state, &provider, async {
            let uris = request::get(
                state,
                &provider,
                &format!("/follow_recommendation/v0/accounts?{params}"),
            )
            .await?;
            for uri in uri_list(uris) {
                if known_account(state, &uri).await? {
                    continue;
                }
                let Some(recommended) = fetch_account(state, &uri).await else {
                    continue;
                };
                // `Fasp::FollowRecommendation.find_or_create_by`.
                sqlx::query!(
                    r#"INSERT INTO fasp_follow_recommendations
                         (requesting_account_id, recommended_account_id, created_at, updated_at)
                       SELECT $1, $2, now(), now()
                        WHERE NOT EXISTS (SELECT 1 FROM fasp_follow_recommendations
                                           WHERE requesting_account_id = $1
                                             AND recommended_account_id = $2)"#,
                    account_id,
                    recommended,
                )
                .execute(&state.db)
                .await
                .map_err(anyhow::Error::from)?;
                crate::async_refresh::increment_result_count(state, refresh_key, 1).await;
            }
            Ok(())
        })
        .await?;
    }
    Ok(())
}

/// `Scheduler::Fasp::FollowRecommendationCleanupScheduler`, daily: forget
/// recommendations more than a day old.
pub async fn run_follow_recommendation_cleanup(state: AppState) {
    while !state.stop.is_cancelled() {
        if super::enabled(&state) {
            if let Err(error) = sqlx::query!(
                "DELETE FROM fasp_follow_recommendations WHERE created_at < now() - make_interval(secs => $1)",
                FOLLOW_RECOMMENDATION_MAX_AGE_SECONDS,
            )
            .execute(&state.db)
            .await
            {
                tracing::warn!(%error, "could not clean up FASP follow recommendations");
            }
        }
        crate::background::rest(&state.stop, Duration::from_secs(24 * 60 * 60)).await;
    }
}

// ── Debug ────────────────────────────────────────────────────────────────

/// `Fasp::Provider::DebugConcern#perform_debug_call`.
pub async fn perform_debug_call(
    state: &AppState,
    provider: &Provider,
) -> Result<(), request::Error> {
    request::post(
        state,
        provider,
        "/debug/v0/callback/logs",
        Some(&json!({ "hello": "world" })),
    )
    .await
    .map(drop)
}
