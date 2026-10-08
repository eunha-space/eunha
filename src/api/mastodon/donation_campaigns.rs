//! `GET /api/v1/donation_campaigns`: Mastodon's `DonationCampaignsController`,
//! which asks the configured campaign API (`[instance.donation_campaigns]`,
//! Mastodon's `DONATION_CAMPAIGNS_URL`) which campaign a user is to be shown,
//! and keeps the answer an hour in the cache Mastodon keeps it in.

use axum::{
    extract::{Extension, Query},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

/// `STOPLIGHT_COOL_OFF_TIME` and `STOPLIGHT_FAILURE_THRESHOLD`.
const BREAKER: ojak::deliverer::CircuitBreaker = ojak::deliverer::CircuitBreaker {
    threshold: 10,
    cool_off: std::time::Duration::from_secs(60),
    scope: ojak::deliverer::BreakerScope::Inbox,
};

/// The light's name, `Stoplight('donation_campaigns', …)`.
const LIGHT: &str = "donation_campaigns";

/// `expires_in: 1.hour`.
const CACHE_TTL: u64 = 60 * 60;

/// `Request#body_with_limit`'s default.
const BODY_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Default, Deserialize)]
pub struct Params {
    /// `Localized`'s `params[:lang]`.
    #[serde(default)]
    lang: Option<String>,
}

/// `DonationCampaignsController#index`: `204` when no campaign API is
/// configured or it has no campaign to show, else the campaign — from the
/// cache when this seed and locale asked within the hour.
pub async fn index(
    state: AppState,
    auth: Option<Extension<AuthenticatedUser>>,
    headers: HeaderMap,
    Query(params): Query<Params>,
) -> AppResult<Response> {
    // `before_action :require_user!`.
    crate::middleware::require_user(auth.as_ref().map(|Extension(a)| a))?;
    let Some(Extension(auth)) = auth else {
        return Err(AppError::Unauthorized);
    };
    let config = &state.instance.donation_campaigns;
    let Some(api_url) = config.api_url.as_deref().filter(|u| !u.trim().is_empty()) else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };

    let seed = seed(auth.account_id);
    let locale =
        super::translations::requested_locale(&state, &auth, &headers, params.lang.as_deref())
            .await?;
    let request_key = format!("cache:donation_campaign_request:{seed}:{locale}");

    if let Some(campaign) = from_cache(&state, &request_key).await {
        return Ok(json(campaign));
    }

    let Some(campaign) = fetch_campaign(
        &state,
        api_url,
        seed,
        &locale,
        config.environment.as_deref(),
    )
    .await?
    else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    save_to_cache(&state, &request_key, &campaign).await;
    Ok(json(campaign.to_string()))
}

fn json(body: String) -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "application/json; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// `from_cache`: the campaign key this seed and locale were last given, and
/// the campaign under it, both raw in Mastodon's `Rails.cache`.
async fn from_cache(state: &AppState, request_key: &str) -> Option<String> {
    let mut redis = state.redis.clone();
    let key: Option<String> = redis::cmd("GET")
        .arg(state.redis_keys.key(request_key))
        .query_async(&mut redis)
        .await
        .ok()
        .flatten()
        .filter(|k: &String| !k.trim().is_empty());
    let campaign: Option<String> = redis::cmd("GET")
        .arg(
            state
                .redis_keys
                .key(format!("cache:donation_campaign:{}", key?)),
        )
        .query_async(&mut redis)
        .await
        .ok()
        .flatten()
        .filter(|c: &String| !c.trim().is_empty());
    // `JSON.parse(campaign)`, rendered again.
    let parsed: serde_json::Value = serde_json::from_str(&campaign?).ok()?;
    Some(parsed.to_string())
}

/// `save_to_cache!`: the request's campaign key and the campaign, for an
/// hour.
async fn save_to_cache(state: &AppState, request_key: &str, campaign: &serde_json::Value) {
    // `return if campaign.blank?`.
    if !crate::federation::json_ld::is_present(campaign) {
        return;
    }
    // `"#{campaign['id']}:#{campaign['locale']}"`, Ruby's `to_s` of each.
    let to_s = |value: Option<&serde_json::Value>| match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    let campaign_key = format!(
        "{}:{}",
        to_s(campaign.get("id")),
        to_s(campaign.get("locale"))
    );
    let mut redis = state.redis.clone();
    let result: redis::RedisResult<()> = redis::pipe()
        .cmd("SET")
        .arg(state.redis_keys.key(request_key))
        .arg(&campaign_key)
        .arg("EX")
        .arg(CACHE_TTL)
        .ignore()
        .cmd("SET")
        .arg(
            state
                .redis_keys
                .key(format!("cache:donation_campaign:{campaign_key}")),
        )
        .arg(campaign.to_string())
        .arg("EX")
        .arg(CACHE_TTL)
        .ignore()
        .query_async(&mut redis)
        .await;
    if let Err(error) = result {
        tracing::warn!(%error, "could not cache the donation campaign");
    }
}

/// `fetch_campaign`: the campaign API asked with `platform=web`, the seed,
/// the locale and the environment, behind the `donation_campaigns`
/// stoplight. A `200` is the campaign; any other answer is none. A request
/// that is not answered, or a body that is not JSON, is none too, and counts
/// against the light; while it is red the request fails with `503`.
async fn fetch_campaign(
    state: &AppState,
    api_url: &str,
    seed: u32,
    locale: &str,
    environment: Option<&str>,
) -> AppResult<Option<serde_json::Value>> {
    let breakers = crate::federation::delivery::RedisBreakers::new(
        state.redis_coordination.clone(),
        state.redis_keys.clone(),
    );
    let probe = match breakers.admit(&BREAKER, LIGHT).await {
        ojak::deliverer::Admission::Pass => None,
        ojak::deliverer::Admission::Probe(probe) => Some(probe),
        // `Stoplight::Error::RedLight`.
        ojak::deliverer::Admission::Held(_) => {
            return Err(AppError::ServiceUnavailable(
                "There was a temporary problem serving your request, please try again".into(),
            ))
        }
    };

    let Ok(mut url) = url::Url::parse(api_url) else {
        tracing::warn!(api_url, "the donation campaigns URL is not a URL");
        return Ok(None);
    };
    // `url.query_values = {…}.compact`: replacing any query the setting had,
    // without an unset environment, in the order Addressable sorts them.
    let seed = seed.to_string();
    let pairs = [
        ("environment", environment),
        ("locale", Some(locale)),
        ("platform", Some("web")),
        ("seed", Some(seed.as_str())),
    ];
    url.query_pairs_mut()
        .clear()
        .extend_pairs(pairs.iter().filter_map(|(k, v)| Some((*k, (*v)?))));

    let answered = match state.fetch.request(reqwest::Method::GET, &url) {
        Ok(request) => request.send().await.map_err(|e| e.to_string()),
        Err(error) => Err(error.to_string()),
    };
    let response = match answered {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, "could not reach the donation campaigns API");
            let _ = breakers.record(&BREAKER, LIGHT, true, probe).await;
            return Ok(None);
        }
    };
    if response.status() != reqwest::StatusCode::OK {
        // The block returns normally: a success.
        let _ = breakers.record(&BREAKER, LIGHT, false, probe).await;
        return Ok(None);
    }
    let body = match read_body(response).await {
        Some(body) => body,
        None => {
            let _ = breakers.record(&BREAKER, LIGHT, true, probe).await;
            return Ok(None);
        }
    };
    match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(campaign) => {
            // `return JSON.parse(…)` leaves the block early, and Stoplight
            // records nothing; a probe is let go, its light still yellow.
            drop(probe);
            Ok(Some(campaign))
        }
        Err(error) => {
            tracing::warn!(%error, "the donation campaigns API answered with no JSON");
            let _ = breakers.record(&BREAKER, LIGHT, true, probe).await;
            Ok(None)
        }
    }
}

/// `Request#body_with_limit`: the body, or none past a megabyte or when it
/// could not be read.
async fn read_body(mut response: reqwest::Response) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > BODY_LIMIT {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

/// `Random.new(current_account.id).rand(100)`: Ruby's Mersenne Twister,
/// seeded as Ruby seeds it from an integer, drawing below 100 as Ruby's
/// `limited_rand` does.
pub fn seed(account_id: i64) -> u32 {
    let magnitude = account_id.unsigned_abs();
    let mut mt = if magnitude >> 32 == 0 {
        Mt::new(magnitude as u32)
    } else {
        Mt::by_array(&[magnitude as u32, (magnitude >> 32) as u32])
    };
    loop {
        // `make_mask(99)` is 127; a draw past 99 is drawn again.
        let value = mt.next_u32() & 127;
        if value <= 99 {
            return value;
        }
    }
}

/// MT19937, as Ruby's `random.c` has it.
struct Mt {
    state: [u32; 624],
    index: usize,
}

impl Mt {
    /// `init_genrand`.
    fn new(seed: u32) -> Self {
        let mut state = [0u32; 624];
        state[0] = seed;
        for i in 1..624 {
            state[i] = 1_812_433_253u32
                .wrapping_mul(state[i - 1] ^ (state[i - 1] >> 30))
                .wrapping_add(i as u32);
        }
        Self { state, index: 624 }
    }

    /// `init_by_array`.
    fn by_array(key: &[u32]) -> Self {
        let mut mt = Self::new(19_650_218);
        let s = &mut mt.state;
        let (mut i, mut j) = (1usize, 0usize);
        for _ in 0..624.max(key.len()) {
            s[i] = (s[i] ^ (s[i - 1] ^ (s[i - 1] >> 30)).wrapping_mul(1_664_525))
                .wrapping_add(key[j])
                .wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= 624 {
                s[0] = s[623];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
        }
        for _ in 0..623 {
            s[i] = (s[i] ^ (s[i - 1] ^ (s[i - 1] >> 30)).wrapping_mul(1_566_083_941))
                .wrapping_sub(i as u32);
            i += 1;
            if i >= 624 {
                s[0] = s[623];
                i = 1;
            }
        }
        s[0] = 0x8000_0000;
        mt
    }

    /// `genrand_int32`.
    fn next_u32(&mut self) -> u32 {
        if self.index >= 624 {
            for k in 0..624 {
                let y = (self.state[k] & 0x8000_0000) | (self.state[(k + 1) % 624] & 0x7fff_ffff);
                let mag = if y & 1 == 1 { 0x9908_b0df } else { 0 };
                self.state[k] = self.state[(k + 397) % 624] ^ (y >> 1) ^ mag;
            }
            self.index = 0;
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^ (y >> 18)
    }
}

#[cfg(test)]
mod tests {
    use super::seed;

    /// What `ruby -e 'p Random.new(id).rand(100)'` prints for each.
    #[test]
    fn seeds_as_ruby_does() {
        assert_eq!(seed(1), 37);
        assert_eq!(seed(42), 51);
        assert_eq!(seed(4_294_967_295), 35);
        assert_eq!(seed(109_876_543_210_987_654), 25);
    }
}
