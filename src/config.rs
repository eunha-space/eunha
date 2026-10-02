use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub database_url: String,
    /// Where this instance's pool connects, when a connection pooler sits in
    /// front of `database_url` rather than this being PostgreSQL itself.
    ///
    /// Only the request and queue pool moves. `eunha migrate` keeps using
    /// `database_url`, because sqlx takes a session-scoped advisory lock around
    /// a migration run and a transaction pooler would hand the unlock to a
    /// different server connection than the lock.
    #[serde(default)]
    pub pooled_database_url: Option<String>,
    /// Client connections the pooler accepts in total, as its own configuration
    /// sets them. Startup checks the tenants' pools against this instead of
    /// asking PostgreSQL for `max_connections`: through a pooler that answer
    /// describes the wrong limit, and is far smaller than what the pooler holds.
    #[serde(default)]
    pub pooled_client_slots: Option<u64>,
    #[serde(default)]
    pub database_pool: DatabasePoolConfig,
    pub redis_url: String,
    /// Optional non-evicting Redis endpoint for locks, tombstones,
    /// idempotency and notification grouping. When absent, these use
    /// `redis_url`, preserving the single-Redis standalone deployment.
    #[serde(default)]
    pub redis_coordination_url: Option<String>,
    /// Prefix applied to every Redis key owned by this Eunha instance.
    ///
    /// Leave empty for a dedicated Redis deployment. Pooled deployments set a
    /// unique value and restrict the Redis user to `<prefix>:*` with ACLs.
    #[serde(default)]
    pub redis_key_prefix: String,
    /// Whether tenant-facing admin endpoints may report process-wide Redis
    /// memory. This is safe for a dedicated Redis process, but leaks aggregate
    /// pool usage when Redis is shared by several instances.
    #[serde(default = "default_redis_process_metrics")]
    pub redis_process_metrics: bool,
    pub bind_address: String,
    pub media_storage: MediaStorageConfig,
    pub smtp: Option<SmtpConfig>,
    pub instance: InstanceConfig,
    /// Mastodon's ActiveRecord encryption keys, needed to read or write the
    /// encrypted `keypairs.private_key` column a Mastodon 4.7 database uses.
    /// Absent on instances whose keys still live in `accounts`.
    #[serde(default)]
    pub active_record_encryption: Option<ActiveRecordEncryptionConfig>,
    /// Where to ask about newer Mastodon releases and the end of support of the
    /// one eunha implements. Unset — the default — asks nobody anything.
    ///
    /// Nothing in eunha reads the answer back: it is recorded for a Mastodon
    /// that may later boot on the database, and mailed to this instance's own
    /// administrators. A hosted instance's administrators cannot act on it,
    /// since only whoever runs the binary can take a release up, so the
    /// request is made when an operator asks for it rather than by default.
    /// Mastodon's own server is `https://api.joinmastodon.org/update-check`.
    #[serde(default)]
    pub software_update_url: Option<String>,

    /// Private networks this instance may nonetheless reach, as CIDR blocks.
    ///
    /// Federation refuses private addresses by default, because a peer that can
    /// name an address can otherwise make this server probe its own network.
    /// An instance that legitimately federates inside one — split-horizon DNS, a
    /// proxy on a LAN, a mesh network — names those ranges here and no others.
    /// Mastodon's `ALLOWED_PRIVATE_ADDRESSES` is the same setting.
    #[serde(default)]
    pub allowed_private_networks: Vec<String>,
    /// Attach FEP-8b32 integrity proofs to outgoing activities, so that a
    /// relayed or forwarded copy can still be attributed.
    ///
    /// Off by default: Mastodon verifies these but does not produce them, and
    /// what eunha sends should look like what Mastodon sends unless an
    /// administrator decides otherwise. Turning it on is additive — the HTTP
    /// Signature is unchanged, and a peer that ignores the proof is unaffected.
    #[serde(default = "default_sign_integrity_proofs")]
    pub sign_integrity_proofs: bool,
    #[serde(default)]
    pub workers: WorkersConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
}

/// Mastodon's `ACTIVE_RECORD_ENCRYPTION_*` secrets. Both are required together;
/// the deterministic key is not used, because the only encrypted column in
/// Mastodon's schema (`keypairs.private_key`) is not deterministic.
#[derive(Debug, Clone, Deserialize)]
pub struct ActiveRecordEncryptionConfig {
    pub primary_key: String,
    pub key_derivation_salt: String,
}

/// Per-process connection budget. Idle instances need not retain connections.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DatabasePoolConfig {
    pub max_connections: u32,
    pub min_connections: u32,
    pub acquire_timeout_seconds: u64,
    pub idle_timeout_seconds: u64,
}

impl Default for DatabasePoolConfig {
    fn default() -> Self {
        Self {
            max_connections: 20,
            min_connections: 0,
            acquire_timeout_seconds: 30,
            idle_timeout_seconds: 600,
        }
    }
}

impl DatabasePoolConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.max_connections > 0,
            "database_pool.max_connections must be positive"
        );
        anyhow::ensure!(
            self.min_connections <= self.max_connections,
            "database_pool.min_connections must not exceed max_connections"
        );
        anyhow::ensure!(
            self.acquire_timeout_seconds > 0,
            "database_pool.acquire_timeout_seconds must be positive"
        );
        anyhow::ensure!(
            self.idle_timeout_seconds > 0,
            "database_pool.idle_timeout_seconds must be positive"
        );
        Ok(())
    }
}

#[cfg(test)]
mod pool_tests {
    use super::DatabasePoolConfig;

    #[test]
    fn partial_pool_config_preserves_defaults() {
        let pool: DatabasePoolConfig = toml::from_str("max_connections = 5").unwrap();
        pool.validate().unwrap();
        assert_eq!(pool.max_connections, 5);
        assert_eq!(pool.min_connections, 0);
        assert_eq!(pool.acquire_timeout_seconds, 30);
        assert_eq!(pool.idle_timeout_seconds, 600);
        let legacy: DatabasePoolConfig = toml::from_str("").unwrap();
        assert_eq!(legacy.max_connections, 20);
    }

    #[test]
    fn invalid_pool_budgets_are_rejected() {
        for input in [
            "max_connections = 0",
            "max_connections = 2\nmin_connections = 3",
            "acquire_timeout_seconds = 0",
            "idle_timeout_seconds = 0",
        ] {
            let pool: DatabasePoolConfig = toml::from_str(input).unwrap();
            assert!(pool.validate().is_err(), "accepted {input}");
        }
    }
}

/// Sizing for the durable background queues. Every field has a default, so an
/// existing `config.toml` needs no `[workers]` section; tune these when one
/// process can no longer keep up with the queue depth.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkersConfig {
    /// Number of concurrent ActivityPub delivery queue loops. Each claims its
    /// own batch via `FOR UPDATE SKIP LOCKED`, so raising this is safe both
    /// within a process and across processes.
    #[serde(default = "default_delivery_workers")]
    pub delivery_workers: usize,
    /// The most jobs one claim takes. A delivery loop claims as its slots
    /// free up, a quarter of them at a time, up to this many.
    #[serde(default = "default_delivery_batch")]
    pub delivery_batch: i64,
    /// In-flight inbox POSTs per delivery loop. Total delivery concurrency is
    /// `delivery_workers * delivery_concurrency`, and the process's
    /// `process_delivery_concurrency` caps it across instances.
    ///
    /// A delivery mostly waits on the network, and remote servers are slow or
    /// silent often enough to decide the rate: with one in ten taking seconds
    /// and one in a hundred never answering, a slot averages over half a
    /// second a delivery. Sixteen slots took seven minutes to send one post to
    /// 9,258 servers; 128 took 48 seconds, at under a third of a core, with
    /// the instance's own requests unaffected (docs/design/benchmarking.md,
    /// “Fan-out”).
    #[serde(default = "default_delivery_concurrency")]
    pub delivery_concurrency: usize,
    /// Number of concurrent inbound (ingress) queue loops.
    #[serde(default = "default_inbox_workers")]
    pub inbox_workers: usize,
    /// Activities claimed per batch by a single ingress loop.
    #[serde(default = "default_inbox_batch")]
    pub inbox_batch: i64,
    /// Activities processed concurrently per ingress loop.
    #[serde(default = "default_inbox_concurrency")]
    pub inbox_concurrency: usize,
    /// The longest an idle queue loop waits before looking for work again, in
    /// seconds. A job this process enqueues wakes its loop at once regardless;
    /// the poll only finds retries that have come due and jobs another process
    /// enqueued, so this bounds how late those can start. A host of mostly idle
    /// tenants raises it, with `database_pool.idle_timeout_seconds` below it, so
    /// that an idle tenant holds no database connection at all.
    ///
    /// The timed tasks — scheduled statuses, poll expiry and suspended account
    /// cleanup — sleep until their next item is due and, when nothing is, for
    /// this long or a minute, whichever is longer.
    #[serde(default = "default_queue_idle_poll_seconds")]
    pub queue_idle_poll_seconds: u64,
    /// Inbox POSTs in flight across every instance this process serves. Each
    /// delivery loop still claims up to `delivery_concurrency` jobs, but only
    /// this many of all of them are sending at once, first come first served,
    /// so an instance with a large fan-out waits its turn rather than opening
    /// thousands of connections. Instances sharing a process must all name the
    /// same value.
    #[serde(default = "default_process_delivery_concurrency")]
    pub process_delivery_concurrency: usize,
}

/// Whether integrity proofs are signed when a config says nothing about it.
///
/// Public so a test can assert the default rather than restate it.
pub fn default_sign_integrity_proofs() -> bool {
    false
}

fn default_redis_process_metrics() -> bool {
    true
}

#[cfg(test)]
mod redis_tests {
    #[derive(serde::Deserialize)]
    struct RedisDefaults {
        #[serde(default)]
        redis_key_prefix: String,
        #[serde(default = "super::default_redis_process_metrics")]
        redis_process_metrics: bool,
        #[serde(default)]
        redis_coordination_url: Option<String>,
    }

    #[test]
    fn dedicated_redis_defaults_preserve_existing_behavior() {
        let config: RedisDefaults = toml::from_str("").unwrap();
        assert_eq!(config.redis_key_prefix, "");
        assert!(config.redis_process_metrics);
        assert!(config.redis_coordination_url.is_none());
    }
}

fn default_delivery_workers() -> usize {
    1
}

fn default_delivery_batch() -> i64 {
    50
}

fn default_delivery_concurrency() -> usize {
    128
}

fn default_inbox_workers() -> usize {
    1
}

fn default_inbox_batch() -> i64 {
    20
}

fn default_inbox_concurrency() -> usize {
    4
}

fn default_queue_idle_poll_seconds() -> u64 {
    30
}

fn default_process_delivery_concurrency() -> usize {
    256
}

impl Default for WorkersConfig {
    fn default() -> Self {
        Self {
            delivery_workers: default_delivery_workers(),
            delivery_batch: default_delivery_batch(),
            delivery_concurrency: default_delivery_concurrency(),
            inbox_workers: default_inbox_workers(),
            inbox_batch: default_inbox_batch(),
            inbox_concurrency: default_inbox_concurrency(),
            queue_idle_poll_seconds: default_queue_idle_poll_seconds(),
            process_delivery_concurrency: default_process_delivery_concurrency(),
        }
    }
}

impl WorkersConfig {
    /// Clamp every field to at least 1 so a zero in config can't silently stop
    /// a queue from draining.
    pub fn sanitized(&self) -> Self {
        Self {
            delivery_workers: self.delivery_workers.max(1),
            delivery_batch: self.delivery_batch.max(1),
            delivery_concurrency: self.delivery_concurrency.max(1),
            inbox_workers: self.inbox_workers.max(1),
            inbox_batch: self.inbox_batch.max(1),
            inbox_concurrency: self.inbox_concurrency.max(1),
            queue_idle_poll_seconds: self.queue_idle_poll_seconds.max(1),
            process_delivery_concurrency: self.process_delivery_concurrency.max(1),
        }
    }

    /// The longest an idle queue loop sleeps between looks at its table.
    pub fn queue_idle_poll(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.queue_idle_poll_seconds)
    }

    /// The longest a timed task sleeps when nothing is due. Never shorter than
    /// the minute those tasks used to run on, so that a short queue poll does
    /// not make them busier than they were.
    pub fn timed_task_idle_poll(&self) -> std::time::Duration {
        self.queue_idle_poll()
            .max(std::time::Duration::from_secs(60))
    }
}

/// How many requests an instance sharing a process with others may have in
/// flight when its configuration does not say.
pub const DEFAULT_SHARED_MAX_CONCURRENT_REQUESTS: usize = 64;

/// How many instances one process serves when its configuration does not say.
pub const DEFAULT_PROCESS_MAX_TENANTS: usize = 50;

/// Limits an instance is held to so that it cannot take more than its share
/// of a process it shares with other instances, and limits on what the process
/// takes on at all. Every field has a default, so an existing `config.toml`
/// needs no `[limits]` section.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct LimitsConfig {
    /// Requests this instance may have in flight at once. Past it, a request
    /// is answered at once with 503 and `Retry-After` rather than queued, so an
    /// instance being flooded cannot slow the others down. Unset, a lone
    /// instance has no limit and one among several has
    /// [`DEFAULT_SHARED_MAX_CONCURRENT_REQUESTS`].
    #[serde(default)]
    pub max_concurrent_requests: Option<usize>,
    /// Instances one process may serve. A panic, a leak or a saturated runtime
    /// takes down every instance in the process, so this is how many one
    /// failure may reach. A directory with more refuses to start. Unset, it is
    /// [`DEFAULT_PROCESS_MAX_TENANTS`]. Instances sharing a process must all
    /// name the same value.
    #[serde(default)]
    pub process_max_tenants: Option<usize>,
    /// Database connections every instance's pool in this process may open
    /// between them, for when several processes share one PostgreSQL server
    /// and each is given a part of it. A directory whose
    /// `database_pool.max_connections` add up to more refuses to start. Unset,
    /// the only budget is what the server itself accepts. Instances sharing a
    /// process must all name the same value.
    #[serde(default)]
    pub process_database_connections: Option<u64>,
}

impl LimitsConfig {
    /// The in-flight request limit, given whether this instance shares its
    /// process with others.
    pub fn request_limit(&self, shared: bool) -> Option<usize> {
        self.max_concurrent_requests
            .or(shared.then_some(DEFAULT_SHARED_MAX_CONCURRENT_REQUESTS))
            .map(|limit| limit.max(1))
    }

    /// How many instances the process may serve.
    pub fn max_tenants(&self) -> usize {
        self.process_max_tenants
            .unwrap_or(DEFAULT_PROCESS_MAX_TENANTS)
            .max(1)
    }
}

/// Single-tenant instance settings (formerly stored in the `instances` DB table).
#[derive(Debug, Clone, Deserialize)]
pub struct InstanceConfig {
    pub domain: String,
    /// Additional HTTP hostnames that serve this instance without changing
    /// its canonical ActivityPub identity or emitted URLs.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Domains this instance's accounts had before `domain`, whose actors
    /// they were. Each actor lists its id under each of them in
    /// `alsoKnownAs`, which is what a follower's server checks before it
    /// honours `eunha accounts move` and follows the account to `domain`.
    /// Nothing is served on them.
    #[serde(default)]
    pub previous_domains: Vec<String>,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub short_description: String,
    pub contact_email: Option<String>,
    #[serde(default = "default_true")]
    pub registrations_open: bool,
    #[serde(default)]
    pub approval_required: bool,
    pub vapid_private_key: String,
    pub vapid_public_key: String,
    pub icon_url: Option<String>,
    /// Served as the privacy policy while the `site_terms` setting is blank
    /// (docs/operating/terms-of-service.md).
    #[serde(default)]
    pub privacy_policy: String,
    /// Served as terms of service effective on 2025-01-01 until a version is
    /// published, and never read after (docs/operating/terms-of-service.md).
    #[serde(default)]
    pub terms_of_service: String,
    /// Whether this instance offers email subscriptions at all. Mastodon's
    /// `DISABLE_EMAIL_SUBSCRIPTIONS=true` is this set to `false`; with it on,
    /// as by default, an administrator still has to enable the feature.
    #[serde(default = "default_true")]
    pub email_subscriptions: bool,
    /// Mastodon's `AUTHORIZED_FETCH`: when set, whether ActivityPub fetches
    /// must be signed, whatever the `authorized_fetch` site setting says.
    /// Unset, the setting decides. Limited federation mode turns it on
    /// regardless.
    #[serde(default)]
    pub authorized_fetch: Option<bool>,
    /// Mastodon's `LIMITED_FEDERATION_MODE`: federate only with the domains on
    /// the allow list, refuse the API to anyone not signed in, and hide the
    /// peers and activity APIs.
    #[serde(default)]
    pub limited_federation_mode: bool,
    /// Mastodon's `DISALLOW_UNAUTHENTICATED_API_ACCESS`: refuse the API to
    /// anyone not signed in, as limited federation mode does, without
    /// limiting federation.
    #[serde(default)]
    pub disallow_unauthenticated_api_access: bool,
}

impl InstanceConfig {
    /// `Api::BaseController#disallow_unauthenticated_api_access?`.
    pub fn disallows_unauthenticated_api_access(&self) -> bool {
        self.disallow_unauthenticated_api_access || self.limited_federation_mode
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct MediaStorageConfig {
    pub bucket: String,
    /// Namespace prepended to every object key in shared buckets. Empty keeps
    /// the historical one-bucket-per-instance layout.
    #[serde(default)]
    pub key_prefix: String,
    pub region: String,
    pub endpoint: Option<String>,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub base_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        dotenvy::dotenv().ok();
        adopt_mastodon_encryption_env();
        let cfg = config::Config::builder()
            .add_source(config::File::with_name("config").required(false))
            .add_source(config::Environment::default().separator("__"))
            .build()?;
        Ok(cfg.try_deserialize()?)
    }

    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let cfg = config::Config::builder()
            .add_source(config::File::from(std::path::Path::new(path)))
            .build()?;
        Ok(cfg.try_deserialize()?)
    }
}

/// Accept Mastodon's own spelling of the encryption secrets.
///
/// Eunha's environment keys nest with `__`, so its name for the primary key is
/// `ACTIVE_RECORD_ENCRYPTION__PRIMARY_KEY` — but the values themselves come
/// from a Mastodon installation, whose `.env.production` spells them with a
/// single underscore. Copying that file across should be enough.
fn adopt_mastodon_encryption_env() {
    for (mastodon, eunha) in [
        (
            "ACTIVE_RECORD_ENCRYPTION_PRIMARY_KEY",
            "ACTIVE_RECORD_ENCRYPTION__PRIMARY_KEY",
        ),
        (
            "ACTIVE_RECORD_ENCRYPTION_KEY_DERIVATION_SALT",
            "ACTIVE_RECORD_ENCRYPTION__KEY_DERIVATION_SALT",
        ),
    ] {
        if std::env::var_os(eunha).is_none() {
            if let Some(value) = std::env::var_os(mastodon) {
                // Safety: called once, before any threads read the environment.
                unsafe { std::env::set_var(eunha, value) };
            }
        }
    }
}
