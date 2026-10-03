//! Preview cards: Mastodon's `FetchLinkCardService`, which picks the link a
//! post is about, fetches it, asks its oEmbed endpoint or reads its tags, and
//! keeps what it learns in `preview_cards`, joined to the post through
//! `preview_cards_statuses`.

pub mod extract;
pub mod image;
pub mod oembed;

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::Duration;

use chrono::NaiveDateTime;
use regex::Regex;
use serde_json::Value;
use url::Url;

pub(crate) use extract::is_blank;
use extract::{TYPE_LINK, TYPE_PHOTO, TYPE_RICH, TYPE_VIDEO};

use crate::state::AppState;

/// `Request::ClientLimit#truncated_body`'s and `#body_with_limit`'s default.
pub const BODY_LIMIT: usize = 1024 * 1024;

/// `Request::TIMEOUT[:read_deadline]`.
pub const TIMEOUT: Duration = Duration::from_secs(30);

/// `PreviewCard::URL_CHARACTER_LIMIT`.
const URL_CHARACTER_LIMIT: usize = 2692;

/// `ActivityPub::Activity::Create::DISTRIBUTE_DELAY`: how far a remote post's
/// crawl is spread out, so that a popular link is not fetched by every
/// server at the same moment.
const CRAWL_DELAY: Duration = Duration::from_secs(60);

/// Something `FetchLinkCardService#call` rescues: the fetch is abandoned and
/// no card is attached, not even one already known.
#[derive(Debug)]
pub struct Abort;

// ── Scheduling ─────────────────────────────────────────────────────────────

/// `LinkCrawlWorker.perform_async(status.id)`, for a local post just written
/// or edited: the card comes from the first link in its text.
pub async fn crawl(state: &AppState, status_id: i64) {
    crate::jobs::push(
        state,
        LinkCrawlWorker {
            status_id,
            url: None,
        },
    )
    .await;
}

/// `LinkCrawlWorker.perform_in(rand(DISTRIBUTE_DELAY), status.id, link)`, for
/// a remote post: the card comes from its FEP-8967 `Link` attachment, or else
/// from the first link in its content.
pub async fn crawl_later(state: &AppState, status_id: i64, link: Option<String>) {
    let delay = Duration::from_millis(rand::random_range(0..CRAWL_DELAY.as_millis() as u64));
    crate::jobs::push_in(
        state,
        delay,
        LinkCrawlWorker {
            status_id,
            url: link,
        },
    )
    .await;
}

/// `LinkCrawlWorker`.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct LinkCrawlWorker {
    pub status_id: i64,
    #[serde(default)]
    pub url: Option<String>,
}

impl crate::jobs::Job for LinkCrawlWorker {
    const KIND: &'static str = "LinkCrawlWorker";
    const OPTIONS: crate::jobs::Options = crate::jobs::Options::DEFAULT
        .queue(crate::jobs::Queue::Pull)
        .retry(0);

    async fn perform(self, state: &AppState) -> anyhow::Result<()> {
        fetch_link_card(state, self.status_id, self.url).await;
        Ok(())
    }
}

/// `Status#reset_preview_card!`: forget the card, before an edit fetches a
/// new one.
pub async fn reset(state: &AppState, status_id: i64) {
    let _ = sqlx::query!(
        "DELETE FROM preview_cards_statuses WHERE status_id = $1",
        status_id
    )
    .execute(&state.db)
    .await;
}

// ── The service ────────────────────────────────────────────────────────────

struct StatusRow {
    text: String,
    local: bool,
    has_card: bool,
    with_media: bool,
    with_quote: bool,
}

async fn load_status(state: &AppState, status_id: i64) -> Option<StatusRow> {
    sqlx::query_as!(
        StatusRow,
        r#"SELECT s.text,
                  (COALESCE(s.local, false) OR s.uri IS NULL) AS "local!",
                  EXISTS (SELECT 1 FROM preview_cards_statuses p WHERE p.status_id = s.id) AS "has_card!",
                  EXISTS (SELECT 1 FROM media_attachments m
                          WHERE m.status_id = s.id
                            AND (s.ordered_media_attachment_ids IS NULL
                                 OR m.id = ANY(s.ordered_media_attachment_ids))) AS "with_media!",
                  EXISTS (SELECT 1 FROM quotes q WHERE q.status_id = s.id) AS "with_quote!"
           FROM statuses s
           WHERE s.id = $1 AND s.deleted_at IS NULL"#,
        status_id,
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
}

/// `FetchLinkCardService#call(status, original_url)`. Returns the card
/// attached, if one was.
pub async fn fetch_link_card(
    state: &AppState,
    status_id: i64,
    original_url: Option<String>,
) -> Option<i64> {
    let status = load_status(state, status_id).await?;
    let original_url = match original_url {
        Some(url) => url,
        None => parse_urls(state, status_id, &status).await?,
    };
    if status.has_card || status.with_media || status.with_quote {
        return None;
    }

    let card = {
        let _lock = crate::redis_lock::try_acquire(
            state,
            &format!("lock:fetch:{original_url}"),
            crate::redis_lock::DEFAULT_TTL_MS,
        )
        .await?;
        let card = Card::find(state, &original_url).await;
        let stale = card.as_ref().is_none_or(|c| {
            c.updated_at
                .is_some_and(|at| at <= chrono::Utc::now().naive_utc() - chrono::Duration::weeks(2))
                || c.missing_image()
        });
        if stale {
            let mut service = Service {
                state,
                url: original_url.clone(),
                card: card.unwrap_or_else(|| Card::new(&original_url)),
                html: None,
            };
            match service.process_url().await {
                Ok(()) => service.card,
                Err(Abort) => {
                    tracing::debug!(url = %original_url, "could not fetch a link card");
                    return None;
                }
            }
        } else {
            card?
        }
    };

    let card_id = card.id?;
    attach_card(state, status_id, card_id, &original_url).await?;
    Some(card_id)
}

/// `attach_card`.
async fn attach_card(
    state: &AppState,
    status_id: i64,
    card_id: i64,
    original_url: &str,
) -> Option<()> {
    let _lock = crate::redis_lock::try_acquire(
        state,
        &format!("lock:attach_card:{status_id}"),
        crate::redis_lock::DEFAULT_TTL_MS,
    )
    .await?;
    let has_card: bool = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM preview_cards_statuses WHERE status_id = $1) AS "e!""#,
        status_id
    )
    .fetch_one(&state.db)
    .await
    .ok()?;
    if has_card {
        return None;
    }
    sqlx::query!(
        "INSERT INTO preview_cards_statuses (status_id, preview_card_id, url) VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
        status_id,
        card_id,
        original_url,
    )
    .execute(&state.db)
    .await
    .ok()?;
    crate::trends::register_links(state, status_id).await;
    Some(())
}

struct Service<'a> {
    state: &'a AppState,
    /// `@url`: the URL being fetched, and after the page is fetched, the
    /// last URL in its redirect chain.
    url: String,
    card: Card,
    /// `@html`, once fetched: `None` inside when there was no HTML to read.
    html: Option<Option<String>>,
}

impl Service<'_> {
    async fn process_url(&mut self) -> Result<(), Abort> {
        if !self.attempt_oembed().await? {
            self.attempt_opengraph().await?;
        }
        Ok(())
    }

    /// `html`: the page, if it answers 200 with `text/html`.
    async fn html(&mut self) -> Result<Option<String>, Abort> {
        if let Some(html) = &self.html {
            return Ok(html.clone());
        }
        let url = Url::parse(&self.url).map_err(|_| Abort)?;
        let response = self
            .state
            .fetch
            .request(reqwest::Method::GET, &url)
            .map_err(|_| Abort)?
            .header(reqwest::header::ACCEPT, "text/html")
            .header(
                reqwest::header::ACCEPT_LANGUAGE,
                format!("{}, *;q=0.5", crate::api::mastodon::DEFAULT_LOCALE),
            )
            .header(
                reqwest::header::USER_AGENT,
                format!("{} Bot", crate::version::USER_AGENT),
            )
            .timeout(TIMEOUT)
            .send()
            .await
            .map_err(|_| Abort)?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<mime::Mime>().ok());
        let html = if response.status() == reqwest::StatusCode::OK
            && content_type.as_ref().map(mime::Mime::essence_str) == Some("text/html")
        {
            // Redirects are followed, and the card is kept for where they
            // ended rather than for a link shortener on the way.
            let final_url = response.url().to_string();
            if self.card.url != final_url {
                self.card = Card::find(self.state, &final_url)
                    .await
                    .unwrap_or_else(|| Card::new(&final_url));
            }
            self.url = final_url;
            let charset = content_type
                .as_ref()
                .and_then(|m| m.get_param(mime::CHARSET))
                .map(|c| c.as_str().to_owned());
            let body = read_body(response, BODY_LIMIT, true).await?;
            Some(extract::decode_html(&body, charset.as_deref()))
        } else {
            None
        };
        self.html = Some(html.clone());
        Ok(html)
    }

    /// `attempt_oembed`: whether a card was saved from the page's oEmbed.
    async fn attempt_oembed(&mut self) -> Result<bool, Abort> {
        let domain = Url::parse(&self.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        let mut service = oembed::OEmbed::default();
        let mut embed = None;
        if let Some(cached) = oembed::cached_endpoint(self.state, &domain).await {
            embed = service.call_cached(self.state, &self.url, &cached).await?;
        }
        if embed.is_none() {
            if let Some(html) = self.html().await? {
                embed = service.call_discover(self.state, &self.url, &html).await?;
            }
        }
        let Some(embed) = embed else {
            return Ok(false);
        };
        let base = service
            .endpoint_url
            .as_deref()
            .and_then(|e| Url::parse(e).ok())
            .ok_or(Abort)?;
        let join = |key: &str| -> Result<Option<String>, Abort> {
            match ruby_string(embed.get(key)).filter(|s| !is_blank(s)) {
                Some(relative) => base
                    .join(&relative)
                    .map(|u| Some(u.to_string()))
                    .map_err(|_| Abort),
                None => Ok(None),
            }
        };
        let dimension = |key: &str| -> i32 {
            match embed.get(key) {
                Some(Value::Number(n)) => n
                    .as_i64()
                    .or_else(|| n.as_f64().map(|f| f as i64))
                    .map(|n| n.clamp(i32::MIN as i64, i32::MAX as i64) as i32)
                    .unwrap_or(0),
                Some(Value::String(s)) => extract::ruby_to_i(s),
                _ => 0,
            }
        };

        // A type the enum does not know raises, abandoning the whole fetch.
        let card_type = match ruby_string(embed.get("type")).as_deref() {
            Some("link") => TYPE_LINK,
            Some("photo") => TYPE_PHOTO,
            Some("video") => TYPE_VIDEO,
            Some("rich") => TYPE_RICH,
            _ => return Err(Abort),
        };
        let mut card = self.card.clone();
        card.card_type = card_type;
        card.title = ruby_string(embed.get("title")).unwrap_or_default();
        card.author_name = ruby_string(embed.get("author_name")).unwrap_or_default();
        card.author_url = join("author_url")?.unwrap_or_default();
        card.provider_name = ruby_string(embed.get("provider_name")).unwrap_or_default();
        card.provider_url = join("provider_url")?.unwrap_or_default();
        card.width = 0;
        card.height = 0;

        match card_type {
            TYPE_LINK => {
                if let Some(thumbnail) = join("thumbnail_url")? {
                    card.assign_image(self.state, Some(&thumbnail)).await;
                }
            }
            TYPE_PHOTO => {
                let Some(photo) = join("url")? else {
                    return Ok(false);
                };
                card.embed_url = photo.clone();
                card.assign_image(self.state, Some(&photo)).await;
                card.width = dimension("width");
                card.height = dimension("height");
            }
            TYPE_VIDEO => {
                card.width = dimension("width");
                card.height = dimension("height");
                card.html = sanitize_oembed(&ruby_string(embed.get("html")).unwrap_or_default());
                if let Some(thumbnail) = join("thumbnail_url")? {
                    card.assign_image(self.state, Some(&thumbnail)).await;
                }
            }
            // Most providers rely on <script> tags, which is a no-no.
            _ => return Ok(false),
        }

        card.save_with_optional_image(self.state).await?;
        self.card = card;
        Ok(true)
    }

    /// `attempt_opengraph`.
    async fn attempt_opengraph(&mut self) -> Result<(), Abort> {
        let Some(html) = self.html().await? else {
            return Ok(());
        };
        let page_url = Url::parse(&self.url).map_err(|_| Abort)?;
        let details = extract::extract(&page_url, &html);
        let domain = Url::parse(&details.canonical_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        let provider_trendable = provider_trendable(self.state, &domain).await;
        let linked_account = match details.author_account.as_deref() {
            Some(handle) if !is_blank(handle) => resolve_account(self.state, handle).await,
            _ => None,
        };

        if details.canonical_url != self.card.url {
            self.card = Card::find(self.state, &details.canonical_url)
                .await
                .unwrap_or_else(|| Card::new(&details.canonical_url));
        }
        let card = &mut self.card;
        card.title = details.title;
        card.description = details.description;
        card.assign_image(self.state, details.image.as_deref())
            .await;
        card.image_description = details.image_description;
        card.card_type = details.card_type;
        card.link_type = Some(details.link_type);
        card.width = details.width;
        card.height = details.height;
        card.html = details.html;
        card.provider_name = details.provider_name;
        card.provider_url = details.provider_url;
        card.author_name = details.author_name;
        card.author_url = details.author_url;
        card.embed_url = details.embed_url;
        card.language = details.language;
        card.published_at = details.published_at;

        if let Some(account) = linked_account {
            // There is an overlap in the two conditions when the provider is
            // trendable, on purpose, to give people a heads-up before that
            // condition goes away.
            let attributable = can_be_attributed_from(&account.attribution_domains, &domain);
            if attributable || provider_trendable {
                card.author_account_id = Some(account.id);
            }
            if account.local && !attributable {
                card.unverified_author_account_id = Some(account.id);
            }
        }

        if !(is_blank(&card.title) && is_blank(&card.html)) {
            card.save_with_optional_image(self.state).await?;
        }
        Ok(())
    }
}

/// Read a response's body, at most `limit` bytes of it: cut off after the
/// limit when `truncate` (`truncated_body`), refused when not
/// (`body_with_limit`).
pub(crate) async fn read_body(
    mut response: reqwest::Response,
    limit: usize,
    truncate: bool,
) -> Result<Vec<u8>, Abort> {
    if !truncate && response.content_length().is_some_and(|l| l > limit as u64) {
        return Err(Abort);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Abort)? {
        body.extend_from_slice(&chunk);
        if body.len() > limit {
            if truncate {
                break;
            }
            return Err(Abort);
        }
    }
    Ok(body)
}

/// `present?` for a JSON value.
pub(crate) fn present(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(s)) => !is_blank(s),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        _ => true,
    }
}

/// A JSON value as a string attribute receives it.
fn ruby_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

// ── Choosing the link ──────────────────────────────────────────────────────

/// `parse_urls`: the first link in the post that is neither this instance's
/// nor a hashtag or mention.
async fn parse_urls(state: &AppState, status_id: i64, status: &StatusRow) -> Option<String> {
    let candidates: Vec<Url> = if status.local {
        local_urls(&status.text)
            .into_iter()
            .filter_map(|u| Url::parse(&u).ok())
            .collect()
    } else {
        let mentions = mention_urls(state, status_id).await;
        remote_links(&status.text, &mentions)
            .into_iter()
            .filter_map(|u| Url::parse(&u).ok())
            .collect()
    };
    candidates
        .into_iter()
        .find(|u| !bad_url(state, u))
        .map(|u| u.to_string())
}

/// `bad_url?`: no host, this instance's own, or not HTTP(S).
fn bad_url(state: &AppState, url: &Url) -> bool {
    let Some(host) = url.host_str().filter(|h| !h.is_empty()) else {
        return true;
    };
    let host = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let local = std::iter::once(&state.instance.domain)
        .chain(state.instance.aliases.iter())
        .any(|d| d.eq_ignore_ascii_case(&host));
    local || !matches!(url.scheme(), "http" | "https")
}

/// The URLs `ActivityPub::TagManager#url_for` gives the post's mentioned
/// accounts.
async fn mention_urls(state: &AppState, status_id: i64) -> HashSet<String> {
    sqlx::query!(
        r#"SELECT a.domain, a.username, a.url, a.uri
           FROM mentions m JOIN accounts a ON a.id = m.account_id
           WHERE m.status_id = $1"#,
        status_id
    )
    .fetch_all(&state.db)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|a| match a.domain {
        None => format!("https://{}/@{}", state.instance.domain, a.username),
        Some(_) => a.url.or(a.uri).unwrap_or_default(),
    })
    .collect()
}

/// A remote post's links: the `href` of each `<a>` that is not a hashtag or
/// mention (`skip_link?`).
fn remote_links(html: &str, mention_urls: &HashSet<String>) -> Vec<String> {
    static A: LazyLock<scraper::Selector> =
        LazyLock::new(|| scraper::Selector::parse("a").unwrap());
    static MICROFORMAT: LazyLock<Regex> = LazyLock::new(|| Regex::new("u-url|h-card").unwrap());
    let document = scraper::Html::parse_fragment(html);
    document
        .select(&A)
        .filter_map(|a| {
            let a = a.value();
            if a.attr("rel").is_some_and(|rel| rel.contains("tag"))
                || a.attr("class").is_some_and(|c| MICROFORMAT.is_match(c))
            {
                return None;
            }
            let href = a.attr("href")?;
            (!mention_urls.contains(href)).then(|| href.to_owned())
        })
        .collect()
}

/// `FetchLinkCardService::URL_PATTERN`: the URLs in a local post's text,
/// found with twitter-text's URL expression as Mastodon amends it, its
/// top-level domains twitter-text 3.1.0's lists.
fn local_urls(text: &str) -> Vec<String> {
    static URL_PATTERN: LazyLock<fancy_regex::Regex> = LazyLock::new(|| {
        let dvc = r"[^\x00-\x2F\x3A-\x40\x5B-\x60\x7B-\x7F\x{85}\x{A0}\x{1680}\x{180E}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FFFE}\x{FEFF}\x{FFFF}]";
        let subdomain = format!(r"(?:(?:{dvc}(?:[_-]|{dvc})*)?{dvc}\.)");
        let domain_name = format!(r"(?:(?:{dvc}(?:-|{dvc})*)?{dvc}\.)");
        let tld = crate::formatter::extractor::tld_pattern();
        let domain = format!("(?:{subdomain}*{domain_name}{tld})");
        let general = r"[^\s<>()?]";
        let balanced = format!(r"\((?:{general}+|(?:{general}*\({general}+\){general}*))\)");
        let ending = format!(r#"(?:[^\s()?!*"'「」<>;:=,.$%\[\]~&|]|{balanced})"#);
        let path = format!(r"(?:(?:{general}*(?:{balanced}{general}*)*{ending})|(?:{general}+/))");
        let uchars = r"\x{A0}-\x{D7FF}\x{F900}-\x{FDCF}\x{FDF0}-\x{FFEF}\x{10000}-\x{1FFFD}\x{20000}-\x{2FFFD}\x{30000}-\x{3FFFD}\x{40000}-\x{4FFFD}\x{50000}-\x{5FFFD}\x{60000}-\x{6FFFD}\x{70000}-\x{7FFFD}\x{80000}-\x{8FFFD}\x{90000}-\x{9FFFD}\x{A0000}-\x{AFFFD}\x{B0000}-\x{BFFFD}\x{C0000}-\x{CFFFD}\x{D0000}-\x{DFFFD}\x{E1000}-\x{EFFFD}\x{E000}-\x{F8FF}\x{F0000}-\x{FFFFD}\x{100000}-\x{10FFFD}";
        let query = format!(r"[a-z0-9!?*'();:&=+$/%#\[\]\-_.,~|@\^{uchars}]");
        let query_ending = format!(r"[a-z0-9_&=#/\-{uchars}]");
        fancy_regex::Regex::new(&format!(
            r"(?i)(?:^|[^A-Z0-9@＠$#＃\x{{FFFE}}\x{{FEFF}}\x{{FFFF}}]|[\x{{202A}}-\x{{202E}}\x{{061C}}\x{{200E}}\x{{200F}}\x{{2066}}-\x{{2069}}])(https?://{domain}(?::[0-9]+)?(?:/{path}*)?(?:\?{query}*{query_ending})?)"
        ))
        .expect("valid URL pattern")
    });
    URL_PATTERN
        .captures_iter(text)
        .filter_map(|c| c.ok()?.get(1).map(|m| m.as_str().to_owned()))
        .collect()
}

// ── Attribution ────────────────────────────────────────────────────────────

struct LinkedAccount {
    id: i64,
    local: bool,
    attribution_domains: Vec<String>,
}

/// `ResolveAccountService.new.call(handle, suppress_errors: true)`.
async fn resolve_account(state: &AppState, handle: &str) -> Option<LinkedAccount> {
    let handle = handle.trim().trim_start_matches('@');
    let (username, domain) = match handle.split_once('@') {
        Some((user, domain)) => (user, Some(domain.to_owned())),
        None => (handle, None),
    };
    if username.is_empty() {
        return None;
    }
    let resolved = crate::api::mastodon::statuses::resolve_mention_accounts(
        state,
        &[(username.to_lowercase(), domain)],
        &state.instance.domain,
    )
    .await;
    let (_, account) = resolved.into_iter().next()?;
    Some(LinkedAccount {
        id: account.id,
        local: account.domain.is_none(),
        attribution_domains: account.attribution_domains.unwrap_or_default(),
    })
}

/// `Account#can_be_attributed_from?`: the domain, or one it is a subdomain
/// of, is among the account's attribution domains.
fn can_be_attributed_from(attribution_domains: &[String], domain: &str) -> bool {
    let segments: Vec<&str> = domain.split('.').collect();
    (0..segments.len()).any(|i| {
        let variant = segments[i..].join(".");
        attribution_domains.contains(&variant)
    })
}

/// `PreviewCardProvider.matching_domain(domain)&.trendable?`.
async fn provider_trendable(state: &AppState, domain: &str) -> bool {
    let segments: Vec<&str> = domain.split('.').collect();
    let variants: Vec<String> = (0..segments.len())
        .map(|i| segments[i..].join("."))
        .collect();
    sqlx::query_scalar!(
        r#"SELECT trendable FROM preview_card_providers
           WHERE domain = ANY($1) ORDER BY char_length(domain) DESC LIMIT 1"#,
        &variants
    )
    .fetch_optional(&state.db)
    .await
    .ok()
    .flatten()
    .flatten()
    .unwrap_or(false)
}

// ── The card ───────────────────────────────────────────────────────────────

/// A `preview_cards` row, as the service changes it.
#[derive(Clone, Debug, PartialEq, Default)]
struct Card {
    id: Option<i64>,
    url: String,
    title: String,
    description: String,
    image_file_name: Option<String>,
    image_content_type: Option<String>,
    image_file_size: Option<i32>,
    image_updated_at: Option<NaiveDateTime>,
    image_storage_schema_version: Option<i32>,
    card_type: i32,
    html: String,
    author_name: String,
    author_url: String,
    provider_name: String,
    provider_url: String,
    width: i32,
    height: i32,
    embed_url: String,
    blurhash: Option<String>,
    language: Option<String>,
    link_type: Option<i32>,
    published_at: Option<NaiveDateTime>,
    image_description: String,
    author_account_id: Option<i64>,
    unverified_author_account_id: Option<i64>,
    updated_at: Option<NaiveDateTime>,
    /// A newly downloaded image, not yet stored.
    #[allow(clippy::struct_field_names)]
    pending_image: Option<PendingImage>,
}

#[derive(Clone, Debug)]
struct PendingImage {
    bytes: std::sync::Arc<Vec<u8>>,
    stored_content_type: &'static str,
    content_type: &'static str,
    file_name: String,
    width: u32,
    height: u32,
    blurhash: Option<String>,
}

impl PartialEq for PendingImage {
    fn eq(&self, other: &Self) -> bool {
        self.file_name == other.file_name
    }
}

impl Card {
    fn new(url: &str) -> Self {
        Card {
            url: url.to_owned(),
            ..Default::default()
        }
    }

    async fn find(state: &AppState, url: &str) -> Option<Card> {
        sqlx::query!(
            r#"SELECT id, url, title, description, image_file_name, image_content_type,
                      image_file_size, image_updated_at, image_storage_schema_version,
                      type AS card_type, html, author_name, author_url, provider_name,
                      provider_url, width, height, embed_url, blurhash, language, link_type,
                      published_at, image_description, author_account_id,
                      unverified_author_account_id, updated_at
               FROM preview_cards WHERE url = $1"#,
            url
        )
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
        .map(|r| Card {
            id: Some(r.id),
            url: r.url,
            title: r.title,
            description: r.description,
            image_file_name: r.image_file_name,
            image_content_type: r.image_content_type,
            image_file_size: r.image_file_size,
            image_updated_at: r.image_updated_at,
            image_storage_schema_version: r.image_storage_schema_version,
            card_type: r.card_type,
            html: r.html,
            author_name: r.author_name,
            author_url: r.author_url,
            provider_name: r.provider_name,
            provider_url: r.provider_url,
            width: r.width,
            height: r.height,
            embed_url: r.embed_url,
            blurhash: r.blurhash,
            language: r.language,
            link_type: r.link_type,
            published_at: r.published_at,
            image_description: r.image_description,
            author_account_id: r.author_account_id,
            unverified_author_account_id: r.unverified_author_account_id,
            updated_at: Some(r.updated_at),
            pending_image: None,
        })
    }

    /// `missing_image?`. Width and height are never null, so this is any
    /// card without an image — which is therefore fetched again every time.
    fn missing_image(&self) -> bool {
        self.image_file_name.as_deref().is_none_or(is_blank)
    }

    /// `image_remote_url=`: download and process the image now, as
    /// Paperclip does on assignment.
    async fn assign_image(&mut self, state: &AppState, url: Option<&str>) {
        match image::fetch(state, url).await {
            image::Outcome::Keep => {}
            image::Outcome::Clear => self.clear_image(),
            image::Outcome::New(p) => {
                self.pending_image = Some(PendingImage {
                    bytes: std::sync::Arc::new(p.bytes),
                    stored_content_type: p.stored_content_type,
                    content_type: p.content_type,
                    file_name: p.file_name,
                    width: p.width,
                    height: p.height,
                    blurhash: p.blurhash,
                });
            }
        }
    }

    /// `image = nil`.
    fn clear_image(&mut self) {
        self.pending_image = None;
        self.image_file_name = None;
        self.image_content_type = None;
        self.image_file_size = None;
        self.image_updated_at = None;
    }

    /// `save_with_optional_image!`. An image that cannot be stored is
    /// dropped and the card saved without it; a card that cannot be saved at
    /// all abandons the fetch.
    async fn save_with_optional_image(&mut self, state: &AppState) -> Result<(), Abort> {
        // `validates :url, presence: true, url: true, length: { maximum: … }`.
        let valid_url = Url::parse(&self.url).is_ok_and(|u| {
            matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty())
        });
        if !valid_url || self.url.chars().count() > URL_CHARACTER_LIMIT {
            return Err(Abort);
        }

        let before = match self.id {
            Some(_) => Card::find(state, &self.url).await,
            None => None,
        };
        let id = match self.id {
            Some(id) => id,
            None => sqlx::query_scalar!(r#"SELECT nextval('preview_cards_id_seq') AS "id!""#)
                .fetch_one(&state.db)
                .await
                .map_err(|_| Abort)?,
        };

        if let Some(image) = self.pending_image.take() {
            let key = image_key(id, &image.file_name);
            let stored = state
                .storage
                .store(&image.bytes, &key, image.stored_content_type)
                .await;
            if stored.is_ok() {
                // `extract_dimensions`, for a link.
                if self.card_type == TYPE_LINK {
                    self.width = image.width as i32;
                    self.height = image.height as i32;
                }
                self.image_file_name = Some(image.file_name);
                self.image_content_type = Some(image.content_type.to_owned());
                self.image_file_size = Some(image.bytes.len() as i32);
                self.image_updated_at = Some(chrono::Utc::now().naive_utc());
                self.image_storage_schema_version = Some(1);
                self.blurhash = image.blurhash;
            } else {
                self.clear_image();
            }
        }

        match self.id {
            None => {
                let inserted = sqlx::query_scalar!(
                    r#"INSERT INTO preview_cards
                         (id, url, title, description, image_file_name, image_content_type,
                          image_file_size, image_updated_at, image_storage_schema_version, type,
                          html, author_name, author_url, provider_name, provider_url, width,
                          height, embed_url, blurhash, language, link_type, published_at,
                          image_description, author_account_id, unverified_author_account_id,
                          created_at, updated_at)
                       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                               $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, now(), now())
                       ON CONFLICT (url) DO NOTHING
                       RETURNING id"#,
                    id,
                    self.url,
                    self.title,
                    self.description,
                    self.image_file_name,
                    self.image_content_type.as_deref(),
                    self.image_file_size,
                    self.image_updated_at,
                    self.image_storage_schema_version,
                    self.card_type,
                    self.html,
                    self.author_name,
                    self.author_url,
                    self.provider_name,
                    self.provider_url,
                    self.width,
                    self.height,
                    self.embed_url,
                    self.blurhash,
                    self.language,
                    self.link_type,
                    self.published_at,
                    self.image_description,
                    self.author_account_id,
                    self.unverified_author_account_id,
                )
                .fetch_optional(&state.db)
                .await
                .map_err(|_| Abort)?;
                // Someone else saved this URL first: `RecordNotUnique`.
                self.id = Some(inserted.ok_or(Abort)?);
            }
            Some(id) => {
                // Rails writes nothing, and leaves `updated_at` alone, when
                // nothing changed.
                let unchanged = before.as_ref().is_some_and(|b| {
                    let mut now = self.clone();
                    now.updated_at = b.updated_at;
                    now == *b
                });
                if !unchanged {
                    sqlx::query!(
                        r#"UPDATE preview_cards SET
                             title = $2, description = $3, image_file_name = $4,
                             image_content_type = $5, image_file_size = $6, image_updated_at = $7,
                             image_storage_schema_version = $8, type = $9, html = $10,
                             author_name = $11, author_url = $12, provider_name = $13,
                             provider_url = $14, width = $15, height = $16, embed_url = $17,
                             blurhash = $18, language = $19, link_type = $20, published_at = $21,
                             image_description = $22, author_account_id = $23,
                             unverified_author_account_id = $24, updated_at = now()
                           WHERE id = $1"#,
                        id,
                        self.title,
                        self.description,
                        self.image_file_name,
                        self.image_content_type.as_deref(),
                        self.image_file_size,
                        self.image_updated_at,
                        self.image_storage_schema_version,
                        self.card_type,
                        self.html,
                        self.author_name,
                        self.author_url,
                        self.provider_name,
                        self.provider_url,
                        self.width,
                        self.height,
                        self.embed_url,
                        self.blurhash,
                        self.language,
                        self.link_type,
                        self.published_at,
                        self.image_description,
                        self.author_account_id,
                        self.unverified_author_account_id,
                    )
                    .execute(&state.db)
                    .await
                    .map_err(|_| Abort)?;
                }
                // Paperclip deletes the file an image replaced.
                if let Some(old) = before.and_then(|b| b.image_file_name) {
                    if self.image_file_name.as_deref() != Some(old.as_str()) {
                        let _ = state.storage.delete(&image_key(id, &old)).await;
                    }
                }
            }
        }
        Ok(())
    }
}

/// Where Paperclip keeps a card's image: `:prefix_url:class/:attachment/
/// :id_partition/:style/:filename`, under `cache/` because a card is never
/// local.
pub fn image_key(id: i64, file_name: &str) -> String {
    format!(
        "cache/preview_cards/images/{}/original/{}",
        crate::media::int_to_path(id),
        file_name
    )
}

/// The path of a stored card image: `image_key` for cards stored with
/// `image_storage_schema_version` 1, and without `cache/` for older ones.
pub fn image_path(id: i64, file_name: &str, storage_schema_version: Option<i32>) -> String {
    if storage_schema_version.unwrap_or(0) >= 1 {
        image_key(id, file_name)
    } else {
        format!(
            "preview_cards/images/{}/original/{}",
            crate::media::int_to_path(id),
            file_name
        )
    }
}

/// `Sanitize.fragment(html, Sanitize::Config::MASTODON_OEMBED)`.
pub fn sanitize_oembed(html: &str) -> String {
    crate::formatter::sanitize::oembed(html)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_urls_follow_twitter_text() {
        assert_eq!(
            local_urls("look https://example.com/a/b?x=1, and (https://ex.org/wiki/A_(b)) then."),
            vec!["https://example.com/a/b?x=1", "https://ex.org/wiki/A_(b)"]
        );
        assert_eq!(
            local_urls("https://example.com/path. https://example.comと"),
            vec!["https://example.com/path", "https://example.com"]
        );
        assert!(local_urls("@https://example.com x#https://example.com").is_empty());
        assert!(local_urls("http://localhost:3000/ http://10.0.0.1/").is_empty());
        assert_eq!(
            local_urls("https://한국.kr/경로"),
            vec!["https://한국.kr/경로"]
        );
        // twitter-text 3.1.0 knows no `.example` and no `.zzz`.
        assert!(local_urls("https://한국.example/경로 https://site.zzz/").is_empty());
    }

    #[test]
    fn remote_links_skip_tags_and_mentions() {
        let mentions: HashSet<String> = ["https://social.example/@bob".to_owned()].into();
        let links = remote_links(
            r#"<p><a href="https://social.example/tags/x" rel="tag">#x</a>
               <span class="h-card"><a class="u-url mention" href="https://social.example/@ann">@ann</a></span>
               <a href="https://social.example/@bob">@bob</a>
               <a href="https://news.example/story">story</a></p>"#,
            &mentions,
        );
        assert_eq!(links, vec!["https://news.example/story"]);
    }

    #[test]
    fn attribution_domains_cover_subdomains() {
        let domains = vec!["example.com".to_owned()];
        assert!(can_be_attributed_from(&domains, "example.com"));
        assert!(can_be_attributed_from(&domains, "blog.example.com"));
        assert!(!can_be_attributed_from(&domains, "notexample.com"));
    }

    #[test]
    fn oembed_html_is_sanitized_like_mastodon() {
        assert_eq!(
            sanitize_oembed(
                r#"<div><iframe src="https://v.example/e/1" width="480" height="270" allow="autoplay" onload="x()"></iframe><script>alert(1)</script></div>"#
            ),
            r#" <iframe src="https://v.example/e/1" width="480" height="270" sandbox="allow-scripts allow-same-origin allow-popups allow-popups-to-escape-sandbox allow-forms"></iframe> "#
        );
        assert_eq!(
            sanitize_oembed(r#"<iframe src="/relative"></iframe>"#),
            r#"<iframe sandbox="allow-scripts allow-same-origin allow-popups allow-popups-to-escape-sandbox allow-forms"></iframe>"#
        );
    }

    #[test]
    fn image_paths_follow_paperclip() {
        assert_eq!(
            image_path(12, "abc.jpg", Some(1)),
            "cache/preview_cards/images/000/000/012/original/abc.jpg"
        );
        assert_eq!(
            image_path(12, "abc.jpg", None),
            "preview_cards/images/000/000/012/original/abc.jpg"
        );
    }
}
