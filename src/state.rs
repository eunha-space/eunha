use crate::config::{Config, InstanceConfig};
use crate::email::EmailSender;
use crate::media::Storage;
use crate::streaming::StreamBus;
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    /// Requests awaiting response headers; sampled only by the private metrics listener.
    pub metrics_in_flight: Arc<std::sync::atomic::AtomicU64>,
    pub redis: redis::aio::ConnectionManager,
    /// Non-evicting coordination state. This is the same manager as `redis`
    /// unless an operator configures a separate endpoint.
    pub redis_coordination: redis::aio::ConnectionManager,
    pub redis_keys: crate::redis_keys::RedisKeyspace,
    pub config: Arc<Config>,
    pub instance: Arc<InstanceConfig>,
    pub http: reqwest::Client,
    /// The SSRF-guarded client every request to a URL someone else chose goes
    /// through — link previews, profile link verification, FASP, the update
    /// check — as Mastodon's `Request` guards them: ojak's, refusing what
    /// `PrivateAddressCheck` refuses unless this instance's
    /// `allowed_private_networks` names it. The same client, and pool, as
    /// `fetcher`'s and the deliverer's.
    pub fetch: ojak::client::Client,
    /// Fetches ActivityPub documents, signed as the instance actor, through
    /// ojak's guarded client: each redirect is checked and signed again,
    /// and a document is trusted only from its own origin.
    pub fetcher: Arc<ojak::fetch::Fetcher>,
    /// What ojak caches for this instance — activities seen and forwarded,
    /// keys eunha does not store, JSON-LD contexts — in Redis behind the
    /// instance's key prefix, on the evictable `redis` pool: losing an entry
    /// costs a refetch, or a redelivery processed again.
    pub federation_kv: Arc<ojak_redis::RedisKvStore<redis::aio::ConnectionManager>>,
    pub email: EmailSender,
    pub streaming: StreamBus,
    pub storage: Arc<Storage>,
    /// Reads and writes Mastodon's encrypted `keypairs.private_key` column.
    /// `None` when the instance has not been given the encryption keys, in
    /// which case signing keys stay in the legacy `accounts` columns.
    pub encryptor: Option<crate::rails_encryption::Encryptor>,
    /// The instance actor's signing key, parsed on first use. Every signed GET
    /// uses it, and loading, decrypting and parsing an RSA key costs more than
    /// signing with it — a post going viral had that at a tenth of eunha's CPU.
    pub instance_actor_key: Arc<tokio::sync::OnceCell<Arc<ojak::sig::signature::PrivateKey>>>,
    /// Raised on enqueue so the durable queue loops need not poll for work.
    pub queues: Arc<crate::background::QueueWakes>,
    /// How this instance runs the jobs in its job queue (crate::jobs).
    pub jobs: Arc<crate::jobs::Runtime>,
    /// Outgoing deliveries, queued in `eunha.ojak_queue` (federation::delivery).
    pub deliverer: Arc<crate::federation::delivery::Deliverer>,
    /// Which servers have stopped answering deliveries, as Mastodon tracks it
    /// (federation::delivery_failures).
    pub delivery_failures: crate::federation::delivery_failures::DeliveryFailureTracker,
    /// This instance's domain and media locations, which every URL it serves is
    /// built from. Held here rather than process-wide, so that one process can
    /// serve several instances.
    pub urls: Arc<crate::api::mastodon::convert::InstanceUrls>,
    /// The URIs of what ojak serves for this instance — actors, their inboxes
    /// and collections, statuses — built from the templates they are served
    /// at, for code that has no request to build them from.
    pub uris: ojak::federation::Uris,
    /// Raised when this instance is stopped — removed from a running process,
    /// or restarted with a new configuration — so that its background loops
    /// return once they have finished the pass they are in, and its streaming
    /// connections close.
    pub stop: tokio_util::sync::CancellationToken,
    /// The Elasticsearch or OpenSearch cluster this instance searches, when
    /// `[instance.elasticsearch]` enables one (crate::search::elasticsearch).
    pub search: Option<Arc<crate::search::elasticsearch::Client>>,
}

impl AppState {
    pub async fn new(db: PgPool, config: Config) -> anyhow::Result<Self> {
        let deprecated = config.instance.deprecated_site_keys();
        if !deprecated.is_empty() {
            tracing::warn!(
                keys = ?deprecated,
                "these [instance] keys are no longer read; `eunha migrate` copies them into \
                 the settings once for an instance that was serving, and \
                 `eunha settings import-config` whenever asked; remove them"
            );
        }
        let http = reqwest::Client::builder()
            .user_agent(crate::version::USER_AGENT)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");

        // `ALLOWED_PRIVATE_ADDRESSES`: networks the guarded client may reach
        // although they are not public.
        let allowed: Vec<ipnet::IpNet> = config
            .allowed_private_networks
            .iter()
            // `IPAddr.new`: a network, or a single address.
            .filter_map(|cidr| match cidr
                .parse::<ipnet::IpNet>()
                .or_else(|e| cidr.parse::<std::net::IpAddr>().map(Into::into).map_err(|_| e))
            {
                Ok(net) => Some(net),
                Err(e) => {
                    tracing::error!(cidr, error = %e, "ignoring unparseable allowed_private_networks entry");
                    None
                }
            })
            .collect();
        if !allowed.is_empty() {
            tracing::warn!(
                networks = ?allowed,
                "federation may reach these private networks; this relaxes an SSRF protection"
            );
        }
        let storage = Arc::new(Storage::from_config(&config.media_storage).await);
        let urls = Arc::new(crate::api::mastodon::convert::InstanceUrls::new(
            config.instance.domain.clone(),
            storage.missing_avatar_url(),
            storage.missing_header_url(),
        ));
        let email = EmailSender::new(config.smtp.as_ref())?;

        let redis_keys = crate::redis_keys::RedisKeyspace::new(&config.redis_key_prefix)?;
        let redis_client = redis::Client::open(config.redis_url.as_str())?;
        let redis = redis::aio::ConnectionManager::new(redis_client.clone()).await?;
        let redis_coordination = if let Some(url) = config.redis_coordination_url.as_deref() {
            let client = redis::Client::open(url)?;
            redis::aio::ConnectionManager::new(client).await?
        } else {
            redis.clone()
        };

        let federation_kv = Arc::new(ojak_redis::RedisKvStore::new(
            redis.clone(),
            redis_keys.key(""),
        ));

        let encryptor = config.active_record_encryption.as_ref().map(|keys| {
            crate::rails_encryption::Encryptor::new(&keys.primary_key, &keys.key_derivation_salt)
        });

        // Deliveries, fetches and every other request to a URL someone else
        // chose share one guarded client and its pool.
        let federation_client = ojak::client::Client::new(ojak::client::ClientConfig {
            allow_private: allowed,
            user_agent: crate::version::USER_AGENT.to_string(),
            ..ojak::client::ClientConfig::default()
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        let fetch = federation_client.clone();
        let fetcher = Arc::new(ojak::fetch::Fetcher::new(
            federation_client.clone(),
            ojak::sig::Scheme::DraftCavage,
        ));
        let delivery_failures = crate::federation::delivery_failures::DeliveryFailureTracker::new(
            db.clone(),
            redis_coordination.clone(),
            redis_keys.clone(),
        );
        let queues: Arc<crate::background::QueueWakes> = Arc::default();
        let deliverer = Arc::new(crate::federation::delivery::deliverer(
            db.clone(),
            encryptor.clone(),
            &config.workers.sanitized(),
            federation_client,
            delivery_failures.clone(),
            crate::federation::delivery::RedisBreakers::new(
                redis_coordination.clone(),
                redis_keys.clone(),
            ),
            queues.clone(),
        )?);

        let uris = crate::api::ap::serving::uris(&config.instance.domain)?;
        let search = if config.instance.elasticsearch.enabled {
            Some(Arc::new(crate::search::elasticsearch::Client::new(
                &config.instance.elasticsearch,
            )?))
        } else {
            None
        };
        let instance = Arc::new(config.instance.clone());
        let stop = tokio_util::sync::CancellationToken::new();
        let streaming = StreamBus::new(
            redis_client,
            redis.clone(),
            redis_keys.clone(),
            stop.clone(),
        );
        Ok(Self {
            db,
            metrics_in_flight: Arc::default(),
            redis,
            redis_coordination,
            redis_keys,
            config: Arc::new(config),
            instance,
            http,
            fetch,
            fetcher,
            federation_kv,
            email,
            streaming,
            storage,
            encryptor,
            instance_actor_key: Arc::default(),
            queues,
            jobs: Arc::default(),
            deliverer,
            delivery_failures,
            urls,
            uris,
            stop,
            search,
        })
    }
}

impl AppState {
    /// The mailer `deliver_later` sends through: what it is given to send
    /// goes into the job queue, as `ActionMailer::MailDeliveryJob`, and out
    /// from there.
    pub fn mailer(&self) -> EmailSender {
        self.email.later(self)
    }
}

/// The instance a request is for, which the tenant dispatcher puts on every
/// request before the router sees it.
///
/// Handlers take this rather than axum's `State` so that one router serves
/// every instance in the process: `State` bakes one instance into the routes,
/// and building 594 routes for each tenant cost about 1.4 MiB apiece.
impl<S: Send + Sync> axum::extract::FromRequestParts<S> for AppState {
    type Rejection = crate::error::AppError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<AppState>().cloned().ok_or_else(|| {
            crate::error::AppError::Internal(anyhow::anyhow!(
                "no instance on this request: it did not come through the tenant dispatcher"
            ))
        })
    }
}
