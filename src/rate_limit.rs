//! Mastodon's rate limits: the `Rack::Attack` throttles
//! (*config/initializers/rack_attack.rb*) every request passes, and the
//! `RateLimiter` families (*app/lib/rate_limiter.rb*) that count what an
//! account creates through the API.
//!
//! A throttle counts requests per period in `Rails.cache`, which Mastodon
//! keeps in Redis under the `cache` namespace:
//! `cache:rack::attack:<epoch / period>:<name>:<discriminator>`. A family
//! counts in Redis itself, `rate_limit:<account id>:<family>:<epoch /
//! period>`. Both are under the instance's key prefix, so each instance a
//! process serves has its own counts.

use axum::{
    body::Body,
    extract::Request,
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use regex::Regex;

use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

/// `I18n.t('errors.429')`, in `I18n.default_locale`: both the throttled
/// responder and `Api::ErrorHandling` run outside the controller's
/// `set_locale`. Eunha has the message in English and Korean, and gives any
/// other locale the English one (`rate-limit-message-in-english-and-korean`).
#[must_use]
pub fn too_many_requests(default_locale: &str) -> &'static str {
    let locale = if default_locale == "ko" {
        crate::locale::Locale::Ko
    } else {
        crate::locale::Locale::En
    };
    locale.t("errors.429")
}

// ── RateLimiter ────────────────────────────────────────────────────────────

/// One of `RateLimiter::FAMILIES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Family {
    pub name: &'static str,
    pub limit: i64,
    /// In seconds.
    pub period: i64,
}

pub const FOLLOWS: Family = Family {
    name: "follows",
    limit: 400,
    period: 24 * 3600,
};
pub const STATUSES: Family = Family {
    name: "statuses",
    limit: 300,
    period: 3 * 3600,
};
pub const REPORTS: Family = Family {
    name: "reports",
    limit: 400,
    period: 24 * 3600,
};

/// What a request may record: its account, and the family its route counts
/// in (`with_rate_limit: true`, `rate_limit: true`).
#[derive(Debug, Clone, Copy)]
struct Scope {
    account_id: i64,
    family: Family,
}

tokio::task_local! {
    static SCOPE: Scope;
}

fn family_key(state: &AppState, account_id: i64, family: Family, epoch: i64) -> String {
    state.redis_keys.key(format!(
        "rate_limit:{account_id}:{}:{}",
        family.name,
        epoch / family.period
    ))
}

/// `RateLimitable`'s `after_create`: count something `account_id` created
/// in `family`, or refuse it with `Mastodon::RateLimitExceededError` once
/// the period's limit is reached. Only what the API asked to be rate
/// limited counts — a status posted, an edit, a follow or follow request, a
/// hashtag followed — so outside such a request this does nothing.
pub async fn record(state: &AppState, account_id: i64, family: Family) -> AppResult<()> {
    if !in_scope(account_id, family) {
        return Ok(());
    }
    record_at(state, account_id, family, Utc::now().timestamp()).await
}

fn in_scope(account_id: i64, family: Family) -> bool {
    SCOPE
        .try_with(|scope| scope.account_id == account_id && scope.family == family)
        .unwrap_or(false)
}

/// `RateLimiter#record!`.
async fn record_at(state: &AppState, account_id: i64, family: Family, epoch: i64) -> AppResult<()> {
    let key = family_key(state, account_id, family, epoch);
    let mut redis = state.redis.clone();
    let count: Option<i64> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    if count.is_none() {
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg(0)
            .query_async(&mut redis)
            .await
            .map_err(|e| AppError::Internal(e.into()))?;
        let _: () = redis::cmd("EXPIRE")
            .arg(&key)
            .arg(family.period - epoch % family.period + 1)
            .query_async(&mut redis)
            .await
            .map_err(|e| AppError::Internal(e.into()))?;
    }
    if count.is_some_and(|count| count >= family.limit) {
        return Err(AppError::TooManyRequests(
            too_many_requests(state.instance.default_locale()).to_owned(),
        ));
    }
    let _: i64 = redis::cmd("INCRBY")
        .arg(&key)
        .arg(1)
        .query_async(&mut redis)
        .await
        .map_err(|e| AppError::Internal(e.into()))?;
    Ok(())
}

/// [`record`] for a hashtag follow, which `TagFollow.find_or_create_by!`
/// only creates, and so only counts, when the account does not follow it
/// yet.
pub async fn record_tag_follow(state: &AppState, account_id: i64, tag_id: i64) -> AppResult<()> {
    if !in_scope(account_id, FOLLOWS) {
        return Ok(());
    }
    let following = sqlx::query_scalar!(
        r#"SELECT EXISTS(SELECT 1 FROM tag_follows WHERE account_id = $1 AND tag_id = $2) AS "e!""#,
        account_id,
        tag_id,
    )
    .fetch_one(&state.db)
    .await?;
    if following {
        return Ok(());
    }
    record(state, account_id, FOLLOWS).await
}

/// `RateLimiter#to_headers`.
async fn family_headers(
    state: &AppState,
    account_id: i64,
    family: Family,
    now: DateTime<Utc>,
) -> (i64, String) {
    let key = family_key(state, account_id, family, now.timestamp());
    let mut redis = state.redis.clone();
    let count: Option<i64> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .ok()
        .flatten();
    (family.limit - count.unwrap_or(0), reset(now, family.period))
}

/// The routes `override_rate_limit_headers` names, and whether what they
/// create is counted.
fn family_route(method: &Method, path: &str) -> Option<(Family, bool)> {
    static STATUS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\A/api/v1/statuses/[^/]+\z").unwrap());
    static REBLOG: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"\A/api/v1/statuses/[^/]+/reblog\z").unwrap());
    static FOLLOW: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"\A/api/v1/(accounts|tags)/[^/]+/follow\z").unwrap());
    match *method {
        // `Api::V1::StatusesController#create`, with `with_rate_limit: true`.
        Method::POST if path == "/api/v1/statuses" => Some((STATUSES, true)),
        // `#update`: the `StatusEdit` it makes counts.
        Method::PUT if STATUS.is_match(path) => Some((STATUSES, true)),
        // `ReblogsController#create`: the headers, but no `with_rate_limit`.
        Method::POST if REBLOG.is_match(path) => Some((STATUSES, false)),
        // `AccountsController#follow` and `TagsController#follow`.
        Method::POST if FOLLOW.is_match(path) => Some((FOLLOWS, true)),
        // `ReportsController#create`: the headers alone.
        Method::POST if path == "/api/v1/reports" => Some((REPORTS, false)),
        _ => None,
    }
}

// ── Rack::Attack ───────────────────────────────────────────────────────────

/// What the throttles ask of a request (`Rack::Attack::Request`).
#[derive(Debug, Default)]
struct Facts {
    method: Method,
    path: String,
    /// `authenticated_user_id`: the user behind the access token, whether or
    /// not the token is still good.
    user_id: Option<i64>,
    /// `authenticated_token_id`.
    token_id: Option<i64>,
    /// `throttleable_remote_ip`: the client's address, an IPv6 one masked
    /// to its /64.
    ip: Option<String>,
    paging: bool,
    /// `params.dig('user', 'email')`.
    email: Option<String>,
    /// Whether the form signs in: an email, a password or a pending
    /// sign-in, as eunha's authorization page tells a sign-in from its
    /// buttons.
    credentials: bool,
    /// `session[:attempt_user_id]`.
    attempt_user_id: Option<i64>,
    /// `warden.user.id`: the user signed in to the browser session.
    warden_user_id: Option<i64>,
}

impl Facts {
    fn api(&self) -> bool {
        self.path.starts_with("/api")
    }

    fn unauthenticated(&self) -> bool {
        self.user_id.is_none()
    }

    fn post(&self) -> bool {
        self.method == Method::POST
    }

    fn put_or_patch(&self) -> bool {
        self.method == Method::PUT || self.method == Method::PATCH
    }

    /// `path_matches?`: the path, or it with a format.
    fn path_matches(&self, other: &str) -> bool {
        self.path
            .strip_prefix(other)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    }

    /// `POST /auth/sign_in`, which eunha takes at `/account/login` and, with
    /// a password or a pending sign-in, at `/oauth/authorize`.
    fn sign_in(&self) -> bool {
        self.post()
            && (self.path_matches("/auth/sign_in")
                || self.path == "/account/login"
                || (self.path == "/oauth/authorize" && self.credentials))
    }

    /// `PUT /auth/setup`, which eunha's form also takes as a `POST`.
    fn auth_setup(&self) -> bool {
        (self.put_or_patch() || self.post()) && self.path_matches("/auth/setup")
    }

    /// `PUT /auth` and `PUT /auth/password`: a password changed, which
    /// eunha's account page posts to `/account/password`.
    fn password_change(&self) -> bool {
        (self.put_or_patch() && (self.path_matches("/auth") || self.path_matches("/auth/password")))
            || (self.post() && self.path == "/account/password")
    }
}

type Discriminator = fn(&Facts) -> Option<String>;

struct Throttle {
    name: &'static str,
    limit: i64,
    period: i64,
    discriminator: Discriminator,
}

fn id(id: Option<i64>) -> Option<String> {
    id.map(|id| id.to_string())
}

/// Mastodon's throttles, in the order it declares them: a request is
/// counted by each until one has had too many.
static THROTTLES: [Throttle; 20] = [
    Throttle {
        name: "throttle_authenticated_api",
        limit: 1_500,
        period: 300,
        discriminator: |r| id(r.user_id).filter(|_| r.api()),
    },
    Throttle {
        name: "throttle_per_token_api",
        limit: 300,
        period: 300,
        discriminator: |r| id(r.token_id).filter(|_| r.api()),
    },
    Throttle {
        name: "throttle_unauthenticated_api",
        limit: 300,
        period: 300,
        discriminator: |r| r.ip.clone().filter(|_| r.api() && r.unauthenticated()),
    },
    Throttle {
        name: "throttle_api_media",
        limit: 30,
        period: 1800,
        discriminator: |r| {
            static MEDIA: Lazy<Regex> =
                Lazy::new(|| Regex::new(r"(?i)\A/api/v\d+/media\z").unwrap());
            id(r.user_id).filter(|_| r.post() && MEDIA.is_match(&r.path))
        },
    },
    Throttle {
        name: "throttle_media_proxy",
        limit: 30,
        period: 600,
        discriminator: |r| r.ip.clone().filter(|_| r.path.starts_with("/media_proxy")),
    },
    Throttle {
        name: "throttle_api_sign_up",
        limit: 5,
        period: 1800,
        discriminator: |r| {
            r.ip.clone()
                .filter(|_| r.post() && r.path == "/api/v1/accounts")
        },
    },
    Throttle {
        name: "throttle_authenticated_paging",
        limit: 300,
        period: 900,
        discriminator: |r| id(r.user_id).filter(|_| r.paging),
    },
    Throttle {
        name: "throttle_unauthenticated_paging",
        limit: 300,
        period: 900,
        discriminator: |r| r.ip.clone().filter(|_| r.paging && r.unauthenticated()),
    },
    Throttle {
        name: "throttle_api_delete",
        limit: 30,
        period: 1800,
        discriminator: |r| {
            static UNREBLOG: Lazy<Regex> =
                Lazy::new(|| Regex::new(r"\A/api/v1/statuses/\d+/unreblog\z").unwrap());
            static STATUS: Lazy<Regex> =
                Lazy::new(|| Regex::new(r"\A/api/v1/statuses/\d+\z").unwrap());
            id(r.user_id).filter(|_| {
                (r.post() && UNREBLOG.is_match(&r.path))
                    || (r.method == Method::DELETE && STATUS.is_match(&r.path))
            })
        },
    },
    Throttle {
        name: "throttle_oauth_application_registrations/ip",
        limit: 5,
        period: 600,
        discriminator: |r| {
            r.ip.clone()
                .filter(|_| r.post() && r.path == "/api/v1/apps")
        },
    },
    Throttle {
        name: "throttle_sign_up_attempts/ip",
        limit: 25,
        period: 300,
        discriminator: |r| r.ip.clone().filter(|_| r.post() && r.path_matches("/auth")),
    },
    Throttle {
        name: "throttle_password_resets/ip",
        limit: 25,
        period: 300,
        discriminator: |r| {
            r.ip.clone()
                .filter(|_| r.post() && r.path_matches("/auth/password"))
        },
    },
    Throttle {
        name: "throttle_password_resets/email",
        limit: 5,
        period: 1800,
        discriminator: |r| {
            r.email
                .clone()
                .filter(|_| r.post() && r.path_matches("/auth/password"))
        },
    },
    Throttle {
        name: "throttle_email_confirmations/ip",
        limit: 25,
        period: 300,
        discriminator: |r| {
            r.ip.clone().filter(|_| {
                (r.post()
                    && (r.path_matches("/auth/confirmation")
                        || r.path == "/api/v1/emails/confirmations"))
                    || r.auth_setup()
            })
        },
    },
    Throttle {
        name: "throttle_email_confirmations/email",
        limit: 5,
        period: 1800,
        discriminator: |r| {
            if r.post() && r.path_matches("/auth/confirmation") {
                r.email.clone()
            } else if r.post() && r.path == "/api/v1/emails/confirmations" {
                id(r.user_id)
            } else {
                None
            }
        },
    },
    Throttle {
        name: "throttle_auth_setup/email",
        limit: 5,
        period: 600,
        discriminator: |r| r.email.clone().filter(|_| r.auth_setup()),
    },
    Throttle {
        name: "throttle_auth_setup/account",
        limit: 5,
        period: 600,
        discriminator: |r| id(r.warden_user_id).filter(|_| r.auth_setup()),
    },
    Throttle {
        name: "throttle_login_attempts/ip",
        limit: 25,
        period: 300,
        discriminator: |r| r.ip.clone().filter(|_| r.sign_in()),
    },
    Throttle {
        name: "throttle_login_attempts/email",
        limit: 25,
        period: 3600,
        discriminator: |r| {
            id(r.attempt_user_id)
                .or_else(|| r.email.clone())
                .filter(|_| r.sign_in())
        },
    },
    Throttle {
        name: "throttle_password_change/account",
        limit: 10,
        period: 600,
        discriminator: |r| id(r.warden_user_id).filter(|_| r.password_change()),
    },
];

/// The token a request carried, put there by the authentication middleware
/// whether or not the token is still good, as `Doorkeeper::OAuth::Token
/// .authenticate` finds it for `Rack::Attack`.
#[derive(Debug, Clone, Copy)]
pub struct RequestToken {
    pub id: i64,
    /// `resource_owner_id`: none for an application's own token.
    pub user_id: Option<i64>,
}

/// A response no route answered, which no controller's
/// `set_rate_limit_headers` touched.
#[derive(Debug, Clone, Copy)]
pub struct Unrouted;

/// A throttle's count for this request (`rack.attack.throttle_data`).
#[derive(Debug, Clone, Copy)]
struct ThrottleData {
    limit: i64,
    count: i64,
    period: i64,
}

/// `Rack::Attack::Cache#count`: the request counted in the period's key,
/// which expires when the period does.
async fn count(state: &AppState, throttle: &Throttle, discriminator: &str, epoch: i64) -> i64 {
    let key = state.redis_keys.key(format!(
        "cache:rack::attack:{}:{}:{discriminator}",
        epoch / throttle.period,
        throttle.name
    ));
    let mut redis = state.redis.clone();
    let count: redis::RedisResult<i64> = redis::cmd("INCRBY")
        .arg(&key)
        .arg(1)
        .query_async(&mut redis)
        .await;
    match count {
        Ok(count) => {
            if count == 1 {
                let _: redis::RedisResult<()> = redis::cmd("EXPIRE")
                    .arg(&key)
                    .arg(throttle.period - epoch % throttle.period + 1)
                    .query_async(&mut redis)
                    .await;
            }
            count
        }
        // `Rails.cache` fails safe: an unreachable cache counts the request
        // as the first.
        Err(_) => 1,
    }
}

/// When a period ends: `(now + (period - now.to_i % period)).iso8601(6)`.
fn reset(now: DateTime<Utc>, period: i64) -> String {
    let at = now + chrono::Duration::seconds(period - now.timestamp() % period);
    at.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

fn set_headers(headers: &mut HeaderMap, limit: i64, remaining: i64, reset: &str) {
    for (name, value) in [
        ("x-ratelimit-limit", limit.to_string()),
        ("x-ratelimit-remaining", remaining.to_string()),
        ("x-ratelimit-reset", reset.to_owned()),
    ] {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
}

/// `Rack::Attack.throttled_responder`.
fn throttled(state: &AppState, limit: i64, period: i64, now: DateTime<Utc>) -> Response {
    let message = too_many_requests(state.instance.default_locale());
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        axum::Json(serde_json::json!({ "error": message })),
    )
        .into_response();
    set_headers(response.headers_mut(), limit, 0, &reset(now, period));
    response
}

/// Paths whose form a throttle reads: the email given, the pending sign-in,
/// or `_method`.
fn reads_form(method: &Method, path: &str) -> bool {
    *method == Method::POST
        && (path.starts_with("/auth")
            || path == "/account/login"
            || path == "/account/password"
            || path == "/oauth/authorize")
}

/// The form a request posted, read without taking it from the handler.
async fn take_form(req: Request) -> (Request, Vec<(String, String)>) {
    let form_encoded = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"));
    if !form_encoded {
        return (req, Vec::new());
    }
    let (parts, body) = req.into_parts();
    // Axum's own limit on what a form extractor reads.
    let bytes = axum::body::to_bytes(body, 2 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let form = serde_urlencoded::from_bytes(&bytes).unwrap_or_default();
    (Request::from_parts(parts, Body::from(bytes)), form)
}

fn param<'a>(form: &'a [(String, String)], names: &[&str]) -> Option<&'a str> {
    form.iter()
        .find(|(k, v)| names.contains(&k.as_str()) && !v.trim().is_empty())
        .map(|(_, v)| v.as_str())
}

/// `throttleable_remote_ip`.
fn throttleable(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V6(v6) => {
            let masked = u128::from(v6) & !((1u128 << 64) - 1);
            std::net::Ipv6Addr::from(masked).to_string()
        }
        v4 => v4.to_string(),
    }
}

/// The rest of what the throttles ask, which the session and pending
/// sign-ins hold.
async fn session_facts(
    state: &AppState,
    mut facts: Facts,
    headers: &HeaderMap,
    form: &[(String, String)],
) -> Facts {
    if facts.sign_in() {
        if let Some(token) = param(form, &["attempt"]) {
            facts.attempt_user_id =
                crate::api::account::sign_in::attempt_user_id(state, token).await;
        }
    }
    if facts.auth_setup() || facts.password_change() {
        facts.warden_user_id = crate::api::account::signed_in_user(headers, state).await;
    }
    facts
}

/// What the request itself says.
fn request_facts(req: &Request, form: &[(String, String)]) -> Facts {
    let path = req.uri().path().to_owned();
    // `Rack::MethodOverride`, which runs before `Rack::Attack`.
    let mut method = req.method().clone();
    if method == Method::POST {
        if let Some(over) = param(form, &["_method"]) {
            if let Ok(over) = Method::from_bytes(over.to_ascii_uppercase().as_bytes()) {
                method = over;
            }
        }
    }
    let token = req.extensions().get::<RequestToken>().copied();
    let query: Vec<(String, String)> = req
        .uri()
        .query()
        .and_then(|q| serde_urlencoded::from_str(q).ok())
        .unwrap_or_default();
    let paging_keys = ["page", "min_id", "max_id", "since_id"];
    let paging = param(&query, &paging_keys).is_some() || param(form, &paging_keys).is_some();
    let mut facts = Facts {
        method,
        path,
        user_id: token.and_then(|t| t.user_id),
        token_id: token.map(|t| t.id),
        ip: req
            .extensions()
            .get::<crate::remote_ip::ClientIp>()
            .and_then(|c| c.0)
            .map(throttleable),
        paging,
        email: param(form, &["user[email]", "email"]).map(str::to_owned),
        ..Facts::default()
    };
    facts.credentials = param(form, &["email", "user[email]", "password", "attempt"]).is_some();
    facts
}

/// The throttles, and the families' headers, around every request.
pub async fn layer(req: Request, next: Next) -> Response {
    let Some(state) = req.extensions().get::<AppState>().cloned() else {
        return next.run(req).await;
    };
    // The streaming API is Mastodon's other process, which `Rack::Attack`
    // never sees.
    if !state.config.limits.rate_limits() || req.uri().path().starts_with("/api/v1/streaming") {
        return next.run(req).await;
    }
    let now = Utc::now();
    let epoch = now.timestamp();

    let (req, form) = if reads_form(req.method(), req.uri().path()) {
        take_form(req).await
    } else {
        (req, Vec::new())
    };
    let facts = request_facts(&req, &form);
    let facts = session_facts(&state, facts, &req.headers().clone(), &form).await;
    // `Rack::Attack::Configuration#throttled?`: `any?`, so the throttles
    // after the first one exceeded do not count the request.
    let mut throttle_data: Vec<ThrottleData> = Vec::new();
    for throttle in &THROTTLES {
        let Some(discriminator) = (throttle.discriminator)(&facts) else {
            continue;
        };
        let discriminator = discriminator.trim().to_lowercase();
        let count = count(&state, throttle, &discriminator, epoch).await;
        throttle_data.push(ThrottleData {
            limit: throttle.limit,
            count,
            period: throttle.period,
        });
        if count > throttle.limit {
            tracing::info!(
                throttle = throttle.name,
                ip = facts.ip.as_deref().unwrap_or(""),
                "Rate limit hit (throttle): {} {}",
                req.method(),
                req.uri()
            );
            return throttled(&state, throttle.limit, throttle.period, now);
        }
    }

    let family = req
        .extensions()
        .get::<AuthenticatedUser>()
        .map(|auth| auth.account_id)
        .and_then(|account_id| {
            family_route(req.method(), req.uri().path()).map(|(f, counts)| (account_id, f, counts))
        });
    let mut response = match family {
        Some((account_id, family, true)) => {
            SCOPE
                .scope(Scope { account_id, family }, next.run(req))
                .await
        }
        _ => next.run(req).await,
    };

    // `Api::RateLimitHeaders#set_rate_limit_headers`, which every API
    // controller runs: the throttle closest to its limit.
    let api = facts.api() && response.extensions().get::<Unrouted>().is_none();
    if api {
        if let Some(data) = throttle_data
            .iter()
            .min_by_key(|data| data.limit - data.count)
        {
            set_headers(
                response.headers_mut(),
                data.limit,
                data.limit - data.count,
                &reset(now, data.period),
            );
        }
    }
    // `override_rate_limit_headers`, unless what the throttles said leaves
    // fewer.
    if let Some((account_id, family, _)) = family.filter(|_| api) {
        let (remaining, reset) = family_headers(&state, account_id, family, Utc::now()).await;
        let existing = response
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<i64>().ok());
        if existing.is_none_or(|existing| remaining <= existing) {
            set_headers(response.headers_mut(), family.limit, remaining, &reset);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_is_in_the_default_locale() {
        assert_eq!(too_many_requests("en"), "Too many requests");
        assert_eq!(too_many_requests("ko"), "요청 횟수 제한에 도달했습니다");
        // A locale eunha has no text in reads in English.
        assert_eq!(too_many_requests("de"), "Too many requests");
    }

    #[test]
    fn a_period_resets_at_its_boundary() {
        let now = DateTime::parse_from_rfc3339("2026-10-09T12:34:56.789012Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(reset(now, 300), "2026-10-09T12:35:00.789012Z");
        assert_eq!(reset(now, 3 * 3600), "2026-10-09T15:00:00.789012Z");
    }

    #[test]
    fn ipv6_clients_are_throttled_by_their_64() {
        assert_eq!(
            throttleable("2001:db8:1:2:3:4:5:6".parse().unwrap()),
            "2001:db8:1:2::"
        );
        assert_eq!(throttleable("192.0.2.1".parse().unwrap()), "192.0.2.1");
    }

    #[test]
    fn paths_match_with_a_format() {
        let facts = Facts {
            method: Method::POST,
            path: "/auth.json".into(),
            ..Facts::default()
        };
        assert!(facts.path_matches("/auth"));
        assert!(!facts.path_matches("/auth/password"));
        let facts = Facts {
            path: "/authx".into(),
            ..Facts::default()
        };
        assert!(!facts.path_matches("/auth"));
    }

    #[test]
    fn the_families_routes_are_those_mastodon_overrides() {
        assert_eq!(
            family_route(&Method::POST, "/api/v1/statuses"),
            Some((STATUSES, true))
        );
        assert_eq!(
            family_route(&Method::PUT, "/api/v1/statuses/1"),
            Some((STATUSES, true))
        );
        assert_eq!(
            family_route(&Method::POST, "/api/v1/statuses/1/reblog"),
            Some((STATUSES, false))
        );
        assert_eq!(
            family_route(&Method::POST, "/api/v1/tags/rust/follow"),
            Some((FOLLOWS, true))
        );
        assert_eq!(
            family_route(&Method::POST, "/api/v1/reports"),
            Some((REPORTS, false))
        );
        assert_eq!(family_route(&Method::GET, "/api/v1/statuses"), None);
    }
}
