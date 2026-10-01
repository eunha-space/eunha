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
//! such as a 404, is the server working, and counts for nothing.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use redis::aio::ConnectionManager;

/// Days with failures that mark a host unavailable
/// (`DeliveryFailureTracker::FAILURE_THRESHOLDS[:days]`).
const FAILURE_DAYS: usize = 7;

/// How long the hosts marked unavailable are taken from memory before the
/// table is read again, for a host another process marked.
const UNAVAILABLE_TTL: Duration = Duration::from_secs(60);

/// An instance's tracker.
#[derive(Clone)]
pub struct DeliveryFailureTracker {
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

/// The host deliveries to `url` are tracked by, as Mastodon normalises it:
/// lower case, an internationalised name in its ASCII form, which is how
/// `url` keeps a host.
pub fn host(url: &url::Url) -> Option<String> {
    url.host_str().map(str::to_ascii_lowercase)
}

impl DeliveryFailureTracker {
    pub fn new(
        db: sqlx::PgPool,
        redis: ConnectionManager,
        keys: crate::redis_keys::RedisKeyspace,
    ) -> Self {
        Self {
            db,
            redis,
            keys,
            unavailable: Arc::new(Unavailable {
                hosts: RwLock::new((None, Arc::default())),
                reading: AtomicBool::new(false),
            }),
        }
    }

    fn key(&self, host: &str) -> String {
        self.keys.key(format!("exhausted_deliveries:{host}"))
    }

    /// Count a failed attempt at a delivery to `host`
    /// (`DeliveryFailureTracker#track_failure!`).
    pub async fn track_failure(&self, host: &str) -> anyhow::Result<()> {
        let day = chrono::Utc::now().format("%Y%m%d").to_string();
        let mut redis = self.redis.clone();
        let (_, days): (i64, usize) = redis::pipe()
            .cmd("SADD")
            .arg(self.key(host))
            .arg(day)
            .cmd("SCARD")
            .arg(self.key(host))
            .query_async(&mut redis)
            .await?;
        if days >= FAILURE_DAYS {
            // `UnavailableDomain.create`, which a domain already there fails
            // validation for and leaves as it is.
            let marked = sqlx::query!(
                r#"INSERT INTO unavailable_domains (domain, created_at, updated_at)
                   VALUES ($1, now(), now())
                   ON CONFLICT (domain) DO NOTHING"#,
                host,
            )
            .execute(&self.db)
            .await?;
            if marked.rows_affected() > 0 {
                tracing::info!(host, days, "marked domain unavailable");
                self.forget();
            }
        }
        Ok(())
    }

    /// Clear `host`'s failures, and its mark if it has one
    /// (`DeliveryFailureTracker#track_success!`, and `reset!`).
    pub async fn track_success(&self, host: &str) -> anyhow::Result<()> {
        let mut redis = self.redis.clone();
        let cleared: i64 = redis::cmd("DEL")
            .arg(self.key(host))
            .query_async(&mut redis)
            .await?;
        // A host is marked only once it has failures, and only a success or
        // `reset` clears them; with none to clear there is no mark either,
        // and a fan-out to thousands of servers that answer does not ask the
        // table thousands of times. Mastodon asks it every time.
        if cleared > 0 || self.is_unavailable(host) {
            let unmarked = sqlx::query!("DELETE FROM unavailable_domains WHERE domain = $1", host)
                .execute(&self.db)
                .await?;
            if unmarked.rows_affected() > 0 {
                tracing::info!(host, "domain available again");
                self.forget();
            }
        }
        Ok(())
    }

    /// Whether deliveries to `host` are no longer sent. From memory, read
    /// again in the background once it is a minute old.
    pub fn is_unavailable(&self, host: &str) -> bool {
        let (read, hosts) = self
            .unavailable
            .hosts
            .read()
            .expect("unavailable hosts")
            .clone();
        if read.is_none_or(|read| read.elapsed() >= UNAVAILABLE_TTL)
            && !self.unavailable.reading.swap(true, Ordering::AcqRel)
        {
            let tracker = self.clone();
            crate::tenants::spawn(async move {
                let hosts = sqlx::query_scalar!("SELECT domain FROM unavailable_domains")
                    .fetch_all(&tracker.db)
                    .await;
                match hosts {
                    Ok(hosts) => {
                        *tracker
                            .unavailable
                            .hosts
                            .write()
                            .expect("unavailable hosts") =
                            (Some(Instant::now()), Arc::new(hosts.into_iter().collect()));
                    }
                    Err(error) => tracing::warn!(%error, "could not read unavailable domains"),
                }
                tracker.unavailable.reading.store(false, Ordering::Release);
            });
        }
        hosts.contains(host)
    }

    /// Read the table again at the next question, after a change here.
    fn forget(&self) {
        self.unavailable.hosts.write().expect("unavailable hosts").0 = None;
    }

    /// What `attempt` says about its host, recorded in the background.
    pub fn record(&self, attempt: &ojak::deliverer::DeliveryAttempt) {
        use ojak::deliverer::AttemptOutcome;

        let Some(host) = host(&attempt.inbox) else {
            return;
        };
        let succeeded = match attempt.outcome {
            AttemptOutcome::Delivered => true,
            AttemptOutcome::Failed {
                permanent: false, ..
            }
            | AttemptOutcome::Held => false,
            // `@unsalvageable`: neither a success nor a failure.
            AttemptOutcome::Failed {
                permanent: true, ..
            } => return,
        };
        let tracker = self.clone();
        crate::tenants::spawn(async move {
            let recorded = if succeeded {
                tracker.track_success(&host).await
            } else {
                tracker.track_failure(&host).await
            };
            if let Err(error) = recorded {
                tracing::warn!(host, %error, "could not record a delivery's outcome");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::host;

    #[test]
    fn a_host_is_tracked_in_its_ascii_form() {
        let url = url::Url::parse("https://Bücher.Example/inbox").unwrap();
        assert_eq!(host(&url).as_deref(), Some("xn--bcher-kva.example"));
    }
}
