//! Which servers have stopped answering: Mastodon's delivery failure tracker
//! (`app/lib/delivery_failure_tracker.rb`), over the same Redis keys and the
//! same `unavailable_domains` table.
//!
//! Every attempt at a delivery that fails in a way that may pass — and every
//! one the circuit breaker holds back — adds today's date to the set
//! `exhausted_deliveries:<host>`. A host with failures on seven different days
//! is marked unavailable, and the fan-outs leave it out. Any delivery to it
//! that goes through, and any request it signs to our inbox, clears both. A
//! delivery queued before its host was marked is dropped unsent when it comes
//! due, unless it is a `Follow`, which Mastodon sends regardless
//! (`bypass_availability` in `FollowService`). An answer that will not change,
//! such as a 404, is the server working, and counts for nothing. Requests to
//! a FASP are counted in minutes instead, five of which mark it.
//!
//! They are kept in [`Store`]: the Redis sets under the instance's key
//! prefix, and the table.
//!
//! Mastodon: app/lib/delivery_failure_tracker.rb
//! Mastodon: app/models/unavailable_domain.rb

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use ojak::deliverer::{AttemptOutcome, DeliveryAttempt};
use redis::aio::ConnectionManager;

/// How long the hosts marked unavailable are taken from memory before the
/// table is read again, for a host another process marked.
const UNAVAILABLE_TTL: Duration = Duration::from_secs(60);

/// What a host's failures are counted in (`resolution:`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Resolution {
    /// Days, which deliveries are counted in.
    Days,
    /// Minutes, which requests to a FASP are counted in.
    Minutes,
}

impl Resolution {
    /// How many different days or minutes with failures mark a host
    /// unavailable (`FAILURE_THRESHOLDS`).
    fn threshold(self) -> usize {
        match self {
            Self::Days => 7,
            Self::Minutes => 5,
        }
    }

    /// The day or minute `at` falls in, in UTC, as Mastodon writes it into a
    /// host's failures: `20250102`, or `202501021504`.
    fn stamp(self, at: chrono::DateTime<chrono::Utc>) -> String {
        match self {
            Self::Days => at.format("%Y%m%d").to_string(),
            Self::Minutes => at.format("%Y%m%d%H%M").to_string(),
        }
    }
}

/// An instance's tracker: deliveries counted in days, and requests to a FASP
/// in minutes, over one store.
#[derive(Clone)]
pub struct DeliveryFailureTracker {
    store: Store,
}

/// Where an instance's failures and marks are kept.
#[derive(Clone)]
pub struct Store {
    db: sqlx::PgPool,
    redis: ConnectionManager,
    keys: crate::redis_keys::RedisKeyspace,
    unavailable: Arc<Unavailable>,
}

/// The hosts marked unavailable, as last read.
struct Unavailable {
    hosts: RwLock<(Option<Instant>, Arc<HashSet<String>>)>,
    reading: AtomicBool,
}

impl Store {
    fn key(&self, host: &str) -> String {
        self.keys.key(format!("exhausted_deliveries:{host}"))
    }

    /// Read the table again at the next question, after a change here.
    fn forget(&self) {
        self.unavailable.hosts.write().expect("unavailable hosts").0 = None;
    }
}

impl Store {
    /// Add `stamp` to `host`'s failures, if it is not among them, and answer
    /// how many different stamps it then has.
    async fn add_failure(&self, host: &str, stamp: &str) -> anyhow::Result<usize> {
        let mut redis = self.redis.clone();
        let (_, failures): (i64, usize) = redis::pipe()
            .cmd("SADD")
            .arg(self.key(host))
            .arg(stamp)
            .cmd("SCARD")
            .arg(self.key(host))
            .query_async(&mut redis)
            .await?;
        Ok(failures)
    }

    /// Forget `host`'s failures, answering whether it had any.
    async fn clear_failures(&self, host: &str) -> anyhow::Result<bool> {
        let mut redis = self.redis.clone();
        let cleared: i64 = redis::cmd("DEL")
            .arg(self.key(host))
            .query_async(&mut redis)
            .await?;
        Ok(cleared > 0)
    }

    /// `UnavailableDomain.create`, which a domain already there fails
    /// validation for and leaves as it is.
    async fn mark_unavailable(&self, host: &str) -> anyhow::Result<bool> {
        let marked = sqlx::query!(
            r#"INSERT INTO unavailable_domains (domain, created_at, updated_at)
                   VALUES ($1, now(), now())
                   ON CONFLICT (domain) DO NOTHING"#,
            host,
        )
        .execute(&self.db)
        .await?;
        let marked = marked.rows_affected() > 0;
        if marked {
            tracing::info!(host, "marked domain unavailable");
            self.forget();
        }
        Ok(marked)
    }

    /// `UnavailableDomain.find_by(domain:)&.destroy`.
    async fn mark_available(&self, host: &str) -> anyhow::Result<bool> {
        let unmarked = sqlx::query!("DELETE FROM unavailable_domains WHERE domain = $1", host)
            .execute(&self.db)
            .await?;
        let unmarked = unmarked.rows_affected() > 0;
        if unmarked {
            tracing::info!(host, "domain available again");
            self.forget();
        }
        Ok(unmarked)
    }

    /// From memory, read again in the background once it is a minute old.
    fn is_unavailable(&self, host: &str) -> bool {
        let (read, hosts) = self
            .unavailable
            .hosts
            .read()
            .expect("unavailable hosts")
            .clone();
        if read.is_none_or(|read| read.elapsed() >= UNAVAILABLE_TTL)
            && !self.unavailable.reading.swap(true, Ordering::AcqRel)
        {
            let store = self.clone();
            crate::tenants::spawn(async move {
                let hosts = sqlx::query_scalar!("SELECT domain FROM unavailable_domains")
                    .fetch_all(&store.db)
                    .await;
                match hosts {
                    Ok(hosts) => {
                        *store.unavailable.hosts.write().expect("unavailable hosts") =
                            (Some(Instant::now()), Arc::new(hosts.into_iter().collect()));
                    }
                    Err(error) => tracing::warn!(%error, "could not read unavailable domains"),
                }
                store.unavailable.reading.store(false, Ordering::Release);
            });
        }
        hosts.contains(host)
    }
}

impl DeliveryFailureTracker {
    pub fn new(
        db: sqlx::PgPool,
        redis: ConnectionManager,
        keys: crate::redis_keys::RedisKeyspace,
    ) -> Self {
        let store = Store {
            db,
            redis,
            keys,
            unavailable: Arc::new(Unavailable {
                hosts: RwLock::new((None, Arc::default())),
                reading: AtomicBool::new(false),
            }),
        };
        Self { store }
    }

    fn store(&self) -> &Store {
        &self.store
    }

    /// Count a failure reaching `host`, at `resolution`. A host is marked
    /// once it has failures on as many different days or minutes as the
    /// resolution's threshold; one already marked stays as it is.
    async fn track_failure_in(&self, host: &str, resolution: Resolution) -> anyhow::Result<()> {
        let failures = self
            .store
            .add_failure(host, &resolution.stamp(chrono::Utc::now()))
            .await?;
        if failures >= resolution.threshold() {
            self.store.mark_unavailable(host).await?;
        }
        Ok(())
    }

    /// Count a failed attempt at a delivery to `host`
    /// (`DeliveryFailureTracker#track_failure!`).
    pub async fn track_failure(&self, host: &str) -> anyhow::Result<()> {
        self.track_failure_in(host, Resolution::Days).await
    }

    /// Count a failed request to `host` at the resolution of minutes
    /// (`DeliveryFailureTracker.new(url, resolution: :minutes)
    /// .track_failure!`), which is how requests to a FASP are tracked: five
    /// different minutes with failures mark the host unavailable.
    pub async fn track_failure_minutes(&self, host: &str) -> anyhow::Result<()> {
        self.track_failure_in(host, Resolution::Minutes).await
    }

    /// Clear `host`'s failures, and its mark if it has one
    /// (`DeliveryFailureTracker#track_success!`, and `reset!`). With no
    /// failures to clear and no mark remembered, the table is not asked, so
    /// that a fan-out to thousands of servers that answer does not ask it
    /// thousands of times; Mastodon asks it every time.
    pub async fn track_success(&self, host: &str) -> anyhow::Result<()> {
        // A host is marked only once it has failures, and only a success or
        // a reset clears them.
        let cleared = self.store.clear_failures(host).await?;
        if cleared || self.store.is_unavailable(host) {
            self.store.mark_available(host).await?;
        }
        Ok(())
    }

    /// Clear `host`'s failures and its mark, asking the table whatever this
    /// process remembers (`track_success!` as `restart_delivery` calls it).
    pub async fn restart(&self, host: &str) -> anyhow::Result<()> {
        self.store().clear_failures(host).await?;
        self.store().mark_available(host).await?;
        self.store().forget();
        Ok(())
    }

    /// Forget `host`'s failures, leaving any mark
    /// (`DeliveryFailureTracker#clear_failures!`).
    pub async fn clear_failures(&self, host: &str) -> anyhow::Result<()> {
        self.store().clear_failures(host).await.map(drop)
    }

    /// The days deliveries to `host` failed on, oldest first
    /// (`#exhausted_deliveries_days`).
    pub async fn exhausted_deliveries_days(
        &self,
        host: &str,
    ) -> anyhow::Result<Vec<chrono::NaiveDate>> {
        let store = self.store();
        let mut redis = store.redis.clone();
        let days: Vec<String> = redis::cmd("SMEMBERS")
            .arg(store.key(host))
            .query_async(&mut redis)
            .await?;
        let mut days: Vec<chrono::NaiveDate> = days
            .iter()
            .filter_map(|d| chrono::NaiveDate::parse_from_str(d, "%Y%m%d").ok())
            .collect();
        days.sort();
        days.dedup();
        Ok(days)
    }

    /// How many days each of `domains` has failures on, those with any and
    /// not marked unavailable (`.warning_domains_map(domains)`); with `None`,
    /// every domain with failures (`.warning_domains_map`), found by scanning
    /// this instance's keys.
    pub async fn warning_domains_map(
        &self,
        domains: Option<&[String]>,
    ) -> anyhow::Result<std::collections::HashMap<String, usize>> {
        let store = self.store();
        let mut redis = store.redis.clone();
        let candidates: Vec<String> = match domains {
            Some(domains) => domains.to_vec(),
            None => {
                let prefix = store.key("");
                let mut found = vec![];
                let mut cursor: u64 = 0;
                loop {
                    let (next, keys): (u64, Vec<String>) = redis::cmd("SCAN")
                        .arg(cursor)
                        .arg("MATCH")
                        .arg(format!("{prefix}*"))
                        .arg("COUNT")
                        .arg(1000)
                        .query_async(&mut redis)
                        .await?;
                    found.extend(
                        keys.iter()
                            .filter_map(|k| k.strip_prefix(&prefix).map(str::to_owned)),
                    );
                    if next == 0 {
                        break;
                    }
                    cursor = next;
                }
                found
            }
        };
        let unavailable: HashSet<String> = sqlx::query_scalar!(
            "SELECT domain FROM unavailable_domains WHERE domain = ANY($1)",
            &candidates,
        )
        .fetch_all(&store.db)
        .await?
        .into_iter()
        .collect();
        let mut map = std::collections::HashMap::new();
        for domain in candidates {
            if unavailable.contains(&domain) {
                continue;
            }
            let days: usize = redis::cmd("SCARD")
                .arg(store.key(&domain))
                .query_async(&mut redis)
                .await?;
            if days > 0 {
                map.insert(domain, days);
            }
        }
        Ok(map)
    }

    /// `UnavailableDomain.create!(domain:)`: stop delivering to `host` now,
    /// returning the mark's id, or `None` when it was already marked.
    pub async fn stop(&self, host: &str) -> anyhow::Result<Option<i64>> {
        let id = sqlx::query_scalar!(
            r#"INSERT INTO unavailable_domains (domain, created_at, updated_at)
               VALUES ($1, now(), now())
               ON CONFLICT (domain) DO NOTHING
               RETURNING id"#,
            host,
        )
        .fetch_optional(&self.store().db)
        .await?;
        if id.is_some() {
            self.store().forget();
        }
        Ok(id)
    }

    /// Whether deliveries to `host` are no longer sent. From memory, read
    /// again in the background once it is a minute old.
    pub fn is_unavailable(&self, host: &str) -> bool {
        self.store().is_unavailable(host)
    }

    /// Whether deliveries to `inbox` are no longer sent: its host's.
    pub fn is_unavailable_inbox(&self, inbox: &url::Url) -> bool {
        ojak::origin::host_of(inbox.as_str()).is_some_and(|host| self.is_unavailable(&host))
    }

    /// What `attempt` says about its host, recorded in the background.
    pub fn record(&self, attempt: &DeliveryAttempt) {
        // `@unsalvageable`: neither a success nor a failure.
        let Some(succeeded) = succeeded(attempt) else {
            return;
        };
        let Some(host) = ojak::origin::host_of(attempt.inbox.as_str()) else {
            return;
        };
        let tracker = self.clone();
        let inbox = attempt.inbox.clone();
        crate::tenants::spawn(async move {
            let recorded = if succeeded {
                tracker.track_success(&host).await
            } else {
                tracker.track_failure(&host).await
            };
            if let Err(error) = recorded {
                tracing::warn!(%inbox, %error, "could not record a delivery's outcome");
            }
        });
    }
}

/// Whether `attempt` counts as reaching its host (`Some(true)`), as failing
/// to (`Some(false)`) — a failure that may pass, or one the circuit breaker
/// held — or as neither, an answer that will not change (`None`,
/// `@unsalvageable`).
fn succeeded(attempt: &DeliveryAttempt) -> Option<bool> {
    match attempt.outcome {
        AttemptOutcome::Delivered => Some(true),
        AttemptOutcome::Failed {
            permanent: false, ..
        }
        | AttemptOutcome::Held => Some(false),
        AttemptOutcome::Failed {
            permanent: true, ..
        } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(n: u32) -> chrono::DateTime<chrono::Utc> {
        use chrono::TimeZone as _;
        chrono::Utc.with_ymd_and_hms(2025, 1, n, 12, 0, 0).unwrap()
    }

    #[test]
    fn stamps_are_mastodons() {
        assert_eq!(Resolution::Days.stamp(day(2)), "20250102");
        assert_eq!(Resolution::Minutes.stamp(day(2)), "202501021200");
        assert_eq!(Resolution::Days.threshold(), 7);
        assert_eq!(Resolution::Minutes.threshold(), 5);
    }

    #[test]
    fn an_answer_that_will_not_change_counts_for_nothing() {
        let attempt = |outcome| DeliveryAttempt {
            inbox: url::Url::parse("https://a.example/inbox").unwrap(),
            sender: "alice".into(),
            outcome,
        };
        assert_eq!(succeeded(&attempt(AttemptOutcome::Delivered)), Some(true));
        assert_eq!(succeeded(&attempt(AttemptOutcome::Held)), Some(false));
        let failed = |permanent| AttemptOutcome::Failed {
            status: Some(500),
            permanent,
            error: String::new(),
        };
        assert_eq!(succeeded(&attempt(failed(false))), Some(false));
        assert_eq!(succeeded(&attempt(failed(true))), None);
    }
}
