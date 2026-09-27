use crate::config::{Config, InstanceConfig};
use crate::email::EmailSender;
use crate::media::Storage;
use crate::streaming::StreamBus;
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub redis: redis::aio::ConnectionManager,
    /// Non-evicting coordination state. This is the same manager as `redis`
    /// unless an operator configures a separate endpoint.
    pub redis_coordination: redis::aio::ConnectionManager,
    pub redis_keys: crate::redis_keys::RedisKeyspace,
    pub config: Arc<Config>,
    pub instance: Arc<InstanceConfig>,
    pub http: reqwest::Client,
    /// SSRF-guarded client for fetching untrusted remote content (ActivityPub
    /// objects, actor keys, link previews). See [`crate::federation::safe_fetch`].
    pub fetch: reqwest::Client,
    /// Fetches ActivityPub documents, signed as the instance actor, through
    /// feder's guarded client: each redirect is checked and signed again,
    /// and a document is trusted only from its own origin.
    pub fetcher: Arc<feder::fetch::Fetcher>,
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
    pub instance_actor_key: Arc<tokio::sync::OnceCell<Arc<feder_runtime::signature::PrivateKey>>>,
    /// Raised on enqueue so the durable queue loops need not poll for work.
    pub queues: Arc<crate::background::QueueWakes>,
    /// Outgoing deliveries, queued in `eunha.feder_queue` (federation::delivery).
    pub deliverer: Arc<crate::federation::delivery::Deliverer>,
    /// This instance's domain and media locations, which every URL it serves is
    /// built from. Held here rather than process-wide, so that one process can
    /// serve several instances.
    pub urls: Arc<crate::api::mastodon::convert::InstanceUrls>,
    /// Raised when this instance is stopped — removed from a running process,
    /// or restarted with a new configuration — so that its background loops
    /// return once they have finished the pass they are in, and its streaming
    /// connections close.
    pub stop: tokio_util::sync::CancellationToken,
}

impl AppState {
    pub async fn new(db: PgPool, config: Config) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(crate::version::USER_AGENT)
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");

        // Declared before the client is built, so the resolver it installs is
        // already answering with the operator's ranges in mind.
        let allowed: Vec<ipnet::IpNet> = config
            .allowed_private_networks
            .iter()
            .filter_map(|cidr| match cidr.parse() {
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
        crate::federation::safe_fetch::set_allowed_private_networks(allowed.clone());

        let fetch = crate::federation::safe_fetch::build_client();

        let storage = Arc::new(Storage::from_config(&config.media_storage).await);
        let urls = Arc::new(crate::api::mastodon::convert::InstanceUrls::new(
            config.instance.domain.clone(),
            storage.missing_avatar_url(),
            storage.missing_header_url(),
        ));
        let email = EmailSender::new(
            http.clone(),
            config.resend.api_key.clone(),
            config.resend.from.clone(),
        );

        let redis_keys = crate::redis_keys::RedisKeyspace::new(&config.redis_key_prefix)?;
        let redis_client = redis::Client::open(config.redis_url.as_str())?;
        let redis = redis::aio::ConnectionManager::new(redis_client).await?;
        let redis_coordination = if let Some(url) = config.redis_coordination_url.as_deref() {
            let client = redis::Client::open(url)?;
            redis::aio::ConnectionManager::new(client).await?
        } else {
            redis.clone()
        };

        let encryptor = config.active_record_encryption.as_ref().map(|keys| {
            crate::rails_encryption::Encryptor::new(&keys.primary_key, &keys.key_derivation_salt)
        });

        // Deliveries and fetches share one guarded client and its pool.
        let federation_client = feder::client::Client::new(feder::client::ClientConfig {
            allow_private: allowed.clone(),
            user_agent: crate::version::USER_AGENT.to_string(),
            ..feder::client::ClientConfig::default()
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        let fetcher = Arc::new(feder::fetch::Fetcher::new(
            federation_client.clone(),
            feder::delivery::Scheme::DraftCavage,
        ));
        let deliverer = Arc::new(crate::federation::delivery::deliverer(
            db.clone(),
            encryptor.clone(),
            &config.workers.sanitized(),
            federation_client,
        )?);

        let instance = Arc::new(config.instance.clone());
        Ok(Self {
            db,
            redis,
            redis_coordination,
            redis_keys,
            config: Arc::new(config),
            instance,
            http,
            fetch,
            fetcher,
            email,
            streaming: StreamBus::new(),
            storage,
            encryptor,
            instance_actor_key: Arc::default(),
            queues: Arc::default(),
            deliverer,
            urls,
            stop: tokio_util::sync::CancellationToken::new(),
        })
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
