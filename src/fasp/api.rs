//! The API providers call: Mastodon's `Api::Fasp::*` controllers under
//! `/api/fasp/`, which answer 404 unless the `fasp` feature is on.
//!
//! Registration is open; everything else needs a request signed by a
//! confirmed provider (`Api::Fasp::BaseController#require_authentication`):
//! a `Content-Digest` matching the body, and an RFC 9421 signature whose
//! `keyid` is the provider's id here, made in the last five minutes with the
//! key it registered. A refusal is a bare 401. Every answer an action gives
//! is signed over `@status` and `content-digest` with this server's key for
//! the provider (`#sign_response`); a refusal, or an error raised before the
//! action finishes, is not, as Rails skips `after_action` for those.

use axum::{
    body::Bytes,
    extract::{Extension, Path},
    http::{HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{delete, post},
    Router,
};
use serde_json::{json, Value};

use super::{signature, Provider};
use crate::state::AppState;

pub fn router() -> Router {
    Router::new()
        .route("/api/fasp/registration", post(create_registration))
        .route(
            "/api/fasp/debug/v0/callback/responses",
            post(create_debug_callback),
        )
        .route(
            "/api/fasp/data_sharing/v0/backfill_requests",
            post(create_backfill_request),
        )
        .route(
            "/api/fasp/data_sharing/v0/backfill_requests/{id}/continuation",
            post(create_continuation),
        )
        .route(
            "/api/fasp/data_sharing/v0/event_subscriptions",
            post(create_event_subscription),
        )
        .route(
            "/api/fasp/data_sharing/v0/event_subscriptions/{id}",
            delete(destroy_event_subscription),
        )
}

/// What every handler starts from.
struct Call {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({"error": "Record not found"})),
    )
        .into_response()
}

fn unprocessable(message: String) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        axum::Json(json!({ "error": format!("Validation failed: {message}") })),
    )
        .into_response()
}

fn internal(error: impl std::fmt::Display) -> Response {
    tracing::error!(%error, "FASP API request failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(json!({"error": "Internal server error"})),
    )
        .into_response()
}

/// `@target-uri` as Rack rebuilds the URL a request was made to: the host it
/// was sent to and the path and query it asked for, over HTTPS, which is how
/// an instance is served.
fn target_uri(headers: &HeaderMap, uri: &Uri, state: &AppState) -> String {
    let host = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or(&state.instance.domain);
    let path = uri.path_and_query().map_or("/", |p| p.as_str());
    format!("https://{host}{path}")
}

/// `#require_authentication`: the confirmed provider that signed the call.
async fn authenticate(state: &AppState, call: &Call) -> Result<Provider, StatusCode> {
    // `#validate_content_digest!`.
    let digest = call
        .headers
        .get("content-digest")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            tracing::debug!("FASP Authentication error: content-digest missing");
            StatusCode::UNAUTHORIZED
        })?;
    let expected = signature::content_digest(&call.body);
    if signature::sha256_member(digest) != signature::sha256_member(&expected) {
        tracing::debug!("FASP Authentication error: content-digest does not match");
        return Err(StatusCode::UNAUTHORIZED);
    }
    // `#validate_signature!`.
    let Some(key_id) = signature::key_id(&call.headers) else {
        tracing::debug!("FASP Authentication error: signature-input is missing");
        return Err(StatusCode::UNAUTHORIZED);
    };
    let Ok(id) = key_id.parse::<i64>() else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let provider = match Provider::find(state, id).await {
        Ok(Some(provider)) if provider.confirmed => provider,
        Ok(_) => return Err(StatusCode::UNAUTHORIZED),
        Err(error) => {
            tracing::error!(%error, "FASP API request failed");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    let key = provider.provider_public_key().map_err(|error| {
        tracing::debug!(%error, "FASP Authentication error");
        StatusCode::UNAUTHORIZED
    })?;
    signature::verify_request(
        call.method.as_str(),
        &target_uri(&call.headers, &call.uri, state),
        &call.headers,
        &call.body,
        &key,
        chrono::Utc::now().timestamp(),
    )
    .map_err(|error| {
        tracing::debug!(%error, "FASP Authentication error");
        StatusCode::UNAUTHORIZED
    })?;
    Ok(provider)
}

/// `#sign_response`: the answer, with its digest and this server's signature.
fn signed(provider: &Provider, status: StatusCode, body: Option<Value>) -> Response {
    let bytes = body.map(|b| b.to_string()).unwrap_or_default();
    let digest = signature::content_digest(bytes.as_bytes());
    let seed = match provider.server_key() {
        Ok(seed) => seed,
        Err(error) => return internal(error),
    };
    let (input, sig) = signature::sign_response(
        status.as_u16(),
        &digest,
        &seed,
        chrono::Utc::now().timestamp(),
    );
    let mut response = (status, bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json; charset=utf-8"),
    );
    for (name, value) in [
        ("content-digest", digest),
        ("signature-input", input),
        ("signature", sig),
    ] {
        if let Ok(value) = value.parse() {
            headers.insert(name, value);
        }
    }
    response
}

/// The JSON a provider sent, or an empty object for a body that is not.
fn params(body: &Bytes) -> Value {
    serde_json::from_slice::<Value>(body)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

fn string_param(params: &Value, name: &str) -> Option<String> {
    match params.get(name)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// An integer column's cast: a number, or a string that is one.
fn integer_param(value: Option<&Value>) -> Result<Option<i64>, ()> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_i64().map(Some).ok_or(()),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| ()),
        Some(_) => Err(()),
    }
}

/// `URLValidator`: an absolute http or https URL with a host.
fn compliant_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty())
    })
}

// ── POST /api/fasp/registration ─────────────────────────────────────────

/// `Api::Fasp::RegistrationsController#create`: record the provider,
/// unconfirmed, with a key pair of this server's own, and answer with its id
/// here, this server's public key, and where an administrator finishes the
/// registration.
async fn create_registration(state: AppState, body: Bytes) -> Response {
    if !super::enabled(&state) {
        return not_found();
    }
    let params = params(&body);
    let name = string_param(&params, "name").unwrap_or_default();
    let base_url = string_param(&params, "baseUrl").unwrap_or_default();
    let remote_identifier = string_param(&params, "serverId").unwrap_or_default();
    let public_key = string_param(&params, "publicKey").unwrap_or_default();

    let mut errors = vec![];
    if name.trim().is_empty() {
        errors.push("Name can't be blank".to_owned());
    }
    if base_url.trim().is_empty() {
        errors.push("Base url can't be blank".to_owned());
    } else if !compliant_url(&base_url) {
        errors.push("Base url is invalid".to_owned());
    }
    let provider_public_key_pem = if public_key.trim().is_empty() {
        errors.push("Provider public key pem can't be blank".to_owned());
        None
    } else {
        match super::keys::public_key_from_base64(public_key.trim()) {
            Ok(raw) => Some(super::keys::public_key_to_pem(&raw)),
            Err(_) => {
                errors.push("Provider public key pem is invalid".to_owned());
                None
            }
        }
    };
    if remote_identifier.trim().is_empty() {
        errors.push("Remote identifier can't be blank".to_owned());
    }
    if !errors.is_empty() {
        return unprocessable(errors.join(", "));
    }
    let server_private_key_pem = match super::keys::generate_private_key_pem() {
        Ok(pem) => pem,
        Err(error) => return internal(error),
    };
    let inserted = sqlx::query_scalar!(
        r#"INSERT INTO fasp_providers
             (name, base_url, remote_identifier, provider_public_key_pem,
              server_private_key_pem, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, now(), now())
           ON CONFLICT (base_url) DO NOTHING
           RETURNING id"#,
        name,
        base_url,
        remote_identifier,
        provider_public_key_pem,
        server_private_key_pem,
    )
    .fetch_optional(&state.db)
    .await;
    let id = match inserted {
        Ok(Some(id)) => id,
        Ok(None) => return unprocessable("Base url has already been taken".into()),
        Err(error) => return internal(error),
    };
    let provider = match Provider::find(&state, id).await {
        Ok(Some(provider)) => provider,
        Ok(None) => return not_found(),
        Err(error) => return internal(error),
    };
    let public_key = match provider.server_public_key_base64() {
        Ok(key) => key,
        Err(error) => return internal(error),
    };
    signed(
        &provider,
        StatusCode::OK,
        Some(json!({
            "faspId": provider.id.to_string(),
            "publicKey": public_key,
            "registrationCompletionUri": format!(
                "https://{}/admin/fasp/providers/{}/registration/new",
                state.instance.domain, provider.id
            ),
        })),
    )
}

// ── POST /api/fasp/debug/v0/callback/responses ──────────────────────────

/// `Api::Fasp::Debug::V0::Callback::ResponsesController#create`: keep what
/// the provider sent back after a debug call, for the administrator to see.
async fn create_debug_callback(
    state: AppState,
    client_ip: Option<Extension<crate::remote_ip::ClientIp>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !super::enabled(&state) {
        return not_found();
    }
    let call = Call {
        method,
        uri,
        headers,
        body,
    };
    let provider = match authenticate(&state, &call).await {
        Ok(provider) => provider,
        Err(status) => return status.into_response(),
    };
    let ip = client_ip
        .and_then(|Extension(ip)| ip.0)
        .map(|ip| ip.to_string())
        .unwrap_or_default();
    let request_body = String::from_utf8_lossy(&call.body).into_owned();
    // `Fasp::DebugCallback.create`, whose failure goes unnoticed.
    if let Err(error) = sqlx::query!(
        r#"INSERT INTO fasp_debug_callbacks (fasp_provider_id, ip, request_body, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())"#,
        provider.id,
        ip,
        request_body,
    )
    .execute(&state.db)
    .await
    {
        tracing::warn!(%error, "could not record a FASP debug callback");
    }
    signed(&provider, StatusCode::CREATED, None)
}

// ── POST /api/fasp/data_sharing/v0/backfill_requests ────────────────────

/// `Api::Fasp::DataSharing::V0::BackfillRequestsController#create`: record
/// the request and start announcing what it asks for.
async fn create_backfill_request(
    state: AppState,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !super::enabled(&state) {
        return not_found();
    }
    let call = Call {
        method,
        uri,
        headers,
        body,
    };
    let provider = match authenticate(&state, &call).await {
        Ok(provider) => provider,
        Err(status) => return status.into_response(),
    };
    let params = params(&call.body);
    let category = string_param(&params, "category").unwrap_or_default();
    // `max_count` defaults to 100 in the table, and must be an integer.
    let max_count = match integer_param(params.get("maxCount")) {
        Ok(Some(n)) if i32::try_from(n).is_ok() => n as i32,
        Ok(None) if params.get("maxCount").is_none() => 100,
        _ => return signed(&provider, StatusCode::UNPROCESSABLE_ENTITY, None),
    };
    if !super::DATA_CATEGORIES.contains(&category.as_str()) {
        return signed(&provider, StatusCode::UNPROCESSABLE_ENTITY, None);
    }
    let id = match sqlx::query_scalar!(
        r#"INSERT INTO fasp_backfill_requests
             (category, max_count, fasp_provider_id, created_at, updated_at)
           VALUES ($1, $2, $3, now(), now())
           RETURNING id"#,
        category,
        max_count,
        provider.id,
    )
    .fetch_one(&state.db)
    .await
    {
        Ok(id) => id,
        Err(error) => return internal(error),
    };
    // `after_commit :queue_fulfillment_job, on: :create`.
    super::workers::backfill_async(&state, id).await;
    signed(
        &provider,
        StatusCode::CREATED,
        Some(json!({ "backfillRequest": { "id": id } })),
    )
}

// ── POST /api/fasp/data_sharing/v0/backfill_requests/:id/continuation ───

/// `Api::Fasp::DataSharing::V0::ContinuationsController#create`: announce
/// the next batch of one of the provider's backfill requests.
async fn create_continuation(
    state: AppState,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !super::enabled(&state) {
        return not_found();
    }
    let call = Call {
        method,
        uri,
        headers,
        body,
    };
    let provider = match authenticate(&state, &call).await {
        Ok(provider) => provider,
        Err(status) => return status.into_response(),
    };
    let Ok(id) = id.parse::<i64>() else {
        return not_found();
    };
    let found = sqlx::query_scalar!(
        "SELECT id FROM fasp_backfill_requests WHERE id = $1 AND fasp_provider_id = $2",
        id,
        provider.id
    )
    .fetch_optional(&state.db)
    .await;
    match found {
        Ok(Some(id)) => {
            super::workers::backfill_async(&state, id).await;
            signed(&provider, StatusCode::NO_CONTENT, None)
        }
        Ok(None) => not_found(),
        Err(error) => internal(error),
    }
}

// ── POST /api/fasp/data_sharing/v0/event_subscriptions ──────────────────

/// `Api::Fasp::DataSharing::V0::EventSubscriptionsController#create`.
async fn create_event_subscription(
    state: AppState,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !super::enabled(&state) {
        return not_found();
    }
    let call = Call {
        method,
        uri,
        headers,
        body,
    };
    let provider = match authenticate(&state, &call).await {
        Ok(provider) => provider,
        Err(status) => return status.into_response(),
    };
    let params = params(&call.body);
    let category = string_param(&params, "category").unwrap_or_default();
    let subscription_type = string_param(&params, "subscriptionType").unwrap_or_default();
    let mut errors = vec![];
    if category.trim().is_empty() {
        errors.push("Category can't be blank");
    } else if !super::DATA_CATEGORIES.contains(&category.as_str()) {
        errors.push("Category is not included in the list");
    }
    if subscription_type.trim().is_empty() {
        errors.push("Subscription type can't be blank");
    } else if !super::SUBSCRIPTION_TYPES.contains(&subscription_type.as_str()) {
        errors.push("Subscription type is not included in the list");
    }
    let max_batch_size = match integer_param(params.get("maxBatchSize")) {
        Ok(Some(n)) => i32::try_from(n).ok(),
        _ => None,
    };
    if max_batch_size.is_none() {
        errors.push("Max batch size can't be blank");
    }
    if !errors.is_empty() {
        return unprocessable(errors.join(", "));
    }
    // `#threshold=`, only when a threshold was given: each part defaults.
    let threshold = params.get("threshold").filter(|t| t.is_object());
    let part = |name: &str, default: i32| -> Option<i32> {
        let threshold = threshold?;
        Some(
            integer_param(threshold.get(name))
                .ok()
                .flatten()
                .and_then(|n| i32::try_from(n).ok())
                .unwrap_or(default),
        )
    };
    let id = match sqlx::query_scalar!(
        r#"INSERT INTO fasp_subscriptions
             (category, subscription_type, max_batch_size, threshold_timeframe,
              threshold_shares, threshold_likes, threshold_replies, fasp_provider_id,
              created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now(), now())
           RETURNING id"#,
        category,
        subscription_type,
        max_batch_size,
        part("timeframe", 15),
        part("shares", 3),
        part("likes", 3),
        part("replies", 3),
        provider.id,
    )
    .fetch_one(&state.db)
    .await
    {
        Ok(id) => id,
        Err(error) => return internal(error),
    };
    signed(
        &provider,
        StatusCode::CREATED,
        Some(json!({ "subscription": { "id": id } })),
    )
}

// ── DELETE /api/fasp/data_sharing/v0/event_subscriptions/:id ────────────

/// `Api::Fasp::DataSharing::V0::EventSubscriptionsController#destroy`.
async fn destroy_event_subscription(
    state: AppState,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !super::enabled(&state) {
        return not_found();
    }
    let call = Call {
        method,
        uri,
        headers,
        body,
    };
    let provider = match authenticate(&state, &call).await {
        Ok(provider) => provider,
        Err(status) => return status.into_response(),
    };
    let Ok(id) = id.parse::<i64>() else {
        return not_found();
    };
    let deleted = sqlx::query!(
        "DELETE FROM fasp_subscriptions WHERE id = $1 AND fasp_provider_id = $2",
        id,
        provider.id
    )
    .execute(&state.db)
    .await;
    match deleted {
        Ok(done) if done.rows_affected() > 0 => signed(&provider, StatusCode::NO_CONTENT, None),
        Ok(_) => not_found(),
        Err(error) => internal(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_params_cast_like_an_integer_column() {
        assert_eq!(integer_param(Some(&json!(5))), Ok(Some(5)));
        assert_eq!(integer_param(Some(&json!("7"))), Ok(Some(7)));
        assert_eq!(integer_param(None), Ok(None));
        assert!(integer_param(Some(&json!("many"))).is_err());
    }
}
