use axum::{extract::Extension, Json};
use serde::{Deserialize, Serialize};

use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    state::AppState,
};

// ── Subscription response type ─────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct PushSubscription {
    pub id: String,
    pub endpoint: String,
    pub standard: bool,
    pub alerts: PushAlerts,
    pub policy: String,
    pub server_key: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct PushAlerts {
    pub follow: bool,
    pub follow_request: bool,
    pub favourite: bool,
    pub reblog: bool,
    pub mention: bool,
    pub poll: bool,
    pub status: bool,
    pub update: bool,
}

fn alerts_from_data(data: &serde_json::Value) -> PushAlerts {
    let a = &data["alerts"];
    PushAlerts {
        follow: a["follow"].as_bool().unwrap_or(true),
        follow_request: a["follow_request"].as_bool().unwrap_or(false),
        favourite: a["favourite"].as_bool().unwrap_or(true),
        reblog: a["reblog"].as_bool().unwrap_or(true),
        mention: a["mention"].as_bool().unwrap_or(true),
        poll: a["poll"].as_bool().unwrap_or(false),
        status: a["status"].as_bool().unwrap_or(false),
        update: a["update"].as_bool().unwrap_or(false),
    }
}

fn policy_from_data(data: &serde_json::Value) -> String {
    data["policy"].as_str().unwrap_or("all").to_string()
}

// ── POST /api/v1/push/subscription ────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateSubscriptionBody {
    pub subscription: SubscriptionInput,
    #[serde(default)]
    pub data: SubscriptionData,
}

#[derive(Debug, Deserialize)]
pub struct SubscriptionInput {
    pub endpoint: String,
    pub keys: SubscriptionKeys,
    #[serde(default)]
    pub standard: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct SubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct SubscriptionData {
    #[serde(default)]
    pub alerts: AlertsInput,
    #[serde(default)]
    pub policy: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct AlertsInput {
    #[serde(default)]
    pub follow: Option<bool>,
    #[serde(default)]
    pub follow_request: Option<bool>,
    #[serde(default)]
    pub favourite: Option<bool>,
    #[serde(default)]
    pub reblog: Option<bool>,
    #[serde(default)]
    pub mention: Option<bool>,
    #[serde(default)]
    pub poll: Option<bool>,
    #[serde(default)]
    pub status: Option<bool>,
    #[serde(default)]
    pub update: Option<bool>,
}

pub async fn create_subscription(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(body): Json<CreateSubscriptionBody>,
) -> AppResult<Json<PushSubscription>> {
    auth.require_scope("push")?;

    let alerts = &body.data.alerts;
    let policy = body.data.policy.as_deref().unwrap_or("all");
    let data = serde_json::json!({
        "alerts": {
            "follow":          alerts.follow.unwrap_or(true),
            "follow_request":  alerts.follow_request.unwrap_or(false),
            "favourite":       alerts.favourite.unwrap_or(true),
            "reblog":          alerts.reblog.unwrap_or(true),
            "mention":         alerts.mention.unwrap_or(true),
            "poll":            alerts.poll.unwrap_or(false),
            "status":          alerts.status.unwrap_or(false),
            "update":          alerts.update.unwrap_or(false),
        },
        "policy": policy,
    });

    let standard = body.subscription.standard.unwrap_or(false);
    let user_id = auth.user_id.ok_or(AppError::Unauthorized)?;
    // Under `with_redis_lock("push_subscription:<user>")`, which, held by
    // another, raises `RaceConditionError`: the token's subscriptions
    // destroyed (`destroy_web_push_subscriptions!`), and a new one made.
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
        sqlx::query!(
            "DELETE FROM web_push_subscriptions WHERE access_token_id = $1",
            auth.token_id,
        )
        .execute(&state.db)
        .await?;
        let row = sqlx::query!(
            r#"INSERT INTO web_push_subscriptions
                 (access_token_id, endpoint, key_p256dh, key_auth, data, standard, user_id, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, now(), now())
               RETURNING id, standard, data as "data: serde_json::Value""#,
            auth.token_id,
            body.subscription.endpoint,
            body.subscription.keys.p256dh,
            body.subscription.keys.auth,
            data,
            standard,
            user_id,
        )
        .fetch_one(&state.db)
        .await?;
        Ok((row.id, row.standard, row.data))
    }
    .await;
    lock.release().await;
    let row = created?;

    Ok(Json(PushSubscription {
        id: row.0.to_string(),
        endpoint: body.subscription.endpoint,
        standard: row.1,
        alerts: alerts_from_data(row.2.as_ref().unwrap_or(&serde_json::json!({}))),
        policy: policy_from_data(row.2.as_ref().unwrap_or(&serde_json::json!({}))),
        server_key: state.instance.vapid_public_key.clone(),
    }))
}

// ── GET /api/v1/push/subscription ─────────────────────────────────────────

pub async fn get_subscription(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<PushSubscription>> {
    auth.require_scope("push")?;

    let row = sqlx::query!(
        r#"SELECT id, endpoint, standard, data as "data: serde_json::Value"
           FROM web_push_subscriptions
           WHERE access_token_id = $1"#,
        auth.token_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    Ok(Json(PushSubscription {
        id: row.id.to_string(),
        endpoint: row.endpoint,
        standard: row.standard,
        alerts: alerts_from_data(row.data.as_ref().unwrap_or(&serde_json::json!({}))),
        policy: policy_from_data(row.data.as_ref().unwrap_or(&serde_json::json!({}))),
        server_key: state.instance.vapid_public_key.clone(),
    }))
}

// ── PUT /api/v1/push/subscription ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct UpdateSubscriptionBody {
    #[serde(default)]
    pub data: SubscriptionData,
}

pub async fn update_subscription(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Json(body): Json<UpdateSubscriptionBody>,
) -> AppResult<Json<PushSubscription>> {
    auth.require_scope("push")?;

    let alerts = &body.data.alerts;
    let policy = body.data.policy.as_deref().unwrap_or("all");

    let row = sqlx::query!(
        r#"UPDATE web_push_subscriptions SET
             data = jsonb_build_object(
               'alerts', jsonb_build_object(
                 'follow',          COALESCE($1::boolean, (data->'alerts'->>'follow')::boolean, true),
                 'follow_request',  COALESCE($2::boolean, (data->'alerts'->>'follow_request')::boolean, false),
                 'favourite',       COALESCE($3::boolean, (data->'alerts'->>'favourite')::boolean, true),
                 'reblog',          COALESCE($4::boolean, (data->'alerts'->>'reblog')::boolean, true),
                 'mention',         COALESCE($5::boolean, (data->'alerts'->>'mention')::boolean, true),
                 'poll',            COALESCE($6::boolean, (data->'alerts'->>'poll')::boolean, false),
                 'status',          COALESCE($7::boolean, (data->'alerts'->>'status')::boolean, false),
                 'update',          COALESCE($8::boolean, (data->'alerts'->>'update')::boolean, false)
               ),
               'policy', $9::text
             ),
             updated_at = now()
           WHERE access_token_id = $10
           RETURNING id, endpoint, standard, data as "data: serde_json::Value""#,
        alerts.follow,
        alerts.follow_request,
        alerts.favourite,
        alerts.reblog,
        alerts.mention,
        alerts.poll,
        alerts.status,
        alerts.update,
        policy,
        auth.token_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;

    Ok(Json(PushSubscription {
        id: row.id.to_string(),
        endpoint: row.endpoint,
        standard: row.standard,
        alerts: alerts_from_data(row.data.as_ref().unwrap_or(&serde_json::json!({}))),
        policy: policy_from_data(row.data.as_ref().unwrap_or(&serde_json::json!({}))),
        server_key: state.instance.vapid_public_key.clone(),
    }))
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
