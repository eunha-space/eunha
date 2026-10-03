//! `FetchOEmbedService`: find a page's oEmbed endpoint, from a per-domain
//! cache or the page's own `<link>`, and ask it about the page.

use std::sync::LazyLock;

use regex::Regex;
use scraper::{Html, Selector};
use serde_json::{Map, Value};
use url::Url;

use super::{Abort, BODY_LIMIT};
use crate::state::AppState;

/// `FetchOEmbedService::ENDPOINT_CACHE_EXPIRES_IN`, 24 hours.
const ENDPOINT_CACHE_SECONDS: u64 = 24 * 60 * 60;

/// `FetchOEmbedService::URL_REGEX`: an endpoint whose query names the page,
/// which can be cached as a template for the rest of the domain.
static URL_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(=(https?(%3A|:)(//|%2F%2F)))([^&]*)").unwrap());

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Json,
    Xml,
}

/// What `Rails.cache` holds under `oembed_endpoint:{domain}`.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct CachedEndpoint {
    pub endpoint: Option<String>,
    pub format: Option<Format>,
}

/// One service object, reused across the cached and the discovered attempt
/// as `FetchLinkCardService` reuses it: an endpoint the first attempt chose
/// is kept by the second, which only fills in what is still missing.
#[derive(Default)]
pub struct OEmbed {
    pub endpoint_url: Option<String>,
    format: Option<Format>,
}

fn cache_key(domain: &str) -> String {
    format!("oembed_endpoint:{domain}")
}

pub async fn cached_endpoint(state: &AppState, domain: &str) -> Option<CachedEndpoint> {
    let mut redis = state.redis.clone();
    let raw: Option<String> = redis::cmd("GET")
        .arg(state.redis_keys.key(cache_key(domain)))
        .query_async(&mut redis)
        .await
        .ok()
        .flatten();
    serde_json::from_str(&raw?).ok()
}

impl OEmbed {
    /// `call(url, cached_endpoint:)`.
    pub async fn call_cached(
        &mut self,
        state: &AppState,
        url: &str,
        cached: &CachedEndpoint,
    ) -> Result<Option<Map<String, Value>>, Abort> {
        if let (Some(endpoint), Some(format)) = (&cached.endpoint, cached.format) {
            // `Addressable::Template#expand(url:)`: the URL percent-encoded
            // into the `{url}` placeholder.
            self.endpoint_url = Some(endpoint.replace("{url}", &urlencoding::encode(url)));
            self.format = Some(format);
        }
        self.fetch(state).await
    }

    /// `call(url, html:)`.
    pub async fn call_discover(
        &mut self,
        state: &AppState,
        url: &str,
        html: &str,
    ) -> Result<Option<Map<String, Value>>, Abort> {
        self.discover(state, url, html).await;
        self.fetch(state).await
    }

    async fn discover(&mut self, state: &AppState, url: &str, html: &str) {
        static JSON_LINK: LazyLock<Selector> = LazyLock::new(|| Selector::parse("link").unwrap());
        // `@format = @options[:format]`, which is never given.
        self.format = None;
        let href_of = |types: &[&str]| -> Option<String> {
            let page = Html::parse_document(html);
            page.select(&JSON_LINK)
                .find(|l| l.value().attr("type").is_some_and(|t| types.contains(&t)))
                .and_then(|l| l.value().attr("href").map(str::to_owned))
        };
        if self.endpoint_url.is_none() {
            self.endpoint_url = href_of(&["application/json+oembed", "text/json+oembed"]);
        }
        if self.endpoint_url.is_some() {
            self.format = Some(Format::Json);
        }
        if self.format.is_none() {
            if self.endpoint_url.is_none() {
                self.endpoint_url = href_of(&["text/xml+oembed"]);
            }
            if self.endpoint_url.is_some() {
                self.format = Some(Format::Xml);
            }
        }
        let Some(endpoint) = self.endpoint_url.take().filter(|e| !super::is_blank(e)) else {
            return;
        };
        // An endpoint named over http on a page served over https is assumed
        // to be available over https as well.
        let Some(absolute) = Url::parse(url).ok().and_then(|base| {
            let mut absolute = base.join(&endpoint).ok()?;
            if base.scheme() == "https" {
                let _ = absolute.set_scheme("https");
            }
            Some(absolute)
        }) else {
            return;
        };
        let absolute = absolute.to_string();
        self.endpoint_url = Some(absolute.clone());
        self.cache_endpoint(state, url, &absolute).await;
    }

    async fn cache_endpoint(&self, state: &AppState, url: &str, endpoint: &str) {
        if !URL_REGEX.is_match(endpoint) {
            return;
        }
        let Some(domain) = Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
        else {
            return;
        };
        let cached = CachedEndpoint {
            endpoint: Some(URL_REGEX.replace_all(endpoint, "={url}").into_owned()),
            format: self.format,
        };
        let Ok(value) = serde_json::to_string(&cached) else {
            return;
        };
        let mut redis = state.redis.clone();
        let _: redis::RedisResult<()> = redis::cmd("SET")
            .arg(state.redis_keys.key(cache_key(&domain)))
            .arg(value)
            .arg("EX")
            .arg(ENDPOINT_CACHE_SECONDS)
            .query_async(&mut redis)
            .await;
    }

    async fn fetch(&self, state: &AppState) -> Result<Option<Map<String, Value>>, Abort> {
        let Some(endpoint) = self.endpoint_url.as_deref().filter(|e| !super::is_blank(e)) else {
            return Ok(None);
        };
        let endpoint = Url::parse(endpoint).map_err(|_| Abort)?;
        let response = state
            .fetch
            .request(reqwest::Method::GET, &endpoint)
            .map_err(|_| Abort)?
            .timeout(super::TIMEOUT)
            .send()
            .await
            .map_err(|_| Abort)?;
        if response.status() != reqwest::StatusCode::OK {
            return Ok(None);
        }
        // `body_with_limit`: more than the limit is an error, not a truncation.
        let body = super::read_body(response, BODY_LIMIT, false).await?;
        if body.is_empty() {
            return Ok(None);
        }
        let body = String::from_utf8_lossy(&body);
        let parsed = match self.format {
            Some(Format::Json) => match serde_json::from_str::<Value>(&body) {
                Ok(Value::Object(map)) => Some(map),
                _ => None,
            },
            Some(Format::Xml) => parse_xml(&body),
            None => None,
        };
        Ok(parsed.filter(valid))
    }
}

/// `validate`: version 1.0, and a type.
fn valid(oembed: &Map<String, Value>) -> bool {
    let version = match oembed.get("version") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    version == "1.0" && oembed.get("type").is_some_and(|t| super::present(Some(t)))
}

/// `Ox.load(body, mode: :hash_no_attrs).dig(:oembed)`: the `<oembed>`
/// element's children, by name, as text.
fn parse_xml(body: &str) -> Option<Map<String, Value>> {
    let doc = roxmltree::Document::parse(body).ok()?;
    let root = doc.root_element();
    if root.tag_name().name() != "oembed" {
        return None;
    }
    Some(
        root.children()
            .filter(roxmltree::Node::is_element)
            .map(|child| {
                (
                    child.tag_name().name().to_owned(),
                    Value::String(child.text().unwrap_or_default().to_owned()),
                )
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_oembed_reads_as_a_hash() {
        let map = parse_xml(
            r#"<?xml version="1.0"?><oembed><version>1.0</version><type>video</type><width>480</width></oembed>"#,
        )
        .unwrap();
        assert!(valid(&map));
        assert_eq!(map["width"], "480");
    }

    #[test]
    fn version_must_be_one_point_oh() {
        let map = |v: Value| {
            let mut m = Map::new();
            m.insert("version".into(), v);
            m.insert("type".into(), "link".into());
            m
        };
        assert!(valid(&map("1.0".into())));
        assert!(valid(&map(serde_json::json!(1.0))));
        assert!(!valid(&map(serde_json::json!(1))));
        assert!(!valid(&map("2.0".into())));
    }

    #[test]
    fn endpoints_are_cached_as_templates() {
        assert_eq!(
            URL_REGEX.replace_all(
                "https://www.youtube.com/oembed?format=json&url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3D1",
                "={url}"
            ),
            "https://www.youtube.com/oembed?format=json&url={url}"
        );
        assert!(!URL_REGEX.is_match("https://example.com/oembed/123.json"));
    }
}
