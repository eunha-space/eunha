//! Remote actors stored as Mastodon's `ActivityPub::ProcessAccountService`
//! stores them.
//!
//! Every path that learns about a remote actor — the document fetched for a
//! first activity's key, an `Update` of the actor, a search, a `Move` — ends
//! here, so that an account's columns are the ones Mastodon would have written
//! from the same document: the profile and its limits, the flags that decide
//! where the account may be shown (`discoverable`, `indexable`), the
//! collections and keys, and the follow-up work the service queues
//! (featured posts and hashtags, link verification, re-following after a
//! change of identity). docs/mastodon/remote-actors.md lists each column.
//!
//! The caller is responsible for the document being the actor's own: fetched
//! from the origin of its `id`, or delivered by it.

use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use futures::future::BoxFuture;
use serde_json::{Map, Value};

use crate::db::models::Account;
use crate::delete_account::suspension_origin;
use crate::federation::replies::is_present;
use crate::state::AppState;

/// `MAX_PUBLIC_KEYS`.
const MAX_PUBLIC_KEYS: usize = 10;
/// `MAX_PROFILE_FIELDS`.
const MAX_PROFILE_FIELDS: usize = 50;
/// `SUBDOMAINS_RATELIMIT`.
const SUBDOMAINS_RATELIMIT: i64 = 10;
/// `DISCOVERIES_PER_REQUEST`.
const DISCOVERIES_PER_REQUEST: i64 = 400;
/// `Account::DISPLAY_NAME_LENGTH_HARD_LIMIT`.
const DISPLAY_NAME_LENGTH_HARD_LIMIT: usize = 2048;
/// `Account::NOTE_LENGTH_HARD_LIMIT`, `20.kilobytes`.
const NOTE_LENGTH_HARD_LIMIT: usize = 20 * 1024;
/// `Account::USERNAME_LENGTH_HARD_LIMIT`.
const USERNAME_LENGTH_HARD_LIMIT: usize = 2048;
/// `Account::ATTRIBUTION_DOMAINS_HARD_LIMIT`.
const ATTRIBUTION_DOMAINS_HARD_LIMIT: usize = 256;
/// `MediaAttachment::MAX_DESCRIPTION_HARD_LENGTH_LIMIT`.
const MAX_DESCRIPTION_HARD_LENGTH_LIMIT: usize = 10_000;
/// `Account::STALE_THRESHOLD`.
const STALE_THRESHOLD: chrono::Duration = chrono::Duration::days(1);
/// `Account::BACKGROUND_REFRESH_INTERVAL`.
const BACKGROUND_REFRESH_INTERVAL: chrono::Duration = chrono::Duration::weeks(1);
/// `Account::REFRESH_DEADLINE`.
const REFRESH_DEADLINE: Duration = Duration::from_secs(6 * 60 * 60);

/// `accounts.protocol`: `ostatus: 0, activitypub: 1`.
const PROTOCOL_ACTIVITYPUB: i32 = 1;

/// `keypairs.type`: `rsa: 0, ed25519: 1, 'ml-dsa-44': 2`.
mod key_type {
    pub const RSA: i32 = 0;
    pub const ED25519: i32 = 1;
    pub const ML_DSA_44: i32 = 2;
}

/// The call's options, as `ProcessAccountService#call` takes them.
#[derive(Default, Clone, Copy)]
pub struct Options<'a> {
    /// The account the document is expected to describe: the sender of an
    /// `Update`.
    pub account: Option<&'a Account>,
    /// Only refresh the keys of an account already known by its `id`.
    pub only_key: bool,
    /// The document came in an activity signed with a key already held,
    /// so a change of every key is not a change of identity.
    pub signed_with_known_key: bool,
    pub request_id: Option<&'a str>,
}

/// `ProcessAccountService#call`: create or update the account `json`
/// describes, returning its id, or `None` where Mastodon returns `nil`.
/// An `Err` is what Mastodon raises as `ProcessAccountService::Error`.
pub fn process<'a>(
    state: &'a AppState,
    json: &'a Value,
    options: Options<'a>,
) -> BoxFuture<'a, Result<Option<i64>>> {
    Box::pin(process_inner(state, json, options))
}

async fn process_inner(
    state: &AppState,
    json: &Value,
    options: Options<'_>,
) -> Result<Option<i64>> {
    let id = json.get("id").and_then(Value::as_str).unwrap_or_default();
    let portable = crate::federation::portable::reach(json);
    if portable.is_none() && unsupported_uri_scheme(id) {
        bail!("Actor {id} has unsupported URI scheme");
    }
    if !is_present(json.get("inbox").unwrap_or(&Value::Null)) {
        bail!("Actor {id} has no inbox");
    }
    let uri = crate::federation::portable::canonical(id);
    if let Some(account) = options.account {
        if account.stored_uri() != Some(uri.as_str()) {
            bail!(
                "Actor {uri} does not correspond to provided Account ({})",
                account.uri.as_deref().unwrap_or_default()
            );
        }
    }
    if crate::federation::moderation::domain_not_allowed(state, &uri).await
        || options.account.is_some_and(Account::is_local)
    {
        return Ok(None);
    }

    // `extract_username_and_domain!`.
    let (mut username, mut domain) = match json.get("webfinger").and_then(Value::as_str) {
        Some(acct) if !acct.trim().is_empty() => crate::federation::webfinger::split_acct(acct),
        _ => (String::new(), String::new()),
    };
    if username.trim().is_empty() || domain.trim().is_empty() {
        let preferred = json
            .get("preferredUsername")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "Actor {uri} has no `preferredUsername`, and either a bogus or missing \
                     `webfinger`, which is a requirement for Mastodon compatibility"
                )
            })?;
        username = preferred.to_owned();
        domain = match &portable {
            Some(reach) => reach.domain.clone(),
            None => url::Url::parse(&uri)
                .ok()
                .and_then(|url| url.host_str().map(str::to_owned))
                .unwrap_or_default(),
        };
    }
    // `normalizes :username, with: squish`.
    let mut username = squish(&username);
    let mut domain = normalize_domain(&domain);

    let mut webfinger_verified = options.account.is_some_and(|account| {
        account.username == username && account.domain.as_deref() == Some(domain.as_str())
    });
    if !webfinger_verified && !options.only_key {
        if portable.is_some() {
            // A portable actor is vouched for by the key its id names, and
            // proved its document with it; there is no host to ask.
            webfinger_verified = true;
        } else {
            let (confirmed_username, confirmed_domain) =
                crate::federation::webfinger::confirm(&state.fetcher, &username, &domain, &uri)
                    .await
                    .with_context(|| {
                        format!("Webfinger error when resolving {username}@{domain}")
                    })?;
            username = confirmed_username;
            domain = confirmed_domain;
            webfinger_verified = true;
        }
    }

    if options.account.is_none()
        && crate::federation::moderation::domain_not_allowed(state, &domain).await
    {
        return Ok(None);
    }

    let request_id = options
        .request_id
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{}-{username}@{domain}", chrono::Utc::now().timestamp()));

    let lock = acquire_lock(state, &format!("process_account:{uri}")).await;

    let mut account = match options.account {
        Some(account) => Some(account.clone()),
        None => find_by_uri(state, &uri).await?,
    };
    if account.is_none() && options.only_key {
        return Ok(None);
    }
    if account.is_none() && webfinger_verified {
        account = find_remote(state, &username, &domain).await?;
    }

    let old_public_keys = match &account {
        Some(account) => {
            let mut keys: Vec<String> = sqlx::query_scalar!(
                "SELECT public_key FROM keypairs WHERE account_id = $1",
                account.id
            )
            .fetch_all(&state.db)
            .await?;
            if !account.public_key.is_empty() {
                keys.push(account.public_key.clone());
            }
            keys
        }
        None => Vec::new(),
    };
    let old_protocol = account.as_ref().map(|account| account.protocol);
    let uri_changed = account
        .as_ref()
        .is_some_and(|account| account.stored_uri() != Some(uri.as_str()));

    let was_discoverable = account
        .as_ref()
        .map(|account| account.discoverable.unwrap_or(false));
    let account_id = match account {
        None => {
            if over_discovery_limits(state, &domain, &request_id).await {
                return Ok(None);
            }
            if !webfinger_verified {
                bail!(
                    "Attempting to create an account without having verified its webfinger handle"
                );
            }
            let id = create_account(state, &username, &domain, &uri).await?;
            // `after_commit :announce_new_account_to_subscribed_fasp, on: :create`.
            crate::fasp::events::account_created(state, id).await;
            id
        }
        Some(account) => {
            if webfinger_verified {
                rename_account(state, &account, &username, &domain, &request_id).await?;
            }
            account.id
        }
    };

    let account = load(state, account_id).await?;
    let mut processor = Processor {
        state,
        json,
        uri: &uri,
        portable: portable.as_ref(),
        domain: &domain,
        only_key: options.only_key,
        request_id: &request_id,
        collections: Default::default(),
    };
    let suspended = processor.update_account(&account).await?;
    let account = load(state, account_id).await?;
    // The callbacks of the account's save: `update_index('accounts', :self)`,
    // and `announce_updated_account_to_subscribed_fasp`, which speaks while
    // the account is discoverable or when this save changed whether it is.
    crate::search::elasticsearch::indexing::account(state, account_id).await;
    let discoverable = account.discoverable.unwrap_or(false);
    crate::fasp::events::account_updated(
        state,
        account_id,
        was_discoverable.unwrap_or(false) != discoverable,
    )
    .await;
    processor.process_tags(&account, suspended).await;

    // While `rename_account!` makes this unlikely, `uri` is not unique.
    if webfinger_verified {
        process_duplicate_accounts(state, &account).await?;
    }
    drop(lock);

    if old_protocol.is_some_and(|protocol| protocol != PROTOCOL_ACTIVITYPUB) {
        after_protocol_change(state, &domain).await?;
    }
    let all_public_keys_changed =
        all_public_keys_changed(state, account_id, &old_public_keys).await?;
    if uri_changed || (!options.signed_with_known_key && all_public_keys_changed) {
        refollow_later(state, account_id);
    }
    // `clear_tombstones!`.
    if all_public_keys_changed {
        sqlx::query!("DELETE FROM tombstones WHERE account_id = $1", account_id)
            .execute(&state.db)
            .await?;
    }

    if !options.only_key && !suspended {
        let featured_tags = json.get("featuredTags").filter(|v| is_present(v));
        if let Some(featured) = json.get("featured").filter(|v| is_present(v)) {
            crate::federation::featured::synchronize_featured_collection_later(
                state,
                account_id,
                value_or_id_owned(featured),
                featured_tags.is_none(),
                &request_id,
            );
        }
        if let Some(featured_tags) = featured_tags {
            crate::federation::featured::synchronize_featured_tags_collection_later(
                state,
                account_id,
                value_or_id_owned(featured_tags),
            );
        }
        if json.get("featuredCollections").is_some_and(is_present) {
            crate::federation::featured::synchronize_featured_collections_collection_later(
                state,
                account_id,
                &request_id,
            );
        }
        if account
            .fields
            .as_ref()
            .is_some_and(crate::link_verification::any_requires_verification_remote)
        {
            crate::link_verification::spawn_later(state, account_id);
        }
    }

    Ok(Some(account_id))
}

/// `with_redis_lock`, as eunha takes its locks: retried briefly, then done
/// without, rather than holding up the inbox.
async fn acquire_lock(state: &AppState, name: &str) -> Option<crate::redis_lock::RedisLock> {
    for attempt in 0..40 {
        if let Some(lock) =
            crate::redis_lock::try_acquire(state, name, crate::redis_lock::DEFAULT_TTL_MS).await
        {
            return Some(lock);
        }
        if attempt < 39 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    None
}

/// What one call works out once and uses throughout.
struct Processor<'a> {
    state: &'a AppState,
    json: &'a Value,
    uri: &'a str,
    portable: Option<&'a crate::federation::portable::Reach>,
    domain: &'a str,
    only_key: bool,
    request_id: &'a str,
    /// `@collections`: what each of `outbox`, `following` and `followers`
    /// said, fetched once.
    collections: std::collections::HashMap<&'static str, (Option<i64>, bool)>,
}

impl Processor<'_> {
    /// `update_account`. Returns whether the account is suspended.
    async fn update_account(&mut self, account: &Account) -> Result<bool> {
        let state = self.state;
        // `set_suspension!`.
        let suspended = crate::moderation::remote::set_suspension(
            state,
            account,
            truthy(self.json.get("suspended")),
        )
        .await?;
        let suspended_locally = suspended
            && account.suspended_at.is_some()
            && account.suspension_origin != Some(suspension_origin::REMOTE);

        // `set_immediate_protocol_attributes!`, and the two columns
        // `update_account` sets itself.
        let allow_ap = self.portable.is_some();
        let (inbox_url, outbox_url, shared_inbox_url) = match self.portable {
            Some(reach) => (
                reach.inbox.clone(),
                reach.outbox.clone(),
                reach.shared_inbox.clone(),
            ),
            None => (
                valid_collection_uri(self.json.get("inbox"), false),
                valid_collection_uri(self.json.get("outbox"), false),
                valid_collection_uri(
                    match self.json.get("endpoints") {
                        Some(endpoints) if endpoints.is_object() => endpoints.get("sharedInbox"),
                        _ => self.json.get("sharedInbox"),
                    },
                    false,
                ),
            ),
        };
        let followers_url = valid_collection_uri(self.json.get("followers"), allow_ap);
        let following_url = valid_collection_uri(self.json.get("following"), allow_ap);
        let url = self.url().unwrap_or_else(|| self.uri.to_owned());
        let published = self
            .json
            .get("published")
            .and_then(Value::as_str)
            .and_then(|published| chrono::DateTime::parse_from_rfc3339(published).ok())
            .map(|published| published.naive_utc());
        let feature_approval_policy = crate::db::models::feature_policy::parse(
            self.json
                .get("interactionPolicy")
                .and_then(|policy| policy.get("canFeature")),
            &followers_url,
            &following_url,
            self.uri,
        );
        sqlx::query!(
            r#"UPDATE accounts
               SET last_webfingered_at = CASE WHEN $2 THEN last_webfingered_at ELSE now() END,
                   protocol = $3,
                   inbox_url = $4,
                   outbox_url = $5,
                   shared_inbox_url = $6,
                   followers_url = $7,
                   following_url = $8,
                   url = $9,
                   uri = $10,
                   actor_type = $11,
                   created_at = COALESCE($12, created_at),
                   feature_approval_policy = $13,
                   updated_at = now()
               WHERE id = $1"#,
            account.id,
            self.only_key,
            PROTOCOL_ACTIVITYPUB,
            inbox_url,
            outbox_url,
            shared_inbox_url,
            followers_url,
            following_url,
            url,
            self.uri,
            self.actor_type(),
            published,
            feature_approval_policy,
        )
        .execute(&state.db)
        .await?;

        if !suspended_locally {
            self.set_fetchable_key(account.id).await?;
        }
        if !suspended {
            self.set_immediate_attributes(account.id).await?;
        }
        if !self.only_key && !suspended {
            self.set_fetchable_attributes(account).await?;
        }
        Ok(suspended)
    }

    /// `actor_type`: the first supported type of several, or the one given.
    fn actor_type(&self) -> Option<String> {
        match self.json.get("type") {
            Some(Value::Array(types)) => types
                .iter()
                .filter_map(Value::as_str)
                .find(|kind| crate::federation::fetch_resource::ACTOR_TYPES.contains(kind))
                .map(str::to_owned),
            Some(Value::String(kind)) => Some(kind.clone()),
            _ => None,
        }
    }

    /// `url`: the actor's HTML page, when it is on the actor's own host.
    fn url(&self) -> Option<String> {
        let value = self.json.get("url").filter(|url| is_present(url))?;
        let candidate = url_to_href(value, Some("text/html"))?;
        if unsupported_uri_scheme(&candidate) {
            return None;
        }
        let host = |uri: &str| {
            url::Url::parse(uri)
                .ok()
                .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
        };
        let haystack = match self.portable {
            Some(reach) => Some(reach.domain.to_ascii_lowercase()),
            None => host(self.uri),
        };
        (haystack.is_some() && host(&candidate) == haystack).then_some(candidate)
    }

    /// `set_immediate_attributes!`.
    async fn set_immediate_attributes(&self, account_id: i64) -> Result<()> {
        let json = self.json;
        let allow_ap = self.portable.is_some();
        let featured_collection_url = valid_collection_uri(json.get("featured"), allow_ap);
        let collections_url = valid_collection_uri(json.get("featuredCollections"), allow_ap);
        let display_name = truncate_chars(
            json.get("name").and_then(Value::as_str).unwrap_or_default(),
            DISPLAY_NAME_LENGTH_HARD_LIMIT,
        );
        let note = truncate_chars(
            json.get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            NOTE_LENGTH_HARD_LIMIT,
        );
        let locked = active_record_boolean(json.get("manuallyApprovesFollowers"));
        let fields = property_values(json).unwrap_or_else(|| Value::Object(Map::new()));
        let also_known_as = also_known_as_of(json);
        let discoverable = active_record_boolean(json.get("discoverable"));
        let indexable = active_record_boolean(json.get("indexable"));
        let memorial = active_record_boolean(json.get("memorial"));
        // Set only when the actor says, and a `null` is not a value the
        // column can take.
        let optional = |key: &str| {
            json.get(key)
                .filter(|value| !value.is_null())
                .map(|value| active_record_boolean(Some(value)))
        };
        let attribution_domains: Vec<String> = as_array(json.get("attributionDomains"))
            .into_iter()
            .take(ATTRIBUTION_DOMAINS_HARD_LIMIT)
            .filter_map(|domain| domain.as_str().map(str::to_owned))
            .collect();
        sqlx::query!(
            r#"UPDATE accounts
               SET featured_collection_url = $2,
                   collections_url = $3,
                   display_name = $4,
                   note = $5,
                   locked = $6,
                   fields = $7,
                   also_known_as = $8,
                   discoverable = $9,
                   indexable = $10,
                   memorial = $11,
                   show_featured = COALESCE($12, show_featured),
                   show_media = COALESCE($13, show_media),
                   show_media_replies = COALESCE($14, show_media_replies),
                   attribution_domains = $15
               WHERE id = $1"#,
            account_id,
            featured_collection_url,
            collections_url,
            display_name,
            note,
            locked,
            fields,
            &also_known_as,
            discoverable,
            indexable,
            memorial,
            optional("showFeatured"),
            optional("showMedia"),
            optional("showRepliesInMedia"),
            &attribution_domains,
        )
        .execute(&self.state.db)
        .await?;
        Ok(())
    }

    /// `set_fetchable_key!`: the account's keys are the ones the document
    /// publishes, kept in `keypairs`; the legacy column is cleared.
    async fn set_fetchable_key(&self, account_id: i64) -> Result<()> {
        let keys = self.public_keys().await;
        let uris: Vec<String> = keys.iter().map(|key| key.uri.clone()).collect();
        for key in &keys {
            sqlx::query!(
                r#"INSERT INTO keypairs (account_id, uri, type, public_key, created_at, updated_at)
                   VALUES ($1, $2, $3, $4, now(), now())
                   ON CONFLICT (uri) DO UPDATE
                     SET account_id = EXCLUDED.account_id,
                         type = EXCLUDED.type,
                         public_key = EXCLUDED.public_key,
                         updated_at = now()"#,
                account_id,
                key.uri,
                key.kind,
                key.pem,
            )
            .execute(&self.state.db)
            .await?;
        }
        sqlx::query!(
            "DELETE FROM keypairs WHERE account_id = $1 AND uri <> ALL($2)",
            account_id,
            &uris,
        )
        .execute(&self.state.db)
        .await?;
        sqlx::query!(
            "UPDATE accounts SET public_key = '' WHERE id = $1",
            account_id
        )
        .execute(&self.state.db)
        .await?;
        Ok(())
    }

    /// `public_keys`: FEP-521a keys, then legacy ones, one per id.
    async fn public_keys(&self) -> Vec<RemoteKey> {
        let mut keys = self.fep_521a_public_keys().await;
        keys.extend(self.legacy_public_keys().await);
        let mut seen = std::collections::HashSet::new();
        keys.retain(|key| seen.insert(key.uri.clone()));
        keys
    }

    /// `legacy_public_keys`: `publicKey`, embedded when it is the actor's
    /// own fragment, fetched otherwise.
    async fn legacy_public_keys(&self) -> Vec<RemoteKey> {
        let mut keys = Vec::new();
        for value in as_array(self.json.get("publicKey"))
            .into_iter()
            .take(MAX_PUBLIC_KEYS)
        {
            let key_id = match value {
                Value::Object(key) => {
                    if key.get("owner").and_then(Value::as_str) != Some(self.uri) {
                        continue;
                    }
                    let Some(id) = key.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    if id.split('#').next() == Some(self.uri) {
                        if let Some(pem) = key.get("publicKeyPem").and_then(Value::as_str) {
                            keys.push(RemoteKey {
                                kind: key_type::RSA,
                                pem: pem.to_owned(),
                                uri: id.to_owned(),
                            });
                        }
                        continue;
                    }
                    id.to_owned()
                }
                Value::String(id) => id.clone(),
                _ => continue,
            };
            // Fetched without checking its id, for GoToSocial, which serves
            // the whole actor at the key's id.
            let Ok(document) = crate::federation::fetch::signed_get_json(self.state, &key_id).await
            else {
                continue;
            };
            let key = match document.get("publicKey") {
                Some(published) => first_of_value(published).cloned().unwrap_or(Value::Null),
                None => document,
            };
            if key.get("owner").and_then(Value::as_str) != Some(self.uri) {
                continue;
            }
            if let Some(pem) = key.get("publicKeyPem").and_then(Value::as_str) {
                keys.push(RemoteKey {
                    kind: key_type::RSA,
                    pem: pem.to_owned(),
                    uri: key_id,
                });
            }
        }
        keys
    }

    /// `fep_521a_public_keys`: the `Multikey`s under `assertionMethod`.
    async fn fep_521a_public_keys(&self) -> Vec<RemoteKey> {
        let mut keys = Vec::new();
        for value in as_array(self.json.get("assertionMethod"))
            .into_iter()
            .take(MAX_PUBLIC_KEYS)
        {
            let key_id = match value {
                Value::Object(key) => {
                    if key.get("type").and_then(Value::as_str) != Some("Multikey")
                        || key.get("controller").and_then(Value::as_str) != Some(self.uri)
                    {
                        continue;
                    }
                    let Some((kind, pem)) = key_from_multikey(key.get("publicKeyMultibase")) else {
                        continue;
                    };
                    let Some(id) = key.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    if id.split('#').next() == Some(self.uri) {
                        keys.push(RemoteKey {
                            kind,
                            pem,
                            uri: id.to_owned(),
                        });
                        continue;
                    }
                    id.to_owned()
                }
                Value::String(id) => id.clone(),
                _ => continue,
            };
            let Ok(key) = crate::federation::fetch::signed_get_json(self.state, &key_id).await
            else {
                continue;
            };
            if key.get("id").and_then(Value::as_str) != Some(key_id.as_str())
                || key.get("type").and_then(Value::as_str) != Some("Multikey")
                || key.get("controller").and_then(Value::as_str) != Some(self.uri)
            {
                continue;
            }
            if let Some((kind, pem)) = key_from_multikey(key.get("publicKeyMultibase")) {
                keys.push(RemoteKey {
                    kind,
                    pem,
                    uri: key_id,
                });
            }
        }
        keys
    }

    /// `set_fetchable_attributes!`.
    async fn set_fetchable_attributes(&mut self, account: &Account) -> Result<()> {
        let state = self.state;
        let skip_download = self.skip_download().await;

        let (avatar_url, avatar_description) = self.image_url_and_description("icon").await;
        let (header_url, header_description) = self.image_url_and_description("image").await;
        let avatar_remote_url = if skip_download {
            account.avatar_remote_url.clone().unwrap_or_default()
        } else {
            avatar_url.unwrap_or_default()
        };
        let header_remote_url = if skip_download {
            account.header_remote_url.clone()
        } else {
            header_url.unwrap_or_default()
        };
        // `@account.avatar = nil if @account.avatar_remote_url.blank?`. Eunha
        // shows a remote account's image from where its server keeps it, and
        // forgets a copy Mastodon made of an image that has since changed, so
        // that a Mastodon sharing this database fetches the new one.
        let clear_avatar = avatar_remote_url.trim().is_empty()
            || account.avatar_remote_url.as_deref() != Some(avatar_remote_url.as_str());
        let clear_header =
            header_remote_url.trim().is_empty() || account.header_remote_url != header_remote_url;
        sqlx::query!(
            r#"UPDATE accounts
               SET avatar_remote_url = $2,
                   avatar_description = $3,
                   avatar_file_name = CASE WHEN $4 THEN NULL ELSE avatar_file_name END,
                   avatar_content_type = CASE WHEN $4 THEN NULL ELSE avatar_content_type END,
                   avatar_file_size = CASE WHEN $4 THEN NULL ELSE avatar_file_size END,
                   avatar_updated_at = CASE WHEN $4 THEN NULL ELSE avatar_updated_at END,
                   header_remote_url = $5,
                   header_description = $6,
                   header_file_name = CASE WHEN $7 THEN NULL ELSE header_file_name END,
                   header_content_type = CASE WHEN $7 THEN NULL ELSE header_content_type END,
                   header_file_size = CASE WHEN $7 THEN NULL ELSE header_file_size END,
                   header_updated_at = CASE WHEN $7 THEN NULL ELSE header_updated_at END
               WHERE id = $1"#,
            account.id,
            avatar_remote_url,
            avatar_description.unwrap_or_default(),
            clear_avatar,
            header_remote_url,
            header_description.unwrap_or_default(),
            clear_header,
        )
        .execute(&state.db)
        .await?;

        let (statuses_count, _) = self.collection_info("outbox").await;
        let (following_count, following_public) = self.collection_info("following").await;
        let (followers_count, followers_public) = self.collection_info("followers").await;
        if statuses_count.is_some() || following_count.is_some() || followers_count.is_some() {
            sqlx::query!(
                r#"INSERT INTO account_stats
                     (account_id, statuses_count, following_count, followers_count, created_at, updated_at)
                   VALUES ($1, COALESCE($2::bigint, 0), COALESCE($3::bigint, 0), COALESCE($4::bigint, 0), now(), now())
                   ON CONFLICT (account_id) DO UPDATE
                     SET statuses_count = COALESCE($2::bigint, account_stats.statuses_count),
                         following_count = COALESCE($3::bigint, account_stats.following_count),
                         followers_count = COALESCE($4::bigint, account_stats.followers_count),
                         updated_at = now()"#,
                account.id,
                statuses_count,
                following_count,
                followers_count,
            )
            .execute(&state.db)
            .await?;
        }
        let hide_collections = !following_public || !followers_public;

        let moved_to = match self.json.get("movedTo").filter(|v| is_present(v)) {
            Some(moved) => self.moved_account(moved).await,
            None => None,
        };
        sqlx::query!(
            "UPDATE accounts SET hide_collections = $2, moved_to_account_id = $3 WHERE id = $1",
            account.id,
            hide_collections,
            moved_to,
        )
        .execute(&state.db)
        .await?;
        Ok(())
    }

    /// `skip_download?`, for an account that is not suspended: its domain
    /// is blocked with `reject_media`.
    async fn skip_download(&self) -> bool {
        crate::federation::moderation::lookup(self.state, self.domain)
            .await
            .is_some_and(|block| block.reject_media)
    }

    /// `image_url_and_description`: an `Image`'s URL, and its `summary` or
    /// `name` as its description.
    async fn image_url_and_description(&self, key: &str) -> (Option<String>, Option<String>) {
        let Some(mut value) = self.json.get(key).and_then(first_of_value).cloned() else {
            return (None, None);
        };
        if let Value::String(url) = &value {
            match crate::federation::fetch::signed_get_json(self.state, url).await {
                Ok(fetched) => value = fetched,
                Err(_) => return (None, None),
            }
        }
        let mut description = None;
        let mut url = if value.get("type").and_then(Value::as_str) == Some("Image") {
            let mut url = value.get("url").and_then(first_of_value).cloned();
            if let Some(Value::Object(link)) = &url {
                url = link.get("href").cloned();
            }
            description = first_lang_string(&value, "summary")
                .filter(|text| !text.trim().is_empty())
                .or_else(|| {
                    first_lang_string(&value, "name").filter(|text| !text.trim().is_empty())
                })
                .map(|text| truncate_chars(text.trim(), MAX_DESCRIPTION_HARD_LENGTH_LIMIT));
            url
        } else {
            Some(value)
        };
        if let Some(Value::Object(link)) = &url {
            url = link.get("href").cloned();
        }
        (
            url.and_then(|url| url.as_str().map(str::to_owned)),
            description,
        )
    }

    /// `collection_info`: a collection's `totalItems`, and whether it has a
    /// first page, which is how a server says its contents are public.
    async fn collection_info(&mut self, kind: &'static str) -> (Option<i64>, bool) {
        let uri = valid_collection_uri(self.json.get(kind), false);
        if uri.is_empty() {
            return (None, false);
        }
        if let Some(info) = self.collections.get(kind) {
            return *info;
        }
        let info = match crate::federation::fetch::signed_get_json(self.state, &uri).await {
            Ok(collection) => {
                let total = collection.get("totalItems").and_then(|total| {
                    total
                        .as_i64()
                        .or_else(|| total.as_f64().map(|total| total as i64))
                });
                let first_page = collection.get("first").is_some_and(is_present);
                (total, first_page)
            }
            Err(_) => (None, false),
        };
        self.collections.insert(kind, info);
        info
    }

    /// `moved_account`: the account `movedTo` names, fetched if unknown —
    /// unless it has moved on itself.
    async fn moved_account(&self, moved: &Value) -> Option<i64> {
        let uri = value_or_id_owned(moved)?;
        // An account redirecting to itself is no redirect.
        if uri == self.uri {
            return None;
        }
        if let Some(id) = crate::federation::local_uri::account(self.state, &uri).await {
            return Some(id);
        }
        fetch_remote_actor(self.state, &uri, true, Some(self.request_id))
            .await
            .ok()
            .flatten()
    }

    /// `process_tags`: the custom emojis the profile uses.
    async fn process_tags(&self, account: &Account, suspended: bool) {
        if suspended || self.skip_download().await {
            return;
        }
        let Some(domain) = account.domain.as_deref() else {
            return;
        };
        for tag in as_array(self.json.get("tag")) {
            if !equals_or_includes(tag.get("type"), "Emoji") {
                continue;
            }
            if let Err(error) = process_emoji(self.state, domain, tag).await {
                tracing::debug!(%error, "could not store a profile's custom emoji");
            }
        }
    }
}

/// A public key an actor publishes.
struct RemoteKey {
    kind: i32,
    pem: String,
    uri: String,
}

/// `process_emoji`.
async fn process_emoji(state: &AppState, domain: &str, tag: &Value) -> Result<()> {
    let Some(name) = tag
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.trim().is_empty())
    else {
        return Ok(());
    };
    let Some(image_url) = tag
        .get("icon")
        .and_then(|icon| icon.get("url"))
        .and_then(Value::as_str)
        .filter(|url| !url.trim().is_empty())
    else {
        return Ok(());
    };
    let shortcode = name.replace(':', "");
    // `CustomEmoji` validations: a shortcode of letters, digits and `_`, two
    // characters or more.
    if shortcode.len() < 2
        || shortcode.len() > 2048
        || !shortcode
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Ok(());
    }
    let uri = tag.get("id").and_then(Value::as_str);
    let updated = tag
        .get("updated")
        .and_then(Value::as_str)
        .and_then(|updated| chrono::DateTime::parse_from_rfc3339(updated).ok())
        .map(|updated| updated.naive_utc());
    let existing = sqlx::query!(
        "SELECT id, image_remote_url, updated_at FROM custom_emojis WHERE shortcode = $1 AND domain = $2",
        shortcode,
        domain,
    )
    .fetch_optional(&state.db)
    .await?;
    match existing {
        None => {
            sqlx::query!(
                r#"INSERT INTO custom_emojis (shortcode, domain, uri, image_remote_url, created_at, updated_at)
                   VALUES ($1, $2, $3, $4, now(), now())
                   ON CONFLICT (shortcode, domain) DO NOTHING"#,
                shortcode,
                domain,
                uri,
                image_url,
            )
            .execute(&state.db)
            .await?;
        }
        Some(emoji) => {
            let changed = emoji.image_remote_url.as_deref() != Some(image_url);
            if changed || updated.is_some_and(|updated| updated >= emoji.updated_at) {
                sqlx::query!(
                    r#"UPDATE custom_emojis
                       SET image_remote_url = $2,
                           image_file_name = CASE WHEN $3 THEN NULL ELSE image_file_name END,
                           updated_at = now()
                       WHERE id = $1"#,
                    emoji.id,
                    image_url,
                    changed,
                )
                .execute(&state.db)
                .await?;
            }
        }
    }
    Ok(())
}

/// `ActivityPub::FetchRemoteActorService#call`, and `FetchRemoteAccountService`
/// with `break_on_redirect`: fetch the actor at `uri` and process it.
pub async fn fetch_remote_actor(
    state: &AppState,
    uri: &str,
    break_on_redirect: bool,
    request_id: Option<&str>,
) -> Result<Option<i64>> {
    if crate::federation::moderation::domain_not_allowed(state, uri).await {
        return Ok(None);
    }
    if let Some(id) = local_account(state, uri).await {
        return Ok(Some(id));
    }
    let json = crate::federation::fetch::signed_get_json(state, uri).await?;
    process_fetched_actor(state, uri, &json, break_on_redirect, false, request_id).await
}

/// The checks `FetchRemoteActorService` makes of a document fetched or
/// handed over (`prefetched_body`) before processing it.
pub async fn process_fetched_actor(
    state: &AppState,
    uri: &str,
    json: &Value,
    break_on_redirect: bool,
    only_key: bool,
    request_id: Option<&str>,
) -> Result<Option<i64>> {
    let id = json.get("id").and_then(Value::as_str).unwrap_or_default();
    if crate::federation::portable::canonical(id) != crate::federation::portable::canonical(uri) {
        bail!("Error fetching actor JSON at {uri}");
    }
    if !equals_or_includes(
        json.get("@context"),
        "https://www.w3.org/ns/activitystreams",
    ) {
        bail!("Unsupported JSON-LD context for document {uri}");
    }
    if !crate::federation::fetch_resource::ACTOR_TYPES
        .iter()
        .any(|kind| equals_or_includes(json.get("type"), kind))
    {
        bail!("Unexpected object type for actor {uri}");
    }
    if break_on_redirect && json.get("movedTo").is_some_and(is_present) {
        bail!("Actor {uri} has moved");
    }
    let named = |key: &str| json.get(key).is_some_and(is_present);
    if !named("preferredUsername") && !named("webfinger") {
        bail!(
            "Actor {uri} has neither 'preferredUsername' nor `webfinger`, which is a requirement \
             for Mastodon compatibility"
        );
    }
    process(
        state,
        json,
        Options {
            only_key,
            request_id,
            ..Default::default()
        },
    )
    .await
}

/// `ActivityPub::TagManager#uri_to_actor` for a URI on this instance.
async fn local_account(state: &AppState, uri: &str) -> Option<i64> {
    let host = crate::federation::moderation::domain_of(uri)?;
    let ours = host.eq_ignore_ascii_case(&state.instance.domain)
        || state
            .instance
            .aliases
            .iter()
            .any(|alias| host.eq_ignore_ascii_case(alias));
    if !ours {
        return None;
    }
    crate::federation::local_uri::account(state, uri).await
}

/// `Account#possibly_stale?`.
pub fn possibly_stale(account: &Account) -> bool {
    account
        .last_webfingered_at
        .is_none_or(|at| at <= chrono::Utc::now().naive_utc() - STALE_THRESHOLD)
        || account.username.starts_with("! ")
}

/// `Account#needs_background_refresh?`.
fn needs_background_refresh(account: &Account) -> bool {
    !account.is_local()
        && (account
            .last_webfingered_at
            .is_none_or(|at| at <= chrono::Utc::now().naive_utc() - BACKGROUND_REFRESH_INTERVAL)
            || account.username.starts_with("! "))
}

/// `Account#schedule_refresh_if_stale!`: refresh an account that has not
/// been refreshed for a week, some time in the next six hours, once.
pub async fn schedule_refresh_if_stale(state: &AppState, account_id: i64) {
    let Ok(Some(account)) = find_by_id(state, account_id).await else {
        return;
    };
    if !needs_background_refresh(&account) {
        return;
    }
    // `lock: :until_executed, lock_ttl: 1.day`.
    let Some(lock) = crate::redis_lock::try_acquire(
        state,
        &format!("account_refresh:{account_id}"),
        24 * 60 * 60 * 1000,
    )
    .await
    else {
        return;
    };
    let state = state.clone();
    let delay = Duration::from_secs(rand::random_range(0..REFRESH_DEADLINE.as_secs()));
    crate::tenants::spawn(async move {
        tokio::time::sleep(delay).await;
        refresh(&state, account_id).await;
        drop(lock);
    });
}

/// `AccountRefreshWorker`: fetch a remote account again, if it is still due.
async fn refresh(state: &AppState, account_id: i64) {
    let Ok(Some(account)) = find_by_id(state, account_id).await else {
        return;
    };
    if !needs_background_refresh(&account) {
        return;
    }
    let Some(uri) = account.stored_uri() else {
        return;
    };
    if let Err(error) = fetch_remote_actor(state, uri, false, None).await {
        tracing::debug!(account_id, %error, "could not refresh a remote account");
    }
}

/// `create_account`. A blocked domain's new account starts out suspended or
/// limited from the time of the block.
async fn create_account(state: &AppState, username: &str, domain: &str, uri: &str) -> Result<i64> {
    // `validates :username, format: USERNAME_ONLY_RE, length: 2048`.
    if !crate::federation::webfinger::is_valid_username(username)
        || username.len() > USERNAME_LENGTH_HARD_LIMIT
    {
        bail!("Validation failed: Username is invalid ({username}@{domain})");
    }
    let first_of_domain = !sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM accounts WHERE domain = $1) AS "exists!""#,
        domain
    )
    .fetch_one(&state.db)
    .await?;
    let id = sqlx::query_scalar!(
        r#"INSERT INTO accounts (id, username, domain, uri, protocol, private_key, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, NULL, now(), now())
           RETURNING id"#,
        crate::snowflake::next_id(),
        username,
        domain,
        uri,
        PROTOCOL_ACTIVITYPUB,
    )
    .fetch_one(&state.db)
    .await?;
    crate::moderation::domain_block::apply_to_new_account(state, id, domain).await?;
    if first_of_domain {
        count_unique_subdomains(state, domain).await;
    }
    Ok(id)
}

/// `rename_account!`: take the handle WebFinger confirmed, from whichever
/// other account held it.
async fn rename_account(
    state: &AppState,
    account: &Account,
    username: &str,
    domain: &str,
    request_id: &str,
) -> Result<()> {
    if account.username == username && account.domain.as_deref() == Some(domain) {
        return Ok(());
    }
    let rename = || {
        sqlx::query!(
            "UPDATE accounts SET username = $2, domain = $3, updated_at = now() WHERE id = $1",
            account.id,
            username,
            domain,
        )
        .execute(&state.db)
    };
    if rename().await.is_ok() {
        return Ok(());
    }
    // `rename_conflicting_account!`.
    if let Some(conflicting) = find_remote(state, username, domain).await? {
        if !conflicting.is_local() && conflicting.uri != account.uri {
            crate::federation::handle::invalidate_conflicting_handle(
                state, account.id, username, domain,
            )
            .await
            .map_err(|error| anyhow!("{error:?}"))?;
            let state = state.clone();
            let request_id = request_id.to_owned();
            crate::tenants::spawn(async move {
                if let Some(uri) = conflicting.stored_uri() {
                    if let Err(error) =
                        fetch_remote_actor(&state, uri, false, Some(&request_id)).await
                    {
                        tracing::debug!(%error, "could not refresh an account whose handle was taken");
                    }
                }
            });
        }
    }
    rename().await?;
    tracing::info!(
        account_id = account.id,
        from = %account.username,
        to = %format!("{username}@{domain}"),
        "remote account changed handle"
    );
    Ok(())
}

/// `ActivityPub::PostUpgradeWorker`.
async fn after_protocol_change(state: &AppState, domain: &str) -> Result<()> {
    sqlx::query!(
        r#"UPDATE accounts SET last_webfingered_at = NULL
           WHERE domain = $1 AND protocol = 0 AND last_webfingered_at IS NOT NULL"#,
        domain,
    )
    .execute(&state.db)
    .await?;
    Ok(())
}

/// `all_public_keys_changed?`.
async fn all_public_keys_changed(
    state: &AppState,
    account_id: i64,
    old: &[String],
) -> Result<bool> {
    if old.is_empty() {
        return Ok(false);
    }
    let current: Vec<String> = sqlx::query_scalar!(
        r#"SELECT public_key FROM keypairs
           WHERE account_id = $1 AND NOT revoked AND (expires_at IS NULL OR expires_at > now())"#,
        account_id
    )
    .fetch_all(&state.db)
    .await?;
    Ok(!current.iter().any(|key| old.contains(key)))
}

/// `RefollowWorker`: an account whose identity changed is followed again by
/// the local accounts that followed it, keeping their settings.
fn refollow_later(state: &AppState, account_id: i64) {
    let state = state.clone();
    crate::tenants::spawn(async move {
        if let Err(error) = refollow(&state, account_id).await {
            tracing::warn!(account_id, %error, "RefollowWorker failed");
        }
    });
}

async fn refollow(state: &AppState, account_id: i64) -> Result<()> {
    use crate::api::mastodon::accounts::{follow, FollowOptions};

    let Some(target) = find_by_id(state, account_id).await? else {
        return Ok(());
    };
    if target.protocol != PROTOCOL_ACTIVITYPUB {
        return Ok(());
    }
    let follows = sqlx::query!(
        r#"SELECT f.id, f.account_id, f.show_reblogs, f.notify, f.languages
           FROM follows f JOIN accounts a ON a.id = f.account_id
           WHERE f.target_account_id = $1 AND a.domain IS NULL"#,
        account_id
    )
    .fetch_all(&state.db)
    .await?;
    for row in follows {
        // `follower.unfollow!(target_account)`: here only.
        sqlx::query!("DELETE FROM follows WHERE id = $1", row.id)
            .execute(&state.db)
            .await?;
        crate::counters::on_follow_removed(&state.db, row.account_id, account_id).await?;
        let Some(follower) = find_by_id(state, row.account_id).await? else {
            continue;
        };
        let options = FollowOptions {
            reblogs: Some(row.show_reblogs),
            notify: Some(row.notify),
            languages: row.languages,
            bypass_limit: true,
            ..Default::default()
        };
        if let Err(error) = follow(state, &follower, &target, options).await {
            tracing::debug!(
                account_id,
                follower = row.account_id,
                ?error,
                "could not re-follow"
            );
        }
    }
    Ok(())
}

/// `Account::Merging::ACCOUNT_MERGING_CLASSES`: every column that points at
/// an account, by table.
const MERGED_COLUMNS: &[(&str, &str)] = &[
    ("statuses", "account_id"),
    ("status_pins", "account_id"),
    ("media_attachments", "account_id"),
    ("polls", "account_id"),
    ("reports", "account_id"),
    ("tombstones", "account_id"),
    ("favourites", "account_id"),
    ("follows", "account_id"),
    ("follow_requests", "account_id"),
    ("blocks", "account_id"),
    ("mutes", "account_id"),
    ("account_moderation_notes", "account_id"),
    ("account_pins", "account_id"),
    ("account_stats", "account_id"),
    ("list_accounts", "account_id"),
    ("poll_votes", "account_id"),
    ("mentions", "account_id"),
    ("account_deletion_requests", "account_id"),
    ("account_notes", "account_id"),
    ("follow_recommendation_suppressions", "account_id"),
    ("appeals", "account_id"),
    ("tag_follows", "account_id"),
    ("quotes", "account_id"),
    ("collections", "account_id"),
    ("collection_items", "account_id"),
    ("notifications", "from_account_id"),
    ("notification_permissions", "from_account_id"),
    ("notification_requests", "from_account_id"),
    ("follows", "target_account_id"),
    ("follow_requests", "target_account_id"),
    ("blocks", "target_account_id"),
    ("mutes", "target_account_id"),
    ("account_moderation_notes", "target_account_id"),
    ("account_pins", "target_account_id"),
    ("account_notes", "target_account_id"),
    ("account_warnings", "target_account_id"),
    ("canonical_email_blocks", "reference_account_id"),
    ("severed_relationships", "local_account_id"),
    ("severed_relationships", "remote_account_id"),
    ("quotes", "quoted_account_id"),
];

/// `process_duplicate_accounts!` and `AccountMergingWorker`: other accounts
/// with the same `uri` are merged into this one and removed.
async fn process_duplicate_accounts(state: &AppState, account: &Account) -> Result<()> {
    let duplicates: Vec<i64> = sqlx::query_scalar!(
        "SELECT id FROM accounts WHERE uri = $1 AND id <> $2 AND domain IS NOT NULL",
        account.uri,
        account.id,
    )
    .fetch_all(&state.db)
    .await?;
    if duplicates.is_empty() {
        return Ok(());
    }
    let state = state.clone();
    let id = account.id;
    crate::tenants::spawn(async move {
        for duplicate in duplicates {
            if let Err(error) = merge_with(&state, id, duplicate).await {
                tracing::warn!(account_id = id, duplicate, %error, "AccountMergingWorker failed");
            }
        }
    });
    Ok(())
}

/// `Account#merge_with!`, then `duplicate.destroy`. A row that would collide
/// with one the account already has stays where it is, and goes with the
/// duplicate.
async fn merge_with(state: &AppState, account_id: i64, duplicate: i64) -> Result<()> {
    for (table, column) in MERGED_COLUMNS {
        let moved = sqlx::query(&format!(
            "UPDATE {table} SET {column} = $1 WHERE {column} = $2"
        ))
        .bind(account_id)
        .bind(duplicate)
        .execute(&state.db)
        .await;
        if moved.is_ok() {
            continue;
        }
        let ids: Vec<i64> =
            sqlx::query_scalar(&format!("SELECT id FROM {table} WHERE {column} = $1"))
                .bind(duplicate)
                .fetch_all(&state.db)
                .await?;
        for row in ids {
            let _ = sqlx::query(&format!("UPDATE {table} SET {column} = $1 WHERE id = $2"))
                .bind(account_id)
                .bind(row)
                .execute(&state.db)
                .await;
        }
    }
    sqlx::query!("DELETE FROM accounts WHERE id = $1", duplicate)
        .execute(&state.db)
        .await?;
    Ok(())
}

/// The first account seen from a domain counts it towards its registrable
/// domain's subdomains (`DomainMaterializable#count_unique_subdomains!`).
async fn count_unique_subdomains(state: &AppState, domain: &str) {
    let key = state.redis_keys.key(format!(
        "unique_subdomains_for:{}",
        registrable_domain(domain)
    ));
    let mut redis = state.redis.clone();
    let _: redis::RedisResult<()> = redis::pipe()
        .cmd("PFADD")
        .arg(&key)
        .arg(domain)
        .ignore()
        .cmd("EXPIRE")
        .arg(&key)
        .arg(60)
        .ignore()
        .query_async(&mut redis)
        .await;
}

/// The limits on discovering new accounts: no more than ten new subdomains
/// of one domain a minute, and no more than 400 new accounts for one
/// request.
async fn over_discovery_limits(state: &AppState, domain: &str, request_id: &str) -> bool {
    let mut redis = state.redis.clone();
    let subdomains = state.redis_keys.key(format!(
        "unique_subdomains_for:{}",
        registrable_domain(domain)
    ));
    let count: i64 = redis::cmd("PFCOUNT")
        .arg(&subdomains)
        .query_async(&mut redis)
        .await
        .unwrap_or(0);
    if count >= SUBDOMAINS_RATELIMIT {
        return true;
    }
    let discoveries_key = state
        .redis_keys
        .key(format!("discovery_per_request:{request_id}"));
    let discoveries: redis::RedisResult<(i64,)> = redis::pipe()
        .cmd("INCRBY")
        .arg(&discoveries_key)
        .arg(1)
        .cmd("EXPIRE")
        .arg(&discoveries_key)
        .arg(5 * 60)
        .ignore()
        .query_async(&mut redis)
        .await;
    discoveries.is_ok_and(|(discoveries,)| discoveries > DISCOVERIES_PER_REQUEST)
}

/// `PublicSuffix.domain(domain, ignore_private: true)`: the domain under the
/// longest ICANN suffix, or the last two labels under an unknown one.
pub fn registrable_domain(domain: &str) -> String {
    let host = domain
        .split(':')
        .next()
        .unwrap_or(domain)
        .trim_end_matches('.');
    let labels: Vec<&str> = host.split('.').collect();
    for start in 1..labels.len() {
        let candidate = labels[start..].join(".");
        let icann = psl::suffix(candidate.as_bytes()).is_some_and(|suffix| {
            suffix.typ() == Some(psl::Type::Icann) && suffix.as_bytes() == candidate.as_bytes()
        });
        if icann {
            return labels[start - 1..].join(".");
        }
    }
    let keep = labels.len().min(2);
    labels[labels.len() - keep..].join(".")
}

async fn find_by_id(state: &AppState, id: i64) -> sqlx::Result<Option<Account>> {
    sqlx::query_as!(Account, "SELECT * FROM accounts WHERE id = $1", id)
        .fetch_optional(&state.db)
        .await
}

async fn load(state: &AppState, id: i64) -> Result<Account> {
    find_by_id(state, id)
        .await?
        .ok_or_else(|| anyhow!("account {id} vanished while it was processed"))
}

/// `Account.remote.find_by(uri:)`.
async fn find_by_uri(state: &AppState, uri: &str) -> sqlx::Result<Option<Account>> {
    sqlx::query_as!(
        Account,
        "SELECT * FROM accounts WHERE uri = $1 AND domain IS NOT NULL ORDER BY id LIMIT 1",
        uri
    )
    .fetch_optional(&state.db)
    .await
}

/// `Account.find_remote(username, domain)`.
async fn find_remote(
    state: &AppState,
    username: &str,
    domain: &str,
) -> sqlx::Result<Option<Account>> {
    if domain == "handle.invalid" {
        return Ok(None);
    }
    sqlx::query_as!(
        Account,
        r#"SELECT * FROM accounts
           WHERE lower(username) = lower($1) AND lower(domain) = lower($2)
           ORDER BY id LIMIT 1"#,
        username,
        domain,
    )
    .fetch_optional(&state.db)
    .await
}

/// `JsonLdHelper#unsupported_uri_scheme?`.
fn unsupported_uri_scheme(uri: &str) -> bool {
    !(uri.starts_with("http://") || uri.starts_with("https://"))
}

/// `TagManager#normalize_domain`: stripped, without a trailing slash, in
/// lower case and ASCII, keeping a port if it has one.
pub fn normalize_domain(domain: &str) -> String {
    let domain = domain.trim().trim_end_matches('/');
    match url::Url::parse(&format!("http://{domain}/")) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_owned(),
            _ => domain.to_lowercase(),
        },
        Err(_) => domain.to_lowercase(),
    }
}

/// Ruby's `String#squish`.
fn squish(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `str[0...limit]`, in characters as Ruby counts them.
fn truncate_chars(value: &str, limit: usize) -> String {
    match value.char_indices().nth(limit) {
        Some((end, _)) => value[..end].to_owned(),
        None => value.to_owned(),
    }
}

/// Whether Ruby holds a JSON value true: anything but `null` and `false`.
fn truthy(value: Option<&Value>) -> bool {
    !matches!(value, None | Some(Value::Null) | Some(Value::Bool(false)))
}

/// `value || false`, as Active Record casts it into a boolean column.
fn active_record_boolean(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(number)) => number.as_f64() != Some(0.0),
        Some(Value::String(text)) => !matches!(
            text.as_str(),
            "" | "0" | "f" | "F" | "false" | "FALSE" | "off" | "OFF"
        ),
        Some(_) => true,
    }
}

/// `JsonLdHelper#value_or_id`, owned.
fn value_or_id_owned(value: &Value) -> Option<String> {
    match value {
        Value::String(id) => Some(id.clone()),
        Value::Object(object) => object.get("id").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

/// `alsoKnownAs` as `ProcessAccountService` keeps it: at most
/// `Account::ALSO_KNOWN_AS_HARD_LIMIT` ids, embedded objects reduced to theirs.
pub fn also_known_as_of(actor: &Value) -> Vec<String> {
    const ALSO_KNOWN_AS_HARD_LIMIT: usize = 256;
    as_array(actor.get("alsoKnownAs"))
        .into_iter()
        .take(ALSO_KNOWN_AS_HARD_LIMIT)
        .filter_map(value_or_id_owned)
        .collect()
}

/// `JsonLdHelper#as_array`.
fn as_array(value: Option<&Value>) -> Vec<&Value> {
    match value {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(item) => vec![item],
    }
}

/// `JsonLdHelper#first_of_value`.
fn first_of_value(value: &Value) -> Option<&Value> {
    match value {
        Value::Array(items) => items.first(),
        value => Some(value),
    }
}

/// `JsonLdHelper#equals_or_includes?`.
fn equals_or_includes(haystack: Option<&Value>, needle: &str) -> bool {
    match haystack {
        Some(Value::Array(items)) => items.iter().any(|item| item.as_str() == Some(needle)),
        Some(Value::String(value)) => value == needle,
        _ => false,
    }
}

/// `JsonLdHelper#first_lang_string`: an `xsd:string` or `rdf:langString`
/// property, or the first value of its language map.
fn first_lang_string<'a>(json: &'a Value, name: &str) -> Option<&'a str> {
    match json.get(name).filter(|value| truthy(Some(value))) {
        Some(value) => match first_of_value(value)? {
            Value::String(text) => Some(text),
            Value::Object(object) => object.get("@value").and_then(Value::as_str),
            _ => None,
        },
        None => json
            .get(format!("{name}Map"))
            .and_then(Value::as_object)
            .and_then(|map| map.values().next())
            .and_then(Value::as_str),
    }
}

/// `JsonLdHelper#url_to_href`: a link's `href`, preferring one of
/// `preferred_type` among several.
fn url_to_href(value: &Value, preferred_type: Option<&str>) -> Option<String> {
    let wrapped;
    let value = match value {
        Value::Object(_) => {
            wrapped = Value::Array(vec![value.clone()]);
            &wrapped
        }
        value => value,
    };
    let single = match value {
        Value::Array(links) if !links.first().is_some_and(Value::is_string) => {
            links.iter().find(|link| {
                preferred_type.is_none_or(|preferred| {
                    let mime = link
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .filter(|mime| !mime.trim().is_empty())
                        .unwrap_or("text/html");
                    mime == preferred
                })
            })
        }
        Value::Array(links) => links.first(),
        value => Some(value),
    }?;
    match single {
        Value::String(href) => Some(href.clone()),
        link => link.get("href").and_then(Value::as_str).map(str::to_owned),
    }
}

/// `valid_collection_uri`: the first of several, the id of an embedded
/// object, and only an `http(s)` URL with a host — or `''`. A portable
/// actor's collections are `ap` ids, kept as they are.
fn valid_collection_uri(value: Option<&Value>, allow_ap: bool) -> String {
    let mut value = value;
    if let Some(Value::Array(items)) = value {
        value = items.first();
    }
    if let Some(Value::Object(object)) = value {
        value = object.get("id");
    }
    let Some(Value::String(uri)) = value else {
        return String::new();
    };
    if allow_ap && ojak::portable::ApUri::parse(uri).is_some() {
        return uri.clone();
    }
    match url::Url::parse(uri) {
        Ok(parsed)
            if matches!(parsed.scheme(), "http" | "https")
                && parsed.host_str().is_some_and(|host| !host.is_empty()) =>
        {
            uri.clone()
        }
        _ => String::new(),
    }
}

/// `property_values`: the profile fields, `PropertyValue`s with their `name`
/// and `value` as the actor wrote them.
fn property_values(json: &Value) -> Option<Value> {
    let attachments = json.get("attachment")?.as_array()?;
    Some(Value::Array(
        attachments
            .iter()
            .filter(|attachment| {
                attachment.get("type").and_then(Value::as_str) == Some("PropertyValue")
            })
            .take(MAX_PROFILE_FIELDS)
            .map(|attachment| {
                let mut field = Map::new();
                for key in ["name", "value"] {
                    if let Some(value) = attachment.get(key) {
                        field.insert(key.to_owned(), value.clone());
                    }
                }
                Value::Object(field)
            })
            .collect(),
    ))
}

/// `Multibase.decode_key_to_pem`: a `Multikey` as a key type and its PEM.
fn key_from_multikey(value: Option<&Value>) -> Option<(i32, String)> {
    use base64::Engine as _;
    use ojak::sig::integrity::PublicKey;

    const ED25519_PUB_DER_HEADER: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    const ML_DSA_44_PUB_DER_HEADER: [u8; 22] = [
        0x30, 0x82, 0x05, 0x32, 0x30, 0x0b, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04,
        0x03, 0x11, 0x03, 0x82, 0x05, 0x21, 0x00,
    ];
    let base64 = base64::engine::general_purpose::STANDARD;
    match ojak::sig::integrity::decode_multikey(value?.as_str()?).ok()? {
        PublicKey::Ed25519(key) => {
            let der = [ED25519_PUB_DER_HEADER.as_slice(), key.as_slice()].concat();
            Some((
                key_type::ED25519,
                format!(
                    "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
                    base64.encode(der)
                ),
            ))
        }
        PublicKey::MlDsa44(key) if key.len() == 1312 => {
            let der = [ML_DSA_44_PUB_DER_HEADER.as_slice(), &key].concat();
            let encoded = base64.encode(der);
            let lines: Vec<&str> = encoded
                .as_bytes()
                .chunks(64)
                .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
                .collect();
            Some((
                key_type::ML_DSA_44,
                format!(
                    "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
                    lines.join("\n")
                ),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collection_uris_are_validated_like_mastodon() {
        assert_eq!(
            valid_collection_uri(Some(&json!("https://a.example/inbox")), false),
            "https://a.example/inbox"
        );
        assert_eq!(
            valid_collection_uri(
                Some(&json!(["https://a.example/1", "https://a.example/2"])),
                false
            ),
            "https://a.example/1"
        );
        assert_eq!(
            valid_collection_uri(Some(&json!({"id": "https://a.example/c"})), false),
            "https://a.example/c"
        );
        assert_eq!(
            valid_collection_uri(Some(&json!("ftp://a.example/c")), false),
            ""
        );
        assert_eq!(valid_collection_uri(Some(&json!(5)), false), "");
        assert_eq!(valid_collection_uri(None, false), "");
    }

    #[test]
    fn url_to_href_prefers_html() {
        assert_eq!(
            url_to_href(
                &json!([
                    {"type": "Link", "mimeType": "application/activity+json", "href": "https://a.example/ap"},
                    {"type": "Link", "href": "https://a.example/@alice"}
                ]),
                Some("text/html")
            ),
            Some("https://a.example/@alice".into())
        );
        assert_eq!(
            url_to_href(
                &json!(["https://a.example/1", "https://a.example/2"]),
                Some("text/html")
            ),
            Some("https://a.example/1".into())
        );
        assert_eq!(
            url_to_href(&json!({"href": "https://a.example/x"}), Some("text/html")),
            Some("https://a.example/x".into())
        );
    }

    #[test]
    fn truncation_counts_characters() {
        assert_eq!(truncate_chars("가나다라", 2), "가나");
        assert_eq!(truncate_chars("ab", 5), "ab");
    }

    #[test]
    fn booleans_are_cast_like_active_record() {
        assert!(active_record_boolean(Some(&json!(true))));
        assert!(!active_record_boolean(Some(&json!("false"))));
        assert!(active_record_boolean(Some(&json!("yes"))));
        assert!(!active_record_boolean(Some(&json!(0))));
        assert!(!active_record_boolean(None));
        assert!(truthy(Some(&json!("false"))));
        assert!(!truthy(Some(&json!(null))));
    }

    #[test]
    fn property_values_keep_name_and_value_only() {
        let json = json!({"attachment": [
            {"type": "PropertyValue", "name": "Web", "value": "<a href=\"https://a.example\">a</a>", "extra": 1},
            {"type": "Note", "name": "skip"},
            {"type": "PropertyValue", "name": "only name"}
        ]});
        assert_eq!(
            property_values(&json),
            Some(json!([
                {"name": "Web", "value": "<a href=\"https://a.example\">a</a>"},
                {"name": "only name"}
            ]))
        );
        assert_eq!(
            property_values(&json!({"attachment": {"type": "PropertyValue"}})),
            None
        );
    }

    #[test]
    fn registrable_domains_ignore_private_suffixes() {
        assert_eq!(registrable_domain("a.b.example.com"), "example.com");
        assert_eq!(registrable_domain("social.example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("alice.github.io"), "github.io");
        assert_eq!(registrable_domain("example.com"), "example.com");
    }

    #[test]
    fn domains_are_normalized() {
        assert_eq!(normalize_domain(" Example.COM/ "), "example.com");
        assert_eq!(normalize_domain("bücher.example"), "xn--bcher-kva.example");
        assert_eq!(normalize_domain("127.0.0.1:3000"), "127.0.0.1:3000");
    }

    #[test]
    fn ed25519_multikeys_become_pem() {
        // The FEP-521a example key.
        let (kind, pem) = key_from_multikey(Some(&json!(
            "z6MkrJVnaZkeFzdQyMZu1cgjg7k1pZZ6pvBQ7XJPt4swbTQ2"
        )))
        .unwrap();
        assert_eq!(kind, key_type::ED25519);
        assert!(pem.starts_with("-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEA"));
        assert!(pem.ends_with("\n-----END PUBLIC KEY-----\n"));
    }
}
