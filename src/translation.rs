//! Machine translation of statuses, as Mastodon's `TranslationService` and
//! `TranslateStatusService` do it.
//!
//! An instance names DeepL or a LibreTranslate server in its
//! `[instance.translation]` table. Both are operator-configured, so neither
//! goes through the SSRF-guarded client: Mastodon reaches LibreTranslate with
//! `allow_local: true`, since it usually runs next to the instance.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{config::TranslationConfig, state::AppState};

/// `Rails.cache.fetch('translation_service/languages', expires_in: 7.days)`.
const LANGUAGES_CACHE_KEY: &str = "translation_service/languages";
const LANGUAGES_CACHE_TTL: u64 = 7 * 24 * 60 * 60;
/// `TranslateStatusService::CACHE_TTL`.
const TRANSLATION_CACHE_TTL: u64 = 24 * 60 * 60;
/// A little more than Mastodon's `Request::TIMEOUT`, which allows ten
/// seconds each to connect, write and read.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// `Request#body_with_limit`'s default.
const BODY_LIMIT: usize = 1024 * 1024;

/// `TranslationService::Error` and its kin, plus the connection failures
/// Mastodon's `Request` raises.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no translation service is configured")]
    NotConfigured,
    #[error("the translation service is rate limiting this instance")]
    TooManyRequests,
    #[error("the translation service's quota is exhausted")]
    QuotaExceeded,
    #[error("the translation service answered unexpectedly")]
    UnexpectedResponse,
    /// `HTTP::ConnectionError` and `HTTP::TimeoutError`.
    #[error("could not reach the translation service: {0}")]
    Connection(String),
    /// The language list did not parse. Mastodon does not rescue the
    /// `JSON::ParserError` there, so it is a 500.
    #[error("the translation service's language list did not parse")]
    MalformedLanguages,
}

/// `TranslationService::Translation`.
#[derive(Debug, Clone)]
pub struct Translation {
    pub text: String,
    pub detected_source_language: Option<String>,
    pub provider: &'static str,
}

/// Which target languages each source language translates into. Mastodon
/// keeps this as a hash keyed by language code with a `nil` key for
/// auto-detected sources; JSON has no `nil` key, so that list is apart.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Languages {
    /// In the order the service listed them.
    pub sources: Vec<(String, Vec<String>)>,
    /// The `nil` key: what a status with no language translates into.
    pub auto: Vec<String>,
}

impl Languages {
    /// `languages[@status.language] || []`.
    pub fn targets(&self, source: Option<&str>) -> &[String] {
        match source {
            None => &self.auto,
            Some(code) => self
                .sources
                .iter()
                .find(|(c, _)| c == code)
                .map(|(_, t)| t.as_slice())
                .unwrap_or(&[]),
        }
    }

    /// `/api/v1/instance/translation_languages`: the `nil` key renamed `und`.
    pub fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        for (source, targets) in &self.sources {
            map.insert(source.clone(), serde_json::json!(targets));
        }
        map.remove("und");
        map.insert("und".into(), serde_json::json!(self.auto));
        serde_json::Value::Object(map)
    }
}

enum Backend<'a> {
    DeepL {
        base_url: String,
        api_key: &'a str,
    },
    LibreTranslate {
        base_url: &'a str,
        api_key: Option<&'a str>,
    },
}

fn present(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|v| !v.trim().is_empty())
}

impl TranslationConfig {
    /// `TranslationService.configured?`.
    pub fn configured(&self) -> bool {
        present(&self.deepl_api_key).is_some() || present(&self.libre_translate_endpoint).is_some()
    }

    /// `TranslationService.configured`.
    fn backend(&self) -> Result<Backend<'_>, Error> {
        if let Some(api_key) = present(&self.deepl_api_key) {
            let base_url = match present(&self.deepl_endpoint) {
                Some(endpoint) => endpoint.trim_end_matches('/').to_string(),
                None if self.deepl_plan.as_deref().unwrap_or("free") == "free" => {
                    "https://api-free.deepl.com".into()
                }
                None => "https://api.deepl.com".into(),
            };
            Ok(Backend::DeepL { base_url, api_key })
        } else if let Some(endpoint) = present(&self.libre_translate_endpoint) {
            Ok(Backend::LibreTranslate {
                base_url: endpoint.trim_end_matches('/'),
                api_key: present(&self.libre_translate_api_key),
            })
        } else {
            Err(Error::NotConfigured)
        }
    }
}

/// `Request#body_with_limit`: the body, refused past a megabyte.
async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, Error> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Error::Connection(e.to_string()))?
    {
        if body.len() + chunk.len() > BODY_LIMIT {
            return Err(Error::UnexpectedResponse);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Each service's `request`: send, and sort the status codes into errors.
async fn perform(
    request: reqwest::RequestBuilder,
    too_many: u16,
    quota: u16,
) -> Result<Vec<u8>, Error> {
    let response = request
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .map_err(|e| Error::Connection(e.to_string()))?;
    match response.status().as_u16() {
        code if code == too_many => Err(Error::TooManyRequests),
        code if code == quota => Err(Error::QuotaExceeded),
        200..300 => read_body(response).await,
        _ => Err(Error::UnexpectedResponse),
    }
}

/// `TranslationService::DeepL#normalize_language`: `PT-BR` is `pt-BR`.
fn normalize_deepl_language(language: &str) -> String {
    let mut subtags: Vec<String> = language.split(['_', '-']).map(str::to_string).collect();
    if let Some(first) = subtags.get_mut(0) {
        *first = first.to_lowercase();
    }
    if let Some(second) = subtags.get_mut(1) {
        *second = second.to_uppercase();
    }
    subtags.join("-")
}

async fn deepl_languages(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    kind: &str,
) -> Result<Vec<String>, Error> {
    #[derive(Deserialize)]
    struct Language {
        language: String,
    }
    let body = perform(
        client
            .get(format!("{base_url}/v2/languages?type={kind}"))
            .header("Authorization", format!("DeepL-Auth-Key {api_key}")),
        429,
        456,
    )
    .await?;
    let languages: Vec<Language> =
        serde_json::from_slice(&body).map_err(|_| Error::MalformedLanguages)?;
    Ok(languages
        .iter()
        .map(|l| normalize_deepl_language(&l.language))
        .collect())
}

/// `TranslationService#languages`, asked of the service.
async fn fetch_languages(
    client: &reqwest::Client,
    backend: &Backend<'_>,
) -> Result<Languages, Error> {
    match backend {
        Backend::DeepL { base_url, api_key } => {
            let sources = deepl_languages(client, base_url, api_key, "source").await?;
            // DeepL deprecated EN and PT for EN-GB/EN-US and PT-BR/PT-PT;
            // they still work but are not listed.
            let mut targets: Vec<String> = vec!["en".into(), "pt".into()];
            targets.extend(deepl_languages(client, base_url, api_key, "target").await?);
            let without = |exclude: Option<&str>| -> Vec<String> {
                targets
                    .iter()
                    .filter(|t| Some(t.as_str()) != exclude)
                    .cloned()
                    .collect()
            };
            Ok(Languages {
                auto: without(None),
                sources: sources
                    .iter()
                    .map(|s| (s.clone(), without(Some(s))))
                    .collect(),
            })
        }
        Backend::LibreTranslate { base_url, .. } => {
            #[derive(Deserialize)]
            struct Language {
                code: String,
                #[serde(default)]
                targets: Vec<String>,
            }
            let body = perform(
                client
                    .get(format!("{base_url}/languages"))
                    .header("Content-Type", "application/json"),
                429,
                403,
            )
            .await?;
            let listed: Vec<Language> =
                serde_json::from_slice(&body).map_err(|_| Error::MalformedLanguages)?;
            let mut sources: Vec<(String, Vec<String>)> = Vec::new();
            for language in listed {
                let targets = language
                    .targets
                    .into_iter()
                    .filter(|t| *t != language.code)
                    .collect();
                // `to_h`: a repeated code keeps its place and takes the last list.
                match sources.iter_mut().find(|(c, _)| *c == language.code) {
                    Some(entry) => entry.1 = targets,
                    None => sources.push((language.code, targets)),
                }
            }
            let mut auto: Vec<String> = sources.iter().flat_map(|(_, t)| t.clone()).collect();
            auto.sort();
            auto.dedup();
            Ok(Languages { sources, auto })
        }
    }
}

/// `TranslationService#translate`.
async fn translate_texts(
    client: &reqwest::Client,
    backend: &Backend<'_>,
    texts: &[String],
    source_language: Option<&str>,
    target_language: &str,
) -> Result<Vec<Translation>, Error> {
    match backend {
        Backend::DeepL { base_url, api_key } => {
            let mut form: Vec<(&str, String)> = texts.iter().map(|t| ("text", t.clone())).collect();
            if let Some(source) = source_language {
                form.push(("source_lang", source.to_uppercase()));
            }
            form.push(("target_lang", target_language.to_string()));
            form.push(("tag_handling", "html".into()));
            let body = perform(
                client
                    .post(format!("{base_url}/v2/translate"))
                    .header("Authorization", format!("DeepL-Auth-Key {api_key}"))
                    .form(&form),
                429,
                456,
            )
            .await?;
            #[derive(Deserialize)]
            struct Item {
                text: String,
                detected_source_language: Option<String>,
            }
            #[derive(Deserialize)]
            struct Response {
                translations: Vec<Item>,
            }
            let data: Response =
                serde_json::from_slice(&body).map_err(|_| Error::UnexpectedResponse)?;
            Ok(data
                .translations
                .into_iter()
                .map(|t| Translation {
                    text: t.text,
                    detected_source_language: t.detected_source_language.map(|l| l.to_lowercase()),
                    provider: "DeepL.com",
                })
                .collect())
        }
        Backend::LibreTranslate { base_url, api_key } => {
            let request = serde_json::json!({
                "q": texts,
                "source": source_language.filter(|s| !s.is_empty()).unwrap_or("auto"),
                "target": target_language,
                "format": "html",
                "api_key": api_key,
            });
            let body = perform(
                client
                    .post(format!("{base_url}/translate"))
                    .header("Content-Type", "application/json")
                    .body(request.to_string()),
                429,
                403,
            )
            .await?;
            let data: serde_json::Value =
                serde_json::from_slice(&body).map_err(|_| Error::UnexpectedResponse)?;
            let texts = data
                .get("translatedText")
                .and_then(|t| t.as_array())
                .ok_or(Error::UnexpectedResponse)?;
            texts
                .iter()
                .enumerate()
                .map(|(index, text)| {
                    let detected = data
                        .get("detectedLanguage")
                        .and_then(|d| d.get(index))
                        .and_then(|d| d.get("language"))
                        .and_then(|l| l.as_str())
                        .map(str::to_string)
                        .or_else(|| source_language.map(str::to_string));
                    Ok(Translation {
                        text: text.as_str().ok_or(Error::UnexpectedResponse)?.to_string(),
                        detected_source_language: detected,
                        provider: "LibreTranslate",
                    })
                })
                .collect()
        }
    }
}

async fn cache_get<T: serde::de::DeserializeOwned>(state: &AppState, key: &str) -> Option<T> {
    let mut redis = state.redis.clone();
    let raw: Option<String> = redis::cmd("GET")
        .arg(state.redis_keys.key(key))
        .query_async(&mut redis)
        .await
        .inspect_err(|error| tracing::warn!(%error, key, "could not read the translation cache"))
        .ok()
        .flatten();
    raw.and_then(|raw| serde_json::from_str(&raw).ok())
}

async fn cache_set<T: Serialize>(state: &AppState, key: &str, value: &T, ttl: u64) {
    let Ok(raw) = serde_json::to_string(value) else {
        return;
    };
    let mut redis = state.redis.clone();
    let result: redis::RedisResult<()> = redis::cmd("SET")
        .arg(state.redis_keys.key(key))
        .arg(raw)
        .arg("EX")
        .arg(ttl)
        .query_async(&mut redis)
        .await;
    if let Err(error) = result {
        tracing::warn!(%error, key, "could not write the translation cache");
    }
}

/// The languages the configured service translates between, cached for a
/// week as Mastodon caches them. Errors when none is configured.
pub async fn languages(state: &AppState) -> Result<Languages, Error> {
    let backend = state.instance.translation.backend()?;
    if let Some(cached) = cache_get(state, LANGUAGES_CACHE_KEY).await {
        return Ok(cached);
    }
    let languages = fetch_languages(&state.http, &backend).await?;
    cache_set(state, LANGUAGES_CACHE_KEY, &languages, LANGUAGES_CACHE_TTL).await;
    Ok(languages)
}

// ── TranslateStatusService ─────────────────────────────────────────────────

/// What `TranslateStatusService#source_texts` keys each text by.
#[derive(Debug, Clone)]
pub enum Source {
    Content,
    SpoilerText,
    /// A poll option, by its index.
    PollOption(usize),
    /// A media attachment, by its id.
    MediaAttachment(String),
}

/// The status translation as `Rails.cache` keeps it: everything but the
/// status, whose poll id the serializer reads off the status itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusTranslation {
    pub detected_source_language: Option<String>,
    pub language: String,
    pub provider: Option<String>,
    pub content: String,
    pub spoiler_text: String,
    pub poll_options: Vec<String>,
    pub media_attachments: Vec<(String, String)>,
}

/// What a status is translated from: its texts in `source_texts` order and
/// what it says it is written in.
pub struct StatusSource {
    pub language: Option<String>,
    pub texts: Vec<(Source, String)>,
}

/// Why a status was not translated.
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    #[error(transparent)]
    Service(#[from] Error),
    /// `Mastodon::NotPermittedError`: not public, or no route from its
    /// language to the target.
    #[error("not permitted")]
    NotPermitted,
}

/// `content_hash`: a digest of the texts under Mastodon's keys.
fn content_hash(texts: &[(Source, String)]) -> String {
    use base64::Engine as _;
    let mut map = serde_json::Map::new();
    for (source, text) in texts {
        let key = match source {
            Source::Content => "content".to_string(),
            Source::SpoilerText => "spoiler_text".to_string(),
            Source::PollOption(index) => format!("Poll::Option-{index}"),
            Source::MediaAttachment(id) => format!("MediaAttachment-{id}"),
        };
        map.insert(key, serde_json::Value::String(text.clone()));
    }
    let json = serde_json::Value::Object(map).to_string();
    base64::engine::general_purpose::STANDARD.encode(Sha256::digest(json.as_bytes()))
}

/// `TranslateStatusService#call`: the status translated into
/// `target_language`, from the cache when it was translated within a day.
pub async fn translate_status(
    state: &AppState,
    source: &StatusSource,
    distributable: bool,
    target_language: &str,
) -> Result<StatusTranslation, TranslateError> {
    // Mastodon asks for the language list before anything else, so an
    // unconfigured service is a 404 before an unlisted status is a 403.
    let languages = languages(state).await?;
    let targets = languages.targets(source.language.as_deref());
    let target_language = if targets.iter().any(|t| t == target_language) {
        target_language.to_string()
    } else {
        target_language
            .split(['_', '-'])
            .next()
            .unwrap_or(target_language)
            .to_string()
    };
    if !distributable || !targets.contains(&target_language) {
        return Err(TranslateError::NotPermitted);
    }

    let cache_key = format!(
        "v2:translations/{}/{}/{}",
        source.language.as_deref().unwrap_or(""),
        target_language,
        content_hash(&source.texts)
    );
    if let Some(cached) = cache_get::<StatusTranslation>(state, &cache_key).await {
        return Ok(cached);
    }

    let backend = state.instance.translation.backend()?;
    let texts: Vec<String> = source.texts.iter().map(|(_, t)| t.clone()).collect();
    let translations = translate_texts(
        &state.http,
        &backend,
        &texts,
        source.language.as_deref(),
        &target_language,
    )
    .await?;
    let translation = build_status_translation(&source.texts, &translations, target_language);
    cache_set(state, &cache_key, &translation, TRANSLATION_CACHE_TTL).await;
    Ok(translation)
}

/// `build_status_translation`.
fn build_status_translation(
    sources: &[(Source, String)],
    translations: &[Translation],
    language: String,
) -> StatusTranslation {
    let mut out = StatusTranslation {
        detected_source_language: translations
            .first()
            .and_then(|t| t.detected_source_language.clone()),
        language,
        provider: translations.first().map(|t| t.provider.to_string()),
        content: String::new(),
        spoiler_text: String::new(),
        poll_options: Vec::new(),
        media_attachments: Vec::new(),
    };
    for (index, (source, _)) in sources.iter().enumerate() {
        // Ruby's `translations[index]` is nil past the end, and `nil.text`
        // raises; a short answer is an unexpected one, so stop there.
        let Some(translation) = translations.get(index) else {
            break;
        };
        match source {
            Source::Content => {
                out.content = sanitize_strict(&unwrap_emoji_shortcodes(&translation.text));
            }
            Source::SpoilerText => {
                out.spoiler_text = text_content(&unwrap_emoji_shortcodes(&translation.text));
            }
            Source::PollOption(_) => out
                .poll_options
                .push(text_content(&unwrap_emoji_shortcodes(&translation.text))),
            Source::MediaAttachment(id) => out
                .media_attachments
                .push((id.clone(), decode_entities(&translation.text))),
        }
    }
    out
}

// ── HTML handling ──────────────────────────────────────────────────────────

/// `ERB::Util.html_escape`.
pub fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn escape_text(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            c => out.push(c),
        }
    }
}

fn escape_attribute(value: &str, out: &mut String) {
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            c => out.push(c),
        }
    }
}

const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr",
];

/// How [`serialize`] treats an element.
enum ElementAction {
    /// Write it with these attributes.
    Keep(Vec<(String, String)>),
    /// Write only its children.
    Unwrap,
}

/// Write a parsed fragment back out as Nokogiri's `to_html` would, letting
/// `text` rewrite text nodes and `element` rewrite or unwrap elements.
fn serialize(
    node: ego_tree::NodeRef<'_, scraper::Node>,
    text: &dyn Fn(&str, &mut String),
    element: &dyn Fn(&scraper::node::Element) -> ElementAction,
    out: &mut String,
) {
    for child in node.children() {
        match child.value() {
            scraper::Node::Text(t) => text(t, out),
            scraper::Node::Element(e) => match element(e) {
                ElementAction::Unwrap => serialize(child, text, element, out),
                ElementAction::Keep(attrs) => {
                    let name = e.name();
                    out.push('<');
                    out.push_str(name);
                    for (key, value) in attrs {
                        out.push(' ');
                        out.push_str(&key);
                        out.push_str("=\"");
                        escape_attribute(&value, out);
                        out.push('"');
                    }
                    out.push('>');
                    if !VOID_ELEMENTS.contains(&name) {
                        serialize(child, text, element, out);
                        out.push_str("</");
                        out.push_str(name);
                        out.push('>');
                    }
                }
            },
            _ => {}
        }
    }
}

fn attrs_of(e: &scraper::node::Element) -> Vec<(String, String)> {
    e.attrs()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// `EmojiFormatter.new(html, emojis, raw_shortcode: true)`: each shortcode of
/// an emoji the status uses wrapped in `<span translate="no">`, so the
/// service leaves it alone.
pub fn wrap_emoji_shortcodes(html: &str, shortcodes: &[String]) -> String {
    if shortcodes.is_empty() || html.trim().is_empty() {
        return html.to_string();
    }
    let fragment = scraper::Html::parse_fragment(html);
    let mut out = String::new();
    let text = |t: &str, out: &mut String| wrap_shortcodes_in_text(t, shortcodes, out);
    let element = |e: &scraper::node::Element| ElementAction::Keep(attrs_of(e));
    serialize(*fragment.root_element(), &text, &element, &mut out);
    out
}

/// `DISALLOWED_BOUNDING_REGEX`: `[[:alnum:]:]`.
fn disallowed_bounding(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric() || c == ':')
}

/// The scan inside `EmojiFormatter#to_s`, over one text node.
fn wrap_shortcodes_in_text(text: &str, shortcodes: &[String], out: &mut String) {
    let chars: Vec<char> = text.chars().collect();
    let mut inside = false;
    let mut start = 0usize;
    let mut last = 0usize;
    let mut i = 0usize;
    while i < chars.len() {
        if inside && chars[i] == ':' {
            inside = false;
            let shortcode: String = chars[start + 1..i].iter().collect();
            let after = chars.get(i + 1).copied();
            if !disallowed_bounding(after) && shortcodes.contains(&shortcode) {
                let before: String = chars[last..start].iter().collect();
                escape_text(&before, out);
                out.push_str("<span translate=\"no\">:");
                escape_text(&shortcode, out);
                out.push_str(":</span>");
                last = i + 1;
            }
        } else if chars[i] == ':' && (i == 0 || !disallowed_bounding(Some(chars[i - 1]))) {
            inside = true;
            start = i;
        }
        i += 1;
    }
    let rest: String = chars[last..].iter().collect();
    escape_text(&rest, out);
}

/// `unwrap_emoji_shortcodes`: `translate` off every `span[translate="no"]`,
/// and the span itself gone when that was its only attribute.
fn unwrap_emoji_shortcodes(html: &str) -> String {
    let fragment = scraper::Html::parse_fragment(html);
    let mut out = String::new();
    let element = |e: &scraper::node::Element| {
        if e.name() == "span" && e.attr("translate") == Some("no") {
            let attrs: Vec<_> = attrs_of(e)
                .into_iter()
                .filter(|(k, _)| k != "translate")
                .collect();
            if attrs.is_empty() {
                ElementAction::Unwrap
            } else {
                ElementAction::Keep(attrs)
            }
        } else {
            ElementAction::Keep(attrs_of(e))
        }
    };
    serialize(*fragment.root_element(), &escape_text, &element, &mut out);
    out
}

/// Nokogiri's `#content`: the text of a fragment, entities decoded.
fn text_content(html: &str) -> String {
    scraper::Html::parse_fragment(html)
        .root_element()
        .text()
        .collect()
}

/// `HTMLEntities.new.decode`: entities decoded, anything that looks like a
/// tag left as it is. A `<textarea>` parses exactly so; the newline after its
/// start tag keeps one at the start of the text from being eaten.
fn decode_entities(text: &str) -> String {
    if text.to_ascii_lowercase().contains("</textarea") {
        return text_content(text);
    }
    let fragment = scraper::Html::parse_fragment(&format!("<textarea>\n{text}</textarea>"));
    fragment.root_element().text().collect()
}

/// `Sanitize::Config::MASTODON_STRICT`.
pub fn sanitize_strict(html: &str) -> String {
    use std::sync::LazyLock;
    static BUILDER: LazyLock<ammonia::Builder<'static>> = LazyLock::new(|| {
        let mut builder = ammonia::Builder::empty();
        builder
            .add_tags([
                "p",
                "br",
                "span",
                "a",
                "del",
                "s",
                "pre",
                "blockquote",
                "code",
                "b",
                "strong",
                "u",
                "i",
                "em",
                "ul",
                "ol",
                "li",
                "ruby",
                "rt",
                "rp",
            ])
            .add_generic_attributes(["lang"])
            .add_tag_attributes("a", ["href", "class", "translate"])
            .add_tag_attributes("span", ["class", "translate"])
            .add_tag_attributes("ol", ["start", "reversed"])
            .add_tag_attributes("li", ["value"])
            .add_tag_attributes("p", ["class"])
            .add_url_schemes([
                "http", "https", "dat", "dweb", "ipfs", "ipns", "ssb", "gopher", "xmpp", "magnet",
                "gemini",
            ])
            .url_relative(ammonia::UrlRelative::Deny)
            .link_rel(Some("nofollow noopener"))
            .set_tag_attribute_value("a", "target", "_blank")
            .attribute_filter(|_element, attribute, value| match attribute {
                // `ALLOWED_CLASS_TRANSFORMER`.
                "class" => Some(
                    value
                        .split(['\t', '\n', '\x0c', '\r', ' '])
                        .filter(|c| {
                            ["h-", "p-", "u-", "dt-", "e-"]
                                .iter()
                                .any(|prefix| c.starts_with(prefix))
                                || matches!(
                                    *c,
                                    "mention"
                                        | "hashtag"
                                        | "ellipsis"
                                        | "invisible"
                                        | "quote-inline"
                                )
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                        .into(),
                ),
                // `TRANSLATE_TRANSFORMER`.
                "translate" => (value == "no").then(|| value.into()),
                _ => Some(value.into()),
            });
        builder
    });
    BUILDER.clean(html).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_only_known_shortcodes_at_boundaries() {
        let codes = vec!["blob".to_string()];
        assert_eq!(
            wrap_emoji_shortcodes("<p>hi :blob: and a:blob: :nope:</p>", &codes),
            "<p>hi <span translate=\"no\">:blob:</span> and a:blob: :nope:</p>"
        );
    }

    #[test]
    fn unwraps_bare_spans_and_keeps_classed_ones() {
        assert_eq!(
            unwrap_emoji_shortcodes(
                "<p>Hallo <span translate=\"no\">:blob:</span> <span class=\"h-card\" translate=\"no\">x</span></p>"
            ),
            "<p>Hallo :blob: <span class=\"h-card\">x</span></p>"
        );
    }

    #[test]
    fn decodes_entities_without_parsing_tags() {
        assert_eq!(decode_entities("a &amp; b &lt;i&gt; <b>"), "a & b <i> <b>");
        assert_eq!(decode_entities("\nline"), "\nline");
    }

    #[test]
    fn strict_sanitizer_keeps_mastodon_markup() {
        let html = sanitize_strict(
            "<p class=\"x\">a <a href=\"https://e.example/\" class=\"mention u-url\">b</a><script>x</script><img src=x></p>",
        );
        assert_eq!(
            html,
            "<p class=\"\">a <a href=\"https://e.example/\" class=\"mention u-url\" target=\"_blank\" rel=\"nofollow noopener\">b</a></p>"
        );
    }

    #[test]
    fn deepl_languages_normalize() {
        assert_eq!(normalize_deepl_language("PT-BR"), "pt-BR");
        assert_eq!(normalize_deepl_language("DE"), "de");
        assert_eq!(normalize_deepl_language("zh_hans"), "zh-HANS");
    }

    #[test]
    fn und_comes_last() {
        let languages = Languages {
            sources: vec![("en".into(), vec!["de".into()])],
            auto: vec!["de".into(), "en".into()],
        };
        assert_eq!(
            languages.to_json(),
            serde_json::json!({"en": ["de"], "und": ["de", "en"]})
        );
    }
}
