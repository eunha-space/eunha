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
    /// The answer is shown on the admin software updates page, recorded for a
    /// Mastodon that may later boot on the database, and mailed to this instance's own
    /// administrators. A hosted instance's administrators cannot act on it,
    /// since only whoever runs the binary can take a release up, so the
    /// request is made when an operator asks for it rather than by default.
    /// Mastodon's own server is `https://api.joinmastodon.org/update-check`.
    #[serde(default)]
    pub software_update_url: Option<String>,

    /// Private networks this instance may nonetheless reach, as CIDR blocks or
    /// single addresses.
    ///
    /// Federation refuses private addresses by default — those Mastodon's
    /// `PrivateAddressCheck` refuses, NAT64 and 6to4 included — because a peer
    /// that can name an address can otherwise make this server probe its own
    /// network. An instance that legitimately federates inside one —
    /// split-horizon DNS, a proxy on a LAN, a mesh network, NAT64 — names those
    /// ranges here and no others. Mastodon's `ALLOWED_PRIVATE_ADDRESSES` is the
    /// same setting. Each instance in a process has its own.
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
    /// The longest an idle queue loop waits before looking for work again, in
    /// seconds. A job this process enqueues wakes its loop at once regardless;
    /// the poll only finds retries that have come due and jobs another process
    /// enqueued, so this bounds how late those can start. A host of mostly idle
    /// tenants raises it, with `database_pool.idle_timeout_seconds` below it, so
    /// that an idle tenant holds no database connection at all.
    ///
    /// The timed tasks — scheduled statuses and suspended account cleanup —
    /// sleep until their next item is due and, when nothing is, for
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
    /// Number of job queue loops (docs/operating/jobs.md). Each claims its
    /// own jobs with `FOR UPDATE SKIP LOCKED`, so raising this is safe both
    /// within a process and across processes.
    #[serde(default = "default_job_workers")]
    pub job_workers: usize,
    /// Jobs each job queue loop runs at once: Sidekiq's `concurrency`.
    #[serde(default = "default_job_concurrency")]
    pub job_concurrency: usize,
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

fn default_queue_idle_poll_seconds() -> u64 {
    30
}

fn default_process_delivery_concurrency() -> usize {
    256
}

fn default_job_workers() -> usize {
    1
}

/// Sidekiq's default `concurrency`, which Mastodon's `sidekiq.yml` keeps
/// unless `SIDEKIQ_CONCURRENCY` says otherwise.
fn default_job_concurrency() -> usize {
    5
}

impl Default for WorkersConfig {
    fn default() -> Self {
        Self {
            delivery_workers: default_delivery_workers(),
            delivery_batch: default_delivery_batch(),
            delivery_concurrency: default_delivery_concurrency(),
            queue_idle_poll_seconds: default_queue_idle_poll_seconds(),
            process_delivery_concurrency: default_process_delivery_concurrency(),
            job_workers: default_job_workers(),
            job_concurrency: default_job_concurrency(),
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
            queue_idle_poll_seconds: self.queue_idle_poll_seconds.max(1),
            process_delivery_concurrency: self.process_delivery_concurrency.max(1),
            job_workers: self.job_workers.max(1),
            job_concurrency: self.job_concurrency.max(1),
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
    /// Whether Mastodon's rate limits apply to this instance: the
    /// `Rack::Attack` throttles and the `RateLimiter` families. Unset, they
    /// do, as they always do in Mastodon.
    #[serde(default)]
    pub rate_limits: Option<bool>,
}

impl LimitsConfig {
    /// Whether the rate limits apply.
    pub fn rate_limits(&self) -> bool {
        self.rate_limits.unwrap_or(true)
    }

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
    /// The site's identity and registrations, as eunha kept them before it
    /// read Mastodon's settings: `site_title`, `site_short_description` (or
    /// `site_extended_description`, when `short_description` is blank),
    /// `site_extended_description`, `site_contact_email` and
    /// `registrations_mode`. Nothing reads them but
    /// `eunha settings import-config`, which copies them into the settings
    /// once (docs/operating/instances.md); a server that finds them set warns.
    #[serde(default)]
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
    /// The icon eunha showed for an instance before it served the uploaded
    /// app icon and thumbnail, or the web frontend's; not read any more.
    pub icon_url: Option<String>,
    /// The privacy policy eunha served before it read `site_terms`, which
    /// `eunha settings import-config` copies there; not read otherwise.
    #[serde(default)]
    pub privacy_policy: String,
    /// The terms of service eunha served before it read
    /// `terms_of_services`, which `eunha settings import-config` publishes as
    /// a version effective on 2025-01-01; not read otherwise.
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
    /// Mastodon's `DEFAULT_LOCALE`: the locale the instance speaks when nobody
    /// has said which they want (`I18n.default_locale`). One of Mastodon's
    /// available locales, or else `en`.
    #[serde(default)]
    pub default_locale: Option<String>,
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
    /// Mastodon's `DISABLE_AUTOMATIC_SWITCHING_TO_APPROVED_REGISTRATIONS`:
    /// keep open registrations open when no moderator has been active for a
    /// week, rather than switching them to approval
    /// (`Scheduler::AutoCloseRegistrationsScheduler`).
    #[serde(default)]
    pub disable_automatic_switching_to_approved_registrations: bool,
    /// Mastodon's `DISABLE_FOLLOWERS_SYNCHRONIZATION=true`: send no
    /// `Collection-Synchronization` header with a followers-only post, and
    /// act on none that arrives (docs/mastodon/serving.md).
    #[serde(default)]
    pub disable_followers_synchronization: bool,
    /// Mastodon's `SECRET_KEY_BASE`. With it, async refresh ids, the signed
    /// GlobalIDs in unsubscribe links and password reset digests are made and
    /// read exactly as that Mastodon makes them, so what it handed out keeps
    /// working. Unset, eunha keeps schemes of its own (see
    /// [`crate::secret_key_base`]).
    #[serde(
        default,
        deserialize_with = "crate::secret_key_base::deserialize_optional"
    )]
    pub secret_key_base: Option<crate::secret_key_base::SecretKeyBase>,
    /// Mastodon's `SELF_DESTRUCT`: the value `eunha self-destruct` prints,
    /// which signs this instance's domain. While it verifies, the instance
    /// refuses nearly every request and tells every server it knows that its
    /// accounts are gone (docs/operating/self-destruct.md); any other value
    /// changes nothing.
    #[serde(default)]
    pub self_destruct: Option<String>,
    /// Mastodon's `DEEPL_*` and `LIBRE_TRANSLATE_*`: the machine translation
    /// service, if any (`[instance.translation]`).
    #[serde(default)]
    pub translation: TranslationConfig,
    /// Mastodon's `EXPERIMENTAL_FEATURES`: the experimental features this
    /// instance turns on, by name. The one eunha knows is `fasp`, Fediverse
    /// Auxiliary Service Providers (docs/operating/fasp.md); a name it does
    /// not know is ignored, as Mastodon ignores one.
    #[serde(default)]
    pub experimental_features: Vec<String>,
    /// Mastodon's `ES_*` settings: full-text search with Elasticsearch or
    /// OpenSearch (docs/operating/search.md). Off unless `enabled` is set.
    #[serde(default)]
    pub elasticsearch: ElasticsearchConfig,
    /// Mastodon's `DONATION_CAMPAIGNS_URL` and `DONATION_CAMPAIGNS_ENVIRONMENT`
    /// (`[instance.donation_campaigns]`): where `GET /api/v1/donation_campaigns`
    /// asks for the campaign to show. Unset, there is none.
    #[serde(default)]
    pub donation_campaigns: DonationCampaignsConfig,
}

/// `Rails.configuration.x.donation_campaigns`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DonationCampaignsConfig {
    /// `DONATION_CAMPAIGNS_URL`: the campaign API.
    #[serde(default)]
    pub api_url: Option<String>,
    /// `DONATION_CAMPAIGNS_ENVIRONMENT`: sent along as `environment`.
    #[serde(default)]
    pub environment: Option<String>,
}

/// Mastodon's `config/translation.yml`. DeepL wins when both are set, as
/// `TranslationService.configured` picks it first.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TranslationConfig {
    /// `DEEPL_API_KEY`.
    #[serde(default)]
    pub deepl_api_key: Option<String>,
    /// `DEEPL_PLAN`: `free` (the default) asks `api-free.deepl.com`, anything
    /// else `api.deepl.com`.
    #[serde(default)]
    pub deepl_plan: Option<String>,
    /// Where DeepL's API is, in place of the host the plan picks: for a proxy
    /// in front of it. Mastodon has no such setting.
    #[serde(default)]
    pub deepl_endpoint: Option<String>,
    /// `LIBRE_TRANSLATE_ENDPOINT`.
    #[serde(default)]
    pub libre_translate_endpoint: Option<String>,
    /// `LIBRE_TRANSLATE_API_KEY`.
    #[serde(default)]
    pub libre_translate_api_key: Option<String>,
}

/// Mastodon's `ES_ENABLED`, `ES_HOST`, `ES_PORT`, `ES_USER`, `ES_PASS`,
/// `ES_PREFIX`, `ES_PRESET`, `ES_CA_FILE` and `ES_QUERY_TIMEOUT`, under
/// `[instance.elasticsearch]`. The `ES_*` environment variables themselves
/// are read too, for a single instance configured from the environment.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ElasticsearchConfig {
    /// `ES_ENABLED=true`.
    pub enabled: bool,
    /// `ES_HOST`: a host name, or a URL with its scheme (`https://...`).
    pub host: String,
    /// `ES_PORT`.
    pub port: u16,
    /// `ES_USER`.
    pub user: Option<String>,
    /// `ES_PASS`.
    pub pass: Option<String>,
    /// `ES_PREFIX`: put before every index name, joined with `_`. Instances
    /// sharing one cluster each need their own.
    pub prefix: Option<String>,
    /// `ES_PRESET`: `single_node_cluster` (the default), `small_cluster` or
    /// `large_cluster`, which decide replicas and shards.
    pub preset: Option<String>,
    /// `ES_CA_FILE`: a PEM certificate to trust for the cluster's TLS.
    pub ca_file: Option<String>,
    /// `ES_QUERY_TIMEOUT`, as Elasticsearch reads a time unit (`10s`).
    pub query_timeout: String,
}

impl Default for ElasticsearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "localhost".into(),
            port: 9200,
            user: None,
            pass: None,
            prefix: None,
            preset: None,
            ca_file: None,
            query_timeout: "10s".into(),
        }
    }
}

impl ElasticsearchConfig {
    /// `"#{host}:#{port}"`, with `http://` when the host names no scheme.
    pub fn base_url(&self) -> String {
        if self.host.contains("://") {
            format!("{}:{}", self.host.trim_end_matches('/'), self.port)
        } else {
            format!("http://{}:{}", self.host, self.port)
        }
    }
}

impl InstanceConfig {
    /// The keys `eunha settings import-config` reads that this configuration
    /// sets, which nothing else reads any more.
    pub fn deprecated_site_keys(&self) -> Vec<&'static str> {
        let mut keys = Vec::new();
        let text = [
            ("title", &self.title),
            ("description", &self.description),
            ("short_description", &self.short_description),
            ("privacy_policy", &self.privacy_policy),
            ("terms_of_service", &self.terms_of_service),
        ];
        for (key, value) in text {
            if !value.trim().is_empty() {
                keys.push(key);
            }
        }
        if self
            .contact_email
            .as_deref()
            .is_some_and(|e| !e.trim().is_empty())
        {
            keys.push("contact_email");
        }
        if !self.registrations_open {
            keys.push("registrations_open");
        }
        if self.approval_required {
            keys.push("approval_required");
        }
        keys
    }

    /// `I18n.default_locale`: `default_locale` when it is one of
    /// `I18n.available_locales`, else `en`.
    pub fn default_locale(&self) -> &'static str {
        self.default_locale
            .as_deref()
            .and_then(crate::languages::available_locale)
            .unwrap_or("en")
    }

    /// `Mastodon::Feature.<name>_enabled?`.
    pub fn feature_enabled(&self, name: &str) -> bool {
        self.experimental_features.iter().any(|f| f.trim() == name)
    }

    /// `Mastodon::Feature.fasp_enabled?`.
    pub fn fasp_enabled(&self) -> bool {
        self.feature_enabled("fasp")
    }

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
        adopt_mastodon_env();
        adopt_mastodon_elasticsearch_env();
        let cfg = config::Config::builder()
            .add_source(config::File::with_name("config").required(false))
            .add_source(config::Environment::default().separator("__"))
            .build()?;
        Ok(cfg.try_deserialize()?)
    }

    /// Only the `[instance]` section of what [`Config::from_env`] reads, for a
    /// command that needs the instance's own settings and nothing else, such
    /// as `eunha migrate` importing the site settings.
    pub fn instance_from_env() -> anyhow::Result<InstanceConfig> {
        dotenvy::dotenv().ok();
        adopt_mastodon_env();
        adopt_mastodon_elasticsearch_env();
        let cfg = config::Config::builder()
            .add_source(config::File::with_name("config").required(false))
            .add_source(config::Environment::default().separator("__"))
            .build()?;
        Ok(cfg.get("instance")?)
    }

    pub fn from_file(path: &str) -> anyhow::Result<Self> {
        let cfg = config::Config::builder()
            .add_source(config::File::from(std::path::Path::new(path)))
            .build()?;
        Ok(cfg.try_deserialize()?)
    }
}

/// Accept Mastodon's `ES_*` variables for `[instance.elasticsearch]`, so that
/// a Mastodon `.env.production` configures search as it did there. A value
/// eunha's own spelling already sets is left alone.
fn adopt_mastodon_elasticsearch_env() {
    for (mastodon, field) in [
        ("ES_ENABLED", "ENABLED"),
        ("ES_HOST", "HOST"),
        ("ES_PORT", "PORT"),
        ("ES_USER", "USER"),
        ("ES_PASS", "PASS"),
        ("ES_PREFIX", "PREFIX"),
        ("ES_PRESET", "PRESET"),
        ("ES_CA_FILE", "CA_FILE"),
        ("ES_QUERY_TIMEOUT", "QUERY_TIMEOUT"),
    ] {
        let eunha = format!("INSTANCE__ELASTICSEARCH__{field}");
        if std::env::var_os(&eunha).is_some() {
            continue;
        }
        let Ok(value) = std::env::var(mastodon) else {
            continue;
        };
        // `ENV.fetch(...).presence`: a blank value is no value.
        if value.trim().is_empty() {
            continue;
        }
        // `ENV['ES_ENABLED'] == 'true'`: anything else is off.
        let value = if mastodon == "ES_ENABLED" {
            (value == "true").to_string()
        } else {
            value
        };
        // Safety: called once, before any threads read the environment.
        unsafe { std::env::set_var(eunha, value) };
    }
}

/// Accept Mastodon's own spelling of its secrets, the translation service,
/// the donation campaigns and self-destruct mode.
///
/// Eunha's environment keys nest with `__`, so its name for the primary key is
/// `ACTIVE_RECORD_ENCRYPTION__PRIMARY_KEY` — but the values themselves come
/// from a Mastodon installation, whose `.env.production` spells them with a
/// single underscore, and calls `instance.secret_key_base` `SECRET_KEY_BASE`.
/// Copying that file across should be enough.
fn adopt_mastodon_env() {
    for (mastodon, eunha) in [
        (
            "ACTIVE_RECORD_ENCRYPTION_PRIMARY_KEY",
            "ACTIVE_RECORD_ENCRYPTION__PRIMARY_KEY",
        ),
        (
            "ACTIVE_RECORD_ENCRYPTION_KEY_DERIVATION_SALT",
            "ACTIVE_RECORD_ENCRYPTION__KEY_DERIVATION_SALT",
        ),
        ("DEEPL_API_KEY", "INSTANCE__TRANSLATION__DEEPL_API_KEY"),
        ("DEEPL_PLAN", "INSTANCE__TRANSLATION__DEEPL_PLAN"),
        (
            "LIBRE_TRANSLATE_ENDPOINT",
            "INSTANCE__TRANSLATION__LIBRE_TRANSLATE_ENDPOINT",
        ),
        (
            "LIBRE_TRANSLATE_API_KEY",
            "INSTANCE__TRANSLATION__LIBRE_TRANSLATE_API_KEY",
        ),
        ("SECRET_KEY_BASE", "INSTANCE__SECRET_KEY_BASE"),
        ("DEFAULT_LOCALE", "INSTANCE__DEFAULT_LOCALE"),
        ("SELF_DESTRUCT", "INSTANCE__SELF_DESTRUCT"),
        (
            "DONATION_CAMPAIGNS_URL",
            "INSTANCE__DONATION_CAMPAIGNS__API_URL",
        ),
        (
            "DONATION_CAMPAIGNS_ENVIRONMENT",
            "INSTANCE__DONATION_CAMPAIGNS__ENVIRONMENT",
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
