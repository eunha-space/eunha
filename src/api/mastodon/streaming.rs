//! Mastodon's streaming server (`streaming/index.js`), ported: the WebSocket
//! at `/api/v1/streaming` with its `subscribe` and `unsubscribe` messages, the
//! server-sent events endpoints under it, and `/api/v1/streaming/health`.
//!
//! Every connection is authenticated, as Mastodon's is. A connection listens
//! on the `timeline:*` channels of the instance's [`crate::streaming`] bus,
//! where the Rails side's messages are published, and passes them on; only
//! `update` and `status.update` on the public and hashtag streams are
//! filtered here, for the viewer's languages, blocks, mutes, domain blocks,
//! the feed access settings and keyword filters, since everything else was
//! filtered and rendered for the recipient before it was published.

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    body::{Body, Bytes},
    extract::ws::{
        rejection::WebSocketUpgradeRejection, CloseFrame, Message, WebSocket, WebSocketUpgrade,
    },
    extract::Request,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tracing::Instrument as _;

use crate::state::AppState;

/// `PERMISSION_VIEW_FEEDS`.
const PERMISSION_VIEW_FEEDS: i64 = 0x0000000000100000;

/// The WebSocket keep-alive: a ping every 30 seconds, and a connection that
/// did not answer the last one is terminated.
const WS_PING_EVERY: Duration = Duration::from_secs(30);

/// The event stream's `:thump` comment, every 15 seconds.
const SSE_HEARTBEAT_EVERY: Duration = Duration::from_secs(15);

// ── Errors (`streaming/errors.js`) ─────────────────────────────────────────

#[derive(Debug)]
enum StreamError {
    /// `AuthenticationError`: 401.
    Authentication(&'static str),
    /// `RequestError`: 400.
    Request(&'static str),
    /// Anything else: 500, `An unexpected error occurred`.
    Unexpected,
}

impl StreamError {
    /// `extractStatusAndMessage`.
    fn status_and_message(&self) -> (StatusCode, &'static str) {
        match self {
            Self::Authentication(message) => (StatusCode::UNAUTHORIZED, message),
            Self::Request(message) => (StatusCode::BAD_REQUEST, message),
            Self::Unexpected => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "An unexpected error occurred",
            ),
        }
    }

    /// `errorMiddleware`'s answer: `{"error": message}`.
    fn into_json_response(self) -> Response {
        let (status, message) = self.status_and_message();
        json_response(status, &serde_json::json!({ "error": message }))
    }
}

impl From<sqlx::Error> for StreamError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(%error, "streaming: database error");
        Self::Unexpected
    }
}

fn json_response(status: StatusCode, body: &Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

// ── The request (`req` and `ResolvedAccount`) ──────────────────────────────

/// Who is streaming: what `accountFromToken` reads, and the keyword filters
/// cached on the request.
struct Session {
    access_token_id: i64,
    scopes: Vec<String>,
    account_id: i64,
    chosen_languages: Option<Vec<String>>,
    permissions: i64,
    /// `req.cachedFilters`, dropped on `filters_changed`.
    cached_filters: Mutex<Option<Arc<BTreeMap<i64, CachedFilter>>>>,
}

impl Session {
    /// `isInScope`.
    fn is_in_scope(&self, necessary: &[&str]) -> bool {
        self.scopes.iter().any(|s| necessary.contains(&s.as_str()))
    }
}

/// The query string as `querystring.parse` reads it, first value of each key.
fn parse_query(query: Option<&str>) -> HashMap<String, String> {
    let mut parsed = HashMap::new();
    for (key, value) in url::form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
        parsed
            .entry(key.into_owned())
            .or_insert_with(|| value.into_owned());
    }
    parsed
}

/// `accountFromRequest`: the `Authorization` header, else the `access_token`
/// parameter, else the `Sec-WebSocket-Protocol` header.
async fn account_from_request(
    state: &AppState,
    headers: &HeaderMap,
    query: &HashMap<String, String>,
) -> Result<Session, StreamError> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let access_token = query
        .get("access_token")
        .filter(|t| !t.is_empty())
        .map(String::as_str)
        .or_else(|| {
            headers
                .get(header::SEC_WEBSOCKET_PROTOCOL)
                .and_then(|v| v.to_str().ok())
                .filter(|t| !t.is_empty())
        });
    let token = match (authorization, access_token) {
        (Some(authorization), _) => authorization
            .strip_prefix("Bearer ")
            .unwrap_or(authorization),
        (None, Some(token)) => token,
        (None, None) => return Err(StreamError::Authentication("Missing access token")),
    };
    account_from_token(state, token).await
}

/// `accountFromToken`.
async fn account_from_token(state: &AppState, token: &str) -> Result<Session, StreamError> {
    let row = sqlx::query!(
        r#"SELECT oauth_access_tokens.id, users.account_id, users.chosen_languages,
                  oauth_access_tokens.scopes, COALESCE(user_roles.permissions, 0) AS "permissions!"
           FROM oauth_access_tokens
           INNER JOIN users ON oauth_access_tokens.resource_owner_id = users.id
           INNER JOIN accounts ON accounts.id = users.account_id
           LEFT OUTER JOIN user_roles ON user_roles.id = users.role_id
           WHERE oauth_access_tokens.token = $1 AND oauth_access_tokens.revoked_at IS NULL
             AND users.disabled IS FALSE AND accounts.suspended_at IS NULL
           LIMIT 1"#,
        token,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(StreamError::Authentication("Invalid access token"))?;
    Ok(Session {
        access_token_id: row.id,
        scopes: row
            .scopes
            .unwrap_or_default()
            .split(' ')
            .map(str::to_owned)
            .collect(),
        account_id: row.account_id,
        chosen_languages: row.chosen_languages,
        permissions: row.permissions,
        cached_filters: Mutex::new(None),
    })
}

/// `checkScopes`: `read`, or `read:notifications` for the notifications
/// stream and `read:statuses` for any other.
fn check_scopes(session: &Session, channel_name: Option<&str>) -> Result<(), StreamError> {
    let narrow = if channel_name == Some("user:notification") {
        "read:notifications"
    } else {
        "read:statuses"
    };
    if session.is_in_scope(&["read", narrow]) {
        Ok(())
    } else {
        Err(StreamError::Authentication(
            "Access token does not have the required scopes",
        ))
    }
}

/// `channelNameFromPath`.
fn channel_name_from_path(path: &str, query: &HashMap<String, String>) -> Option<&'static str> {
    let only_media = query.get("only_media").is_some_and(|v| is_truthy(v));
    Some(match path {
        "/api/v1/streaming/user" => "user",
        "/api/v1/streaming/user/notification" => "user:notification",
        "/api/v1/streaming/public" if only_media => "public:media",
        "/api/v1/streaming/public" => "public",
        "/api/v1/streaming/public/local" if only_media => "public:local:media",
        "/api/v1/streaming/public/local" => "public:local",
        "/api/v1/streaming/public/remote" if only_media => "public:remote:media",
        "/api/v1/streaming/public/remote" => "public:remote",
        "/api/v1/streaming/hashtag" => "hashtag",
        "/api/v1/streaming/hashtag/local" => "hashtag:local",
        "/api/v1/streaming/direct" => "direct",
        "/api/v1/streaming/list" => "list",
        _ => return None,
    })
}

/// `isTruthy` (`utils.js`).
fn is_truthy(value: &str) -> bool {
    !value.is_empty() && !["0", "f", "F", "false", "FALSE", "off", "OFF"].contains(&value)
}

/// JavaScript truthiness, for the values of a parsed message.
fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `normalizeHashtag` (`utils.js`): NFKC, lower case, the ASCII folding of
/// `app/lib/ascii_folder.rb`, then only letters, numbers, `_`, `·` and ZWNJ.
fn normalize_hashtag(tag: &str) -> String {
    use unicode_normalization::UnicodeNormalization as _;
    const NON_ASCII_CHARS: &str = "ÀÁÂÃÄÅàáâãäåĀāĂăĄąÇçĆćĈĉĊċČčÐðĎďĐđÈÉÊËèéêëĒēĔĕĖėĘęĚěĜĝĞğĠġĢģĤĥĦħÌÍÎÏìíîïĨĩĪīĬĭĮįİıĴĵĶķĸĹĺĻļĽľĿŀŁłÑñŃńŅņŇňŉŊŋÒÓÔÕÖØòóôõöøŌōŎŏŐőŔŕŖŗŘřŚśŜŝŞşŠšſŢţŤťŦŧÙÚÛÜùúûüŨũŪūŬŭŮůŰűŲųŴŵÝýÿŶŷŸŹźŻżŽž";
    const EQUIVALENT_ASCII_CHARS: &str = "AAAAAAaaaaaaAaAaAaCcCcCcCcCcDdDdDdEEEEeeeeEeEeEeEeEeGgGgGgGgHhHhIIIIiiiiIiIiIiIiIiJjKkkLlLlLlLlLlNnNnNnNnnNnOOOOOOooooooOoOoOoRrRrRrSsSsSsSssTtTtTtUUUUuuuuUuUuUuUuUuUuWwYyyYyYZzZzZz";
    static INVALID: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"[^\p{L}\p{N}_\x{00b7}\x{200c}]").expect("valid regex")
    });
    let lowered = tag.nfkc().collect::<String>().to_lowercase();
    let folded: String = lowered
        .chars()
        .map(|c| match NON_ASCII_CHARS.chars().position(|n| n == c) {
            Some(i) => EQUIVALENT_ASCII_CHARS.chars().nth(i).unwrap_or(c),
            None => c,
        })
        .collect();
    INVALID.replace_all(&folded, "").into_owned()
}

/// What `streamFrom` is told about a channel's messages.
#[derive(Clone, Copy, Default)]
struct FilterOptions {
    needs_filtering: bool,
    filter_local: bool,
    filter_remote: bool,
}

/// `getFeedAccessSettings`: a feed whose setting is `disabled` is filtered out
/// for anyone whose role may not `view_feeds`.
async fn feed_access_settings(
    state: &AppState,
    kind: &str,
    session: &Session,
) -> Result<(bool, bool), StreamError> {
    if session.permissions & PERMISSION_VIEW_FEEDS != 0 {
        return Ok((true, true));
    }
    let (local_var, remote_var) = if kind == "hashtag" {
        ("local_topic_feed_access", "remote_topic_feed_access")
    } else {
        ("local_live_feed_access", "remote_live_feed_access")
    };
    let rows = sqlx::query!(
        "SELECT var, value FROM settings WHERE var IN ($1, $2)",
        local_var,
        remote_var
    )
    .fetch_all(&state.db)
    .await
    .map_err(|_| StreamError::Unexpected)?;
    let (mut local, mut remote) = (true, true);
    for row in rows {
        let access = row.value.as_deref() != Some("--- disabled\n");
        if row.var == local_var {
            local = access;
        } else {
            remote = access;
        }
    }
    Ok((local, remote))
}

/// `channelNameToIds`: the bus channels a stream listens on, and how their
/// messages are filtered.
async fn channel_name_to_ids(
    state: &AppState,
    session: &Session,
    name: Option<&str>,
    params: &Map<String, Value>,
) -> Result<(Vec<String>, FilterOptions), StreamError> {
    let feed = |kind: &'static str, channel: String| async move {
        let (local, remote) = feed_access_settings(state, kind, session).await?;
        Ok::<_, StreamError>((
            vec![channel],
            FilterOptions {
                needs_filtering: true,
                filter_local: !local,
                filter_remote: !remote,
            },
        ))
    };
    let account_id = session.account_id;
    match name {
        Some("user") => {
            // `channelsForUserStream`.
            let mut ids = vec![format!("timeline:{account_id}")];
            if session.is_in_scope(&["read", "read:notifications"]) {
                ids.push(format!("timeline:{account_id}:notifications"));
            }
            Ok((ids, FilterOptions::default()))
        }
        Some("user:notification") => Ok((
            vec![format!("timeline:{account_id}:notifications")],
            FilterOptions::default(),
        )),
        Some("public") => feed("public", "timeline:public".into()).await,
        Some("public:local") => feed("public", "timeline:public:local".into()).await,
        Some("public:remote") => feed("public", "timeline:public:remote".into()).await,
        Some("public:media") => feed("public", "timeline:public:media".into()).await,
        Some("public:local:media") => feed("public", "timeline:public:local:media".into()).await,
        Some("public:remote:media") => feed("public", "timeline:public:remote:media".into()).await,
        Some("direct") => Ok((
            vec![format!("timeline:direct:{account_id}")],
            FilterOptions::default(),
        )),
        Some(kind @ ("hashtag" | "hashtag:local")) => {
            let tag = params.get("tag").filter(|t| js_truthy(t));
            let Some(tag) = tag else {
                return Err(StreamError::Request("Missing tag name parameter"));
            };
            // `normalizeHashtag` of anything but a string throws.
            let tag = tag.as_str().ok_or(StreamError::Unexpected)?;
            let suffix = if kind == "hashtag:local" {
                ":local"
            } else {
                ""
            };
            feed(
                "hashtag",
                format!("timeline:hashtag:{}{suffix}", normalize_hashtag(tag)),
            )
            .await
        }
        Some("list") => {
            let Some(list) = params.get("list").filter(|l| js_truthy(l)) else {
                return Err(StreamError::Request("Missing list name parameter"));
            };
            // `authorizeListAccess`; an id the database cannot read is as
            // unauthorized as somebody else's list.
            let list_id = match list {
                Value::String(s) => s.trim().parse::<i64>().ok(),
                Value::Number(n) => n.as_i64(),
                _ => None,
            };
            let owned = match list_id {
                Some(id) => sqlx::query_scalar!(
                    r#"SELECT EXISTS (SELECT 1 FROM lists WHERE id = $1 AND account_id = $2) AS "e!""#,
                    id,
                    account_id
                )
                .fetch_one(&state.db)
                .await
                .unwrap_or(false),
                None => false,
            };
            match (owned, list_id) {
                (true, Some(id)) => Ok((
                    vec![format!("timeline:list:{id}")],
                    FilterOptions::default(),
                )),
                _ => Err(StreamError::Authentication(
                    "Not authorized to stream this list",
                )),
            }
        }
        _ => Err(StreamError::Request("Unknown stream type")),
    }
}

/// `streamNameFromChannelName`: the `stream` of each WebSocket message.
fn stream_name_from_channel_name(name: &str, params: &Map<String, Value>) -> Value {
    let mut stream = vec![Value::String(name.to_owned())];
    let extra = match name {
        "list" => params.get("list"),
        "hashtag" | "hashtag:local" => params.get("tag"),
        _ => None,
    };
    if let Some(extra) = extra.filter(|v| js_truthy(v)) {
        stream.push(extra.clone());
    }
    Value::Array(stream)
}

// ── Keyword filters ────────────────────────────────────────────────────────

/// One of `req.cachedFilters`.
struct CachedFilter {
    regexp: Option<regex::Regex>,
    expires_at: Option<chrono::NaiveDateTime>,
    filter: Value,
}

/// The filters of the viewer that have keywords and have not expired, keyed
/// and shaped as the streaming server caches them.
async fn load_filters(
    state: &AppState,
    account_id: i64,
) -> Result<BTreeMap<i64, CachedFilter>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"SELECT filter.id AS id, filter.phrase AS title, filter.context AS context,
                  filter.expires_at AS expires_at, filter.action AS filter_action,
                  keyword.keyword AS keyword, keyword.whole_word AS whole_word
           FROM custom_filter_keywords keyword
           JOIN custom_filters filter ON keyword.custom_filter_id = filter.id
           WHERE filter.account_id = $1 AND (filter.expires_at IS NULL OR filter.expires_at > NOW())"#,
        account_id
    )
    .fetch_all(&state.db)
    .await?;
    let mut keywords: BTreeMap<i64, Vec<(String, bool)>> = BTreeMap::new();
    let mut filters: BTreeMap<i64, CachedFilter> = BTreeMap::new();
    for row in rows {
        keywords
            .entry(row.id)
            .or_default()
            .push((row.keyword, row.whole_word));
        filters.entry(row.id).or_insert_with(|| {
            let mut filter = serde_json::json!({
                "id": row.id.to_string(),
                "title": row.title,
                "context": row.context,
                "expires_at": row.expires_at.map(js_date),
            });
            // `['warn', 'hide'][filter.filter_action]`: `blur` comes out
            // undefined, and `JSON.stringify` leaves it out.
            match row.filter_action {
                0 => filter["filter_action"] = "warn".into(),
                1 => filter["filter_action"] = "hide".into(),
                _ => {}
            }
            CachedFilter {
                regexp: None,
                expires_at: row.expires_at,
                filter,
            }
        });
    }
    for (id, filter) in filters.iter_mut() {
        let alternatives: Vec<String> = keywords[id]
            .iter()
            .map(|(keyword, whole_word)| {
                let mut expr = regex::escape(keyword);
                if *whole_word {
                    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
                    if keyword.chars().next().is_some_and(word) {
                        expr = format!(r"(?-u:\b){expr}");
                    }
                    if keyword.chars().last().is_some_and(word) {
                        expr = format!(r"{expr}(?-u:\b)");
                    }
                }
                expr
            })
            .collect();
        filter.regexp = regex::Regex::new(&format!("(?i){}", alternatives.join("|"))).ok();
    }
    Ok(filters)
}

/// A `Date` as `JSON.stringify` writes it.
fn js_date(at: chrono::NaiveDateTime) -> String {
    at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// The text the keyword filters are matched against: the spoiler, the
/// content, the poll's options and the media descriptions, as the DOM reads
/// them.
fn searchable_text(status: &Value) -> String {
    static BR: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"<br\s*/?>").expect("valid regex"));
    let text = |v: &Value| match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let spoiler = match &status["spoiler_text"] {
        v if js_truthy(v) => text(v),
        _ => String::new(),
    };
    let mut parts = vec![spoiler, text(&status["content"])];
    if let Some(options) = status["poll"]["options"].as_array() {
        parts.extend(options.iter().map(|o| text(&o["title"])));
    }
    if let Some(media) = status["media_attachments"].as_array() {
        parts.extend(media.iter().map(|m| text(&m["description"])));
    }
    let html = BR
        .replace_all(&parts.join("\n\n"), "\n")
        .replace("</p><p>", "\n\n");
    scraper::Html::parse_fragment(&html)
        .root_element()
        .text()
        .collect()
}

/// The `FilterResult`s of the viewer's keyword filters that match.
fn filter_results(filters: &BTreeMap<i64, CachedFilter>, status: &Value) -> Vec<Value> {
    let content = searchable_text(status);
    let now = chrono::Utc::now().naive_utc();
    let mut results = vec![];
    for cached in filters.values() {
        if cached.expires_at.is_some_and(|at| at < now) || content.is_empty() {
            continue;
        }
        if let Some(found) = cached.regexp.as_ref().and_then(|r| r.find(&content)) {
            results.push(serde_json::json!({
                "filter": cached.filter,
                "keyword_matches": [found.as_str()],
                "status_matches": null,
            }));
        }
    }
    results
}

// ── Listening (`streamFrom`) ───────────────────────────────────────────────

/// Where a listener's messages go.
#[derive(Clone)]
enum Output {
    /// `streamToWs`, with the stream's name.
    Ws {
        out: mpsc::UnboundedSender<Outgoing>,
        stream: Arc<Value>,
    },
    /// `streamToHttp`.
    Sse {
        out: mpsc::UnboundedSender<Outgoing>,
    },
}

enum Outgoing {
    Text(String),
    /// `onKill`.
    Kill,
}

impl Output {
    fn send(&self, event: &str, payload: &Value) -> bool {
        // `transmit`: an object goes as its JSON, a string as it is.
        let encoded = match payload {
            Value::Object(_) | Value::Array(_) | Value::Null => Value::String(payload.to_string()),
            other => other.clone(),
        };
        match self {
            Self::Ws { out, stream } => {
                let message = format!(
                    r#"{{"stream":{},"event":{},"payload":{}}}"#,
                    stream,
                    Value::String(event.to_owned()),
                    encoded,
                );
                out.send(Outgoing::Text(message)).is_ok()
            }
            Self::Sse { out } => {
                let data = match &encoded {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                out.send(Outgoing::Text(format!("event: {event}\ndata: {data}\n\n")))
                    .is_ok()
            }
        }
    }
}

/// A spawned listener, stopped when dropped: `unsubscribe`.
struct Listener(tokio::task::JoinHandle<()>);

impl Drop for Listener {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// `streamFrom`: one listener per channel.
fn stream_from(
    state: &AppState,
    session: &Arc<Session>,
    channel_ids: &[String],
    output: Output,
    options: FilterOptions,
) -> Vec<Listener> {
    channel_ids
        .iter()
        .map(|channel| {
            let mut subscription = state.streaming.subscribe(channel);
            let (state, session, output) = (state.clone(), session.clone(), output.clone());
            Listener(crate::tenants::spawn(async move {
                while let Some(message) = subscription.recv().await {
                    if let Some((event, payload)) =
                        listen(&state, &session, options, &message).await
                    {
                        if !output.send(&event, &payload) {
                            break;
                        }
                    }
                }
            }))
        })
        .collect()
}

/// `streamFrom`'s listener: what of a message reaches the client.
async fn listen(
    state: &AppState,
    session: &Session,
    options: FilterOptions,
    message: &Value,
) -> Option<(String, Value)> {
    let event = message.get("event").filter(|e| js_truthy(e))?;
    let payload = message.get("payload").filter(|p| js_truthy(p))?;
    let event = match event {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if !options.needs_filtering || (event != "update" && event != "status.update") {
        return Some((event, payload.clone()));
    }

    let account = &payload["account"];
    let local_payload = account["username"] == account["acct"];
    if if local_payload {
        options.filter_local
    } else {
        options.filter_remote
    } {
        return None;
    }
    if let Some(languages) = &session.chosen_languages {
        let language = payload["language"].as_str();
        if !language.is_some_and(|l| languages.iter().any(|c| c == l)) {
            return None;
        }
    }

    // Blocks, mutes and domain blocks, of the author and everyone mentioned.
    let author_id = id_of(&account["id"])?;
    let mut targets = vec![author_id];
    if let Some(mentions) = payload["mentions"].as_array() {
        targets.extend(mentions.iter().filter_map(|m| id_of(&m["id"])));
    }
    let blocked = sqlx::query_scalar!(
        r#"SELECT EXISTS (
             SELECT 1 FROM blocks
             WHERE (account_id = $1 AND target_account_id = ANY($3))
                OR (account_id = $2 AND target_account_id = $1)
             UNION
             SELECT 1 FROM mutes WHERE account_id = $1 AND target_account_id = ANY($3)
           ) AS "e!""#,
        session.account_id,
        author_id,
        &targets,
    )
    .fetch_one(&state.db)
    .await
    .map_err(|error| tracing::error!(%error, "streaming: could not check blocks"))
    .ok()?;
    if blocked {
        return None;
    }
    if let Some(domain) = account["acct"].as_str().and_then(|a| a.split('@').nth(1)) {
        let domain_blocked = sqlx::query_scalar!(
            r#"SELECT EXISTS (SELECT 1 FROM account_domain_blocks WHERE account_id = $1 AND domain = $2) AS "e!""#,
            session.account_id,
            domain,
        )
        .fetch_one(&state.db)
        .await
        .ok()?;
        if domain_blocked {
            return None;
        }
    }

    // A payload rendered for the viewer already says what filters it.
    if payload.get("filtered").is_some() {
        return Some((event, payload.clone()));
    }
    let cached = session
        .cached_filters
        .lock()
        .expect("cached filters lock")
        .clone();
    let filters = match cached {
        Some(filters) => filters,
        None => {
            let loaded = Arc::new(load_filters(state, session.account_id).await.ok()?);
            *session.cached_filters.lock().expect("cached filters lock") = Some(loaded.clone());
            loaded
        }
    };
    let results = filter_results(&filters, payload);
    let mut payload = payload.clone();
    payload
        .as_object_mut()?
        .insert("filtered".into(), Value::Array(results));
    Some((event, payload))
}

fn id_of(value: &Value) -> Option<i64> {
    match value {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_i64(),
        _ => None,
    }
}

/// `subscribeHttpToSystemChannel` and `subscribeWebsocketToSystemChannel`:
/// `kill` on the token's or the account's system channel ends the
/// connection, and `filters_changed` drops the cached filters.
fn subscribe_to_system_channels(
    state: &AppState,
    session: &Arc<Session>,
    out: mpsc::UnboundedSender<Outgoing>,
) -> Vec<Listener> {
    [
        format!("timeline:access_token:{}", session.access_token_id),
        format!("timeline:system:{}", session.account_id),
    ]
    .into_iter()
    .map(|channel| {
        let mut subscription = state.streaming.subscribe(&channel);
        let (session, out) = (session.clone(), out.clone());
        Listener(crate::tenants::spawn(async move {
            while let Some(message) = subscription.recv().await {
                match message.get("event").and_then(Value::as_str) {
                    Some("kill") => {
                        let _ = out.send(Outgoing::Kill);
                    }
                    Some("filters_changed") => {
                        *session.cached_filters.lock().expect("cached filters lock") = None;
                    }
                    _ => {}
                }
            }
        }))
    })
    .collect()
}

// ── Routes ─────────────────────────────────────────────────────────────────

/// `GET /api/v1/streaming/health`.
pub async fn health() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/plain"),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        "OK",
    )
        .into_response()
}

/// Everything else under `/api/v1/streaming`: a WebSocket upgrade, on any
/// path, as the streaming server takes one; otherwise the event stream the
/// path names.
pub async fn handler(
    state: AppState,
    ws: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    request: Request,
) -> Response {
    let query = parse_query(request.uri().query());
    let headers = request.headers().clone();
    match ws {
        Ok(ws) => websocket(state, ws, headers, query).await,
        Err(_) => event_stream(state, request.uri().path(), headers, query).await,
    }
}

// ── Server-sent events ─────────────────────────────────────────────────────

/// `authenticationMiddleware`, then the `/api/v1/streaming/*splat` route.
async fn event_stream(
    state: AppState,
    path: &str,
    headers: HeaderMap,
    query: HashMap<String, String>,
) -> Response {
    let Some(channel_name) = channel_name_from_path(path, &query) else {
        return StreamError::Request("Unknown channel requested").into_json_response();
    };
    let session = match account_from_request(&state, &headers, &query).await {
        Ok(session) => Arc::new(session),
        Err(error) => return error.into_json_response(),
    };
    if let Err(error) = check_scopes(&session, Some(channel_name)) {
        return error.into_json_response();
    }
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let system = subscribe_to_system_channels(&state, &session, out_tx.clone());

    let params: Map<String, Value> = query
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    let (channel_ids, options) =
        match channel_name_to_ids(&state, &session, Some(channel_name), &params).await {
            Ok(resolved) => resolved,
            Err(error) => return error.into_json_response(),
        };
    let listeners = stream_from(
        &state,
        &session,
        &channel_ids,
        Output::Sse { out: out_tx },
        options,
    );
    tracing::debug!(
        ?channel_ids,
        account_id = session.account_id,
        "streaming: event stream"
    );

    let (body_tx, mut body_rx) = mpsc::channel::<Bytes>(64);
    let stop = state.stop.clone();
    crate::tenants::spawn(async move {
        let _held = (system, listeners);
        if body_tx.send(Bytes::from_static(b":)\n")).await.is_err() {
            return;
        }
        let mut heartbeat = tokio::time::interval(SSE_HEARTBEAT_EVERY);
        heartbeat.tick().await;
        loop {
            tokio::select! {
                () = stop.cancelled() => break,
                () = body_tx.closed() => break,
                outgoing = out_rx.recv() => match outgoing {
                    Some(Outgoing::Text(text)) => {
                        if body_tx.send(Bytes::from(text)).await.is_err() {
                            break;
                        }
                    }
                    // `res.end()`.
                    Some(Outgoing::Kill) | None => break,
                },
                _ = heartbeat.tick() => {
                    if body_tx.send(Bytes::from_static(b":thump\n\n")).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    let body = futures::stream::poll_fn(move |cx| {
        body_rx
            .poll_recv(cx)
            .map(|chunk| chunk.map(Ok::<_, Infallible>))
    });
    let mut response = Response::new(Body::from_stream(body));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

// ── WebSocket ──────────────────────────────────────────────────────────────

/// The `upgrade` handler: authenticate first, and answer a refusal with a bare
/// HTTP response naming the error in `X-Error-Message`.
async fn websocket(
    state: AppState,
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    query: HashMap<String, String>,
) -> Response {
    let session = match account_from_request(&state, &headers, &query).await {
        Ok(session) => Arc::new(session),
        Err(error) => {
            let (status, message) = error.status_and_message();
            return (
                status,
                [
                    (header::CONNECTION, "close"),
                    (header::CONTENT_TYPE, "text/plain"),
                    (header::HeaderName::from_static("x-error-message"), message),
                ],
            )
                .into_response();
        }
    };
    // `ws` answers with the first subprotocol the client offered, which is
    // how clients that pass the token as the subprotocol get it accepted.
    let protocol = headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').map(str::trim).find(|p| !p.is_empty()))
        .map(str::to_owned);
    let ws = match protocol {
        Some(protocol) => ws.protocols([protocol]),
        None => ws,
    };
    // The upgraded connection runs in a task axum spawns, outside this
    // request's span; take the tenant along.
    let span = tracing::Span::current();
    ws.on_upgrade(move |socket| {
        async move {
            tracing::debug!(account_id = session.account_id, "streaming: websocket open");
            on_connection(socket, state, session, query).await;
            tracing::debug!("streaming: websocket closed");
        }
        .instrument(span)
    })
}

/// A WebSocket's subscriptions: their listeners, keyed by their channels
/// joined with `;`.
type Subscribed = HashMap<String, Vec<Listener>>;

/// `onConnection`.
async fn on_connection(
    mut socket: WebSocket,
    state: AppState,
    session: Arc<Session>,
    query: HashMap<String, String>,
) {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel();
    let mut subscriptions = Subscribed::new();
    let _system = subscribe_to_system_channels(&state, &session, out_tx.clone());

    if let Some(stream) = query.get("stream").filter(|s| !s.is_empty()) {
        let params: Map<String, Value> = query
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        if let Some(error) = subscribe(
            &state,
            &session,
            &out_tx,
            &mut subscriptions,
            Some(stream.as_str()),
            &params,
        )
        .await
        {
            if socket.send(Message::Text(error.into())).await.is_err() {
                return;
            }
        }
    }

    let mut alive = true;
    let mut ping = tokio::time::interval(WS_PING_EVERY);
    ping.tick().await;
    loop {
        tokio::select! {
            () = state.stop.cancelled() => {
                // The instance is being stopped. Closing tells the client to
                // reconnect, which reaches whatever serves its host now.
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
            outgoing = out_rx.recv() => match outgoing {
                Some(Outgoing::Text(text)) => {
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                // `websocket.close()`.
                Some(Outgoing::Kill) | None => {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
            },
            message = socket.recv() => match message {
                Some(Ok(Message::Text(text))) => {
                    // Anything that is not a JSON object is logged and ignored.
                    let Ok(Value::Object(mut json)) = serde_json::from_str::<Value>(&text) else {
                        tracing::debug!("streaming: unparseable message");
                        continue;
                    };
                    let kind = json.remove("type");
                    let stream = json.remove("stream").map(|s| match s {
                        // `firstParam`.
                        Value::Array(items) => items.into_iter().next().unwrap_or(Value::Null),
                        other => other,
                    });
                    let channel_name = stream.as_ref().and_then(Value::as_str);
                    let reply = match kind.as_ref().and_then(Value::as_str) {
                        Some("subscribe") => {
                            subscribe(&state, &session, &out_tx, &mut subscriptions, channel_name, &json).await
                        }
                        Some("unsubscribe") => {
                            match channel_name_to_ids(&state, &session, channel_name, &json).await {
                                Ok((channel_ids, _)) => {
                                    subscriptions.remove(&channel_ids.join(";"));
                                    None
                                }
                                Err(_) => Some(r#"{"error":"Error unsubscribing from channel"}"#.to_owned()),
                            }
                        }
                        _ => None,
                    };
                    if let Some(reply) = reply {
                        if socket.send(Message::Text(reply.into())).await.is_err() {
                            break;
                        }
                    }
                }
                Some(Ok(Message::Binary(_))) => {
                    let _ = socket
                        .send(Message::Close(Some(CloseFrame {
                            code: 1003,
                            reason: "The mastodon streaming server does not support binary messages".into(),
                        })))
                        .await;
                    break;
                }
                Some(Ok(Message::Pong(_))) => alive = true,
                Some(Ok(Message::Ping(payload))) => {
                    let _ = socket.send(Message::Pong(payload)).await;
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
            },
            _ = ping.tick() => {
                // Did not answer the last ping: `ws.terminate()`.
                if !alive {
                    break;
                }
                alive = false;
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// `subscribeWebsocketToChannel`; the error frame to send, if it failed.
async fn subscribe(
    state: &AppState,
    session: &Arc<Session>,
    out: &mpsc::UnboundedSender<Outgoing>,
    subscriptions: &mut Subscribed,
    channel_name: Option<&str>,
    params: &Map<String, Value>,
) -> Option<String> {
    let resolved = match check_scopes(session, channel_name) {
        Ok(()) => channel_name_to_ids(state, session, channel_name, params).await,
        Err(error) => Err(error),
    };
    let (channel_ids, options) = match resolved {
        Ok(resolved) => resolved,
        Err(error) => {
            let (status, message) = error.status_and_message();
            return Some(format!(
                r#"{{"error":{},"status":{}}}"#,
                Value::String(message.to_owned()),
                status.as_u16()
            ));
        }
    };
    let key = channel_ids.join(";");
    if subscriptions.contains_key(&key) {
        return None;
    }
    let channel_name = channel_name.unwrap_or_default();
    let output = Output::Ws {
        out: out.clone(),
        stream: Arc::new(stream_name_from_channel_name(channel_name, params)),
    };
    let listeners = stream_from(state, session, &channel_ids, output, options);
    subscriptions.insert(key, listeners);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_hashtags_as_the_streaming_server_does() {
        assert_eq!(normalize_hashtag("Café"), "cafe");
        assert_eq!(normalize_hashtag("ＲＵＳＴ"), "rust");
        assert_eq!(normalize_hashtag("foo-bar!"), "foobar");
        assert_eq!(normalize_hashtag("한국어"), "한국어");
    }

    #[test]
    fn reads_only_media_as_is_truthy_does() {
        assert!(is_truthy("1"));
        assert!(is_truthy("true"));
        assert!(!is_truthy("0"));
        assert!(!is_truthy("false"));
        assert!(!is_truthy(""));
    }

    #[test]
    fn searches_what_the_dom_reads_of_a_status() {
        let status = serde_json::json!({
            "spoiler_text": "",
            "content": "<p>Hello &amp; welcome<br>to <a href=\"x\">#rust</a></p><p>again</p>",
            "poll": {"options": [{"title": "yes"}]},
            "media_attachments": [{"description": null}],
        });
        assert_eq!(
            searchable_text(&status),
            "\n\nHello & welcome\nto #rust\n\nagain\n\nyes\n\n"
        );
    }
}
