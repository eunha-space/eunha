use axum::{extract::Extension, Json};
use serde::Serialize;
use serde_json::{Map, Value};

use super::extractors::NestedParams;
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

// ── Subscription response type ─────────────────────────────────────────────

/// `REST::WebPushSubscriptionSerializer`.
#[derive(Debug, Serialize)]
pub struct PushSubscription {
    /// A number: unlike most entities, the serializer does not override
    /// `id` with `object.id.to_s`.
    pub id: i64,
    pub endpoint: String,
    pub standard: bool,
    /// The alerts as stored, each cast as `ActiveModel::Type::Boolean` casts
    /// it, so a value given as `"1"` reads `true` and one given as `""` reads
    /// `null`.
    pub alerts: Map<String, Value>,
    pub server_key: String,
    /// The stored policy, or `all`.
    pub policy: Value,
}

impl PushSubscription {
    fn new(
        state: &AppState,
        id: i64,
        endpoint: String,
        standard: bool,
        data: Option<&Value>,
    ) -> Self {
        let alerts = data
            .and_then(|d| d.get("alerts"))
            .and_then(Value::as_object)
            .map(|alerts| {
                alerts
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            crate::push::cast_boolean(v).map_or(Value::Null, Value::Bool),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let policy = match data.and_then(|d| d.get("policy")) {
            None | Some(Value::Null) => Value::String("all".into()),
            Some(policy) => policy.clone(),
        };
        PushSubscription {
            id,
            endpoint,
            standard,
            alerts,
            server_key: state.instance.vapid_public_key.clone(),
            policy,
        }
    }
}

/// `ActionController::ParameterMissing`, as Rails 8 words it.
fn parameter_missing(param: &str) -> AppError {
    AppError::BadRequest(format!(
        "param is missing or the value is empty or invalid: {param}"
    ))
}

/// Ruby's `blank?` for a parameter's value.
fn blank(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// A value `permit` lets through for a scalar key: anything but an array
/// or a hash.
fn permitted_scalar(value: &Value) -> bool {
    !matches!(value, Value::Array(_) | Value::Object(_))
}

/// `data_params`: `{}` when `data` is blank, and otherwise
/// `params.expect(data: [:policy, alerts: Notification::TYPES])` — the
/// policy, and the alerts named in `Notification::TYPES`, exactly as given
/// and in that order; anything else is dropped. Nothing permitted is a
/// missing parameter.
pub(crate) fn data_params(data: Option<&Value>) -> AppResult<Value> {
    let Some(data) = data.filter(|d| !blank(d)) else {
        return Ok(Value::Object(Map::new()));
    };
    let Value::Object(data) = data else {
        return Err(parameter_missing("data"));
    };
    let mut permitted = Map::new();
    if let Some(policy) = data.get("policy").filter(|p| permitted_scalar(p)) {
        permitted.insert("policy".into(), policy.clone());
    }
    if let Some(Value::Object(alerts)) = data.get("alerts") {
        let alerts: Map<String, Value> = crate::push::NOTIFICATION_TYPES
            .iter()
            .filter_map(|t| {
                alerts
                    .get(*t)
                    .filter(|v| permitted_scalar(v))
                    .map(|v| ((*t).to_owned(), v.clone()))
            })
            .collect();
        permitted.insert("alerts".into(), Value::Object(alerts));
    }
    if permitted.is_empty() {
        return Err(parameter_missing("data"));
    }
    Ok(Value::Object(permitted))
}

/// `subscription_params`: `params.expect(subscription: [:endpoint,
/// :standard, keys: [:auth, :p256dh]])`.
struct SubscriptionParams {
    endpoint: Option<String>,
    standard: bool,
    p256dh: Option<String>,
    auth: Option<String>,
}

fn scalar_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
        other => Some(other.to_string()),
    }
}

fn subscription_params(params: &Value) -> AppResult<SubscriptionParams> {
    let Some(Value::Object(subscription)) = params.get("subscription") else {
        return Err(parameter_missing("subscription"));
    };
    let keys = subscription.get("keys").and_then(Value::as_object);
    let endpoint = subscription.get("endpoint").filter(|v| permitted_scalar(v));
    let standard = subscription.get("standard").filter(|v| permitted_scalar(v));
    if endpoint.is_none() && standard.is_none() && keys.is_none() {
        return Err(parameter_missing("subscription"));
    }
    Ok(SubscriptionParams {
        endpoint: scalar_text(endpoint),
        // `subscription_params[:standard] || false`, cast for the column.
        standard: standard
            .and_then(crate::push::cast_boolean)
            .unwrap_or(false),
        p256dh: scalar_text(keys.and_then(|k| k.get("p256dh"))),
        auth: scalar_text(keys.and_then(|k| k.get("auth"))),
    })
}

// ── POST /api/v1/push/subscription ────────────────────────────────────────

/// `Api::V1::Push::SubscriptionsController#create`: the token's
/// subscriptions are destroyed and a new one made, under
/// `with_redis_lock("push_subscription:#{current_user.id}")`.
pub async fn create_subscription(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    NestedParams(params): NestedParams,
) -> AppResult<Json<PushSubscription>> {
    auth.require_scope("push")?;
    let user_id = auth.user_id.ok_or(AppError::Unauthorized)?;

    let subscription = subscription_params(&params)?;
    let data = data_params(params.get("data"))?;
    let endpoint = subscription.endpoint.unwrap_or_default();
    let p256dh = subscription.p256dh.unwrap_or_default();
    let key_auth = subscription.auth.unwrap_or_default();

    let lock = crate::redis_lock::try_acquire_lockable(
        &state,
        &format!("push_subscription:{user_id}"),
        crate::redis_lock::DEFAULT_TTL_MS,
    )
    .await
    .ok_or_else(|| {
        AppError::ServiceUnavailable(
            "There was a temporary problem serving your request, please try again".into(),
        )
    })?;
    let created: AppResult<_> = async {
        // `destroy_web_push_subscriptions!`, then `create!`.
        sqlx::query!(
            "DELETE FROM web_push_subscriptions WHERE access_token_id = $1",
            auth.token_id,
        )
        .execute(&state.db)
        .await?;
        // `create!`'s validations; the old subscriptions stay destroyed.
        let errors = crate::push::subscription_errors(&endpoint, &p256dh, &key_auth);
        if !errors.is_empty() {
            return Err(AppError::Unprocessable(format!(
                "Validation failed: {}",
                errors.join(", ")
            )));
        }
        let row = sqlx::query!(
            r#"INSERT INTO web_push_subscriptions
                 (access_token_id, endpoint, key_p256dh, key_auth, data, standard, user_id, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, now(), now())
               RETURNING id, endpoint, standard, data as "data: serde_json::Value""#,
            auth.token_id,
            endpoint,
            p256dh,
            key_auth,
            data,
            subscription.standard,
            user_id,
        )
        .fetch_one(&state.db)
        .await?;
        Ok(row)
    }
    .await;
    // Released as the block `with_redis_lock` runs ends, raise or not.
    lock.release().await;
    let row = created?;

    Ok(Json(PushSubscription::new(
        &state,
        row.id,
        row.endpoint,
        row.standard,
        row.data.as_ref(),
    )))
}

// ── GET /api/v1/push/subscription ─────────────────────────────────────────

pub async fn get_subscription(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<PushSubscription>> {
    auth.require_scope("push")?;

    // `doorkeeper_token.web_push_subscriptions.first`.
    let row = sqlx::query!(
        r#"SELECT id, endpoint, standard, data as "data: serde_json::Value"
           FROM web_push_subscriptions
           WHERE access_token_id = $1
           ORDER BY id LIMIT 1"#,
        auth.token_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    Ok(Json(PushSubscription::new(
        &state,
        row.id,
        row.endpoint,
        row.standard,
        row.data.as_ref(),
    )))
}

// ── PUT /api/v1/push/subscription ─────────────────────────────────────────

/// `#update`: `update!(data: data_params)`, so the data given replaces what
/// was stored, all of it.
pub async fn update_subscription(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    NestedParams(params): NestedParams,
) -> AppResult<Json<PushSubscription>> {
    auth.require_scope("push")?;

    let Some(id) = sqlx::query_scalar!(
        "SELECT id FROM web_push_subscriptions WHERE access_token_id = $1 ORDER BY id LIMIT 1",
        auth.token_id,
    )
    .fetch_optional(&state.db)
    .await?
    else {
        return Err(AppError::NotFound);
    };
    let data = data_params(params.get("data"))?;

    let row = sqlx::query!(
        r#"UPDATE web_push_subscriptions SET data = $2, updated_at = now()
           WHERE id = $1
           RETURNING id, endpoint, standard, data as "data: serde_json::Value""#,
        id,
        data,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    Ok(Json(PushSubscription::new(
        &state,
        row.id,
        row.endpoint,
        row.standard,
        row.data.as_ref(),
    )))
}

// ── DELETE /api/v1/push/subscription ──────────────────────────────────────

pub async fn delete_subscription(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<serde_json::Value>> {
    auth.require_scope("push")?;
    sqlx::query!(
        "DELETE FROM web_push_subscriptions WHERE access_token_id = $1",
        auth.token_id,
    )
    .execute(&state.db)
    .await?;

    Ok(Json(serde_json::json!({})))
}

// ── DELETE /api/web/push_subscriptions/:token ─────────────────────────────

/// `Api::Web::PushSubscriptionsController#destroy`, the `Unsubscribe-URL`
/// every push names: the subscription the token signs is destroyed, if it
/// still exists and the token has not expired, and the answer is a `200`
/// either way. It asks for no user, no CSRF token and no session.
pub async fn unsubscribe(
    state: AppState,
    axum::extract::Path(token): axum::extract::Path<String>,
) -> AppResult<axum::http::StatusCode> {
    if let Some(id) = crate::push::verify_unsubscribe_token(&state, &token, chrono::Utc::now()) {
        sqlx::query!("DELETE FROM web_push_subscriptions WHERE id = $1", id)
            .execute(&state.db)
            .await?;
    }
    Ok(axum::http::StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::data_params;
    use serde_json::json;

    #[test]
    fn data_is_stored_as_given_and_only_what_is_permitted() {
        // Absent alerts stay absent; nothing defaults to true.
        assert_eq!(
            data_params(Some(&json!({"alerts": {"mention": true}}))).unwrap(),
            json!({"alerts": {"mention": true}})
        );
        // Every `Notification::TYPES` key, in that order, policy first;
        // unknown keys dropped; form strings kept as strings.
        let data = data_params(Some(&json!({
            "alerts": {"admin.sign_up": "1", "bogus": true, "follow": false, "mention": "true"},
            "policy": "followed",
            "extra": 1,
        })))
        .unwrap();
        assert_eq!(
            serde_json::to_string(&data).unwrap(),
            r#"{"policy":"followed","alerts":{"mention":"true","follow":false,"admin.sign_up":"1"}}"#
        );
        // Blank data is `{}`.
        for blank in [json!(null), json!(""), json!({}), json!([])] {
            assert_eq!(data_params(Some(&blank)).unwrap(), json!({}));
        }
        assert_eq!(data_params(None).unwrap(), json!({}));
        // Data with nothing permitted, or that is no hash, is missing.
        assert!(data_params(Some(&json!({"bogus": 1}))).is_err());
        assert!(data_params(Some(&json!("x"))).is_err());
    }
}
