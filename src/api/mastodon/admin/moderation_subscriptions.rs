//! `Admin::ModerationSubscriptionsController` and
//! `Admin::ModerationSuggestionsController`, as an API. While the
//! `moderation_subscriptions` feature is off they answer 404.
//!
//! Mastodon: app/controllers/admin/moderation_subscriptions_controller.rb
//! Mastodon: app/controllers/admin/moderation_suggestions_controller.rb
//! Mastodon: app/helpers/admin/moderation_subscriptions_helper.rb
//! Mastodon: app/helpers/admin/moderation_suggestions_helper.rb
//! Mastodon: app/policies/moderation_subscription_policy.rb
//! Mastodon: app/policies/moderation_suggestion_policy.rb

use std::collections::BTreeMap;

use axum::{
    extract::{Extension, Path, Query},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::super::convert::mastodon_date;
use super::super::extractors::{FlexBool, Params};
use super::perm;
use crate::{
    error::{AppError, AppResult},
    middleware::AuthenticatedUser,
    moderation::{
        action_log::{log_for_subscription, Target},
        lists::{self, Subscription},
    },
    state::AppState,
};

const PAGE: i64 = 40;

/// `check_feature_enabled` and the policies' `manage_federation`.
async fn gate(state: &AppState, auth: &AuthenticatedUser, scope: &str) -> AppResult<()> {
    if !state.instance.feature_enabled(lists::FEATURE) {
        return Err(AppError::NotFound);
    }
    auth.require_scope(scope)?;
    super::require_permission(state, auth.account_id, perm::MANAGE_FEDERATION).await
}

#[derive(Debug, Serialize)]
pub struct ModerationSubscription {
    pub id: String,
    pub name: Option<String>,
    pub url: String,
    pub priority: Option<i32>,
    /// `csv_list` or `json`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// `accept`, `reject`, or null for a mixed list.
    pub list_action: Option<&'static str>,
    /// `never`, `safely` or `always`.
    pub apply_conditions: &'static str,
    pub retract_automatically: bool,
    pub override_unattributed: bool,
    pub last_synced_at: Option<String>,
    pub created_at: String,
}

impl From<Subscription> for ModerationSubscription {
    fn from(row: Subscription) -> Self {
        Self {
            id: row.id.to_string(),
            apply_conditions: row.apply_conditions(),
            name: row.name,
            url: row.url,
            priority: row.priority,
            kind: if row.kind == lists::TYPE_JSON {
                "json"
            } else {
                "csv_list"
            },
            list_action: row.list_action.map(lists::action_name),
            retract_automatically: row.retract_automatically,
            override_unattributed: row.override_unattributed,
            last_synced_at: row.last_synced_at.map(mastodon_date),
            created_at: mastodon_date(row.created_at),
        }
    }
}

/// `GET /api/v1/admin/moderation_subscriptions`.
pub async fn list_moderation_subscriptions(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<ModerationSubscription>>> {
    gate(&state, &auth, "admin:read").await?;
    let rows = lists::all(&state).await.map_err(AppError::Internal)?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

#[derive(Debug, Deserialize)]
pub struct SubscriptionForm {
    pub name: Option<String>,
    pub url: Option<String>,
    pub priority: Option<i64>,
    /// `accept`, `reject`, or blank for a mixed list.
    pub list_action: Option<String>,
    pub apply_conditions: Option<String>,
    pub retract_automatically: Option<FlexBool>,
    pub override_unattributed: Option<FlexBool>,
}

/// The attributes a form leaves a subscription with, validated.
fn assign(form: SubscriptionForm, current: Option<&Subscription>) -> AppResult<Assigned> {
    let name = form.name.or_else(|| current.and_then(|c| c.name.clone()));
    let name = name
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| AppError::Unprocessable("Validation failed: Name can't be blank".into()))?;
    let url = form
        .url
        .or_else(|| current.map(|c| c.url.clone()))
        .filter(|url| !url.trim().is_empty())
        .ok_or_else(|| AppError::Unprocessable("Validation failed: Url can't be blank".into()))?;
    // `validates :url, url: true`.
    let valid_url = url::Url::parse(&url)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some());
    if !valid_url {
        return Err(AppError::Unprocessable(
            "Validation failed: Url is invalid".into(),
        ));
    }
    let priority = form
        .priority
        .or_else(|| current.and_then(|c| c.priority).map(i64::from));
    let Some(priority) = priority else {
        return Err(AppError::Unprocessable(
            "Validation failed: Priority can't be blank".into(),
        ));
    };
    // `PRIORITY_LIMIT`: an int4, without its lowest value.
    let priority = i32::try_from(priority)
        .ok()
        .filter(|priority| *priority != i32::MIN)
        .ok_or_else(|| {
            AppError::Unprocessable("Validation failed: Priority is out of range".into())
        })?;
    let list_action = match form.list_action.as_deref() {
        None => current.and_then(|c| c.list_action),
        Some("" | "default") => None,
        Some("accept") => Some(lists::ACCEPT),
        Some("reject") => Some(lists::REJECT),
        Some(_) => {
            return Err(AppError::Unprocessable(
                "Validation failed: List action is not included in the list".into(),
            ))
        }
    };
    let conditions = form
        .apply_conditions
        .as_deref()
        .or(current.map(Subscription::apply_conditions))
        .unwrap_or("never");
    let (apply_automatically, preserve_relationships) = match conditions {
        "never" => (false, current.is_none_or(|c| c.preserve_relationships)),
        "safely" => (true, true),
        "always" => (true, false),
        _ => {
            return Err(AppError::Unprocessable(
                "Validation failed: Apply conditions is not included in the list".into(),
            ))
        }
    };
    Ok(Assigned {
        name,
        url,
        priority,
        list_action,
        apply_automatically,
        preserve_relationships,
        retract_automatically: form
            .retract_automatically
            .map(|b| b.0)
            .unwrap_or_else(|| current.is_none_or(|c| c.retract_automatically)),
        override_unattributed: form
            .override_unattributed
            .map(|b| b.0)
            .unwrap_or_else(|| current.is_some_and(|c| c.override_unattributed)),
    })
}

struct Assigned {
    name: String,
    url: String,
    priority: i32,
    list_action: Option<i32>,
    apply_automatically: bool,
    preserve_relationships: bool,
    retract_automatically: bool,
    override_unattributed: bool,
}

async fn found(state: &AppState, id: i64) -> AppResult<Subscription> {
    lists::find(state, id)
        .await
        .map_err(AppError::Internal)?
        .ok_or(AppError::NotFound)
}

/// `POST /api/v1/admin/moderation_subscriptions`: a CSV list, as the page can
/// only create. It is first read when the scheduler next runs.
pub async fn create_moderation_subscription(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Params(form): Params<SubscriptionForm>,
) -> AppResult<Json<ModerationSubscription>> {
    gate(&state, &auth, "admin:write").await?;
    let new = assign(form, None)?;
    let id = sqlx::query_scalar!(
        r#"INSERT INTO moderation_subscriptions
             (name, url, priority, type, list_action, apply_automatically, preserve_relationships,
              retract_automatically, override_unattributed, created_at, updated_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now(), now())
           RETURNING id"#,
        new.name,
        new.url,
        new.priority,
        lists::TYPE_CSV_LIST,
        new.list_action,
        new.apply_automatically,
        new.preserve_relationships,
        new.retract_automatically,
        new.override_unattributed,
    )
    .fetch_one(&state.db)
    .await?;
    log_for_subscription(
        &state.db,
        auth.account_id,
        "create",
        &Target::moderation_subscription(id, &new.name),
        None,
    )
    .await?;
    Ok(Json(found(&state, id).await?.into()))
}

/// `PATCH /api/v1/admin/moderation_subscriptions/:id`.
pub async fn update_moderation_subscription(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<SubscriptionForm>,
) -> AppResult<Json<ModerationSubscription>> {
    gate(&state, &auth, "admin:write").await?;
    let current = found(&state, id).await?;
    let new = assign(form, Some(&current))?;
    sqlx::query!(
        r#"UPDATE moderation_subscriptions
           SET name = $2, url = $3, priority = $4, list_action = $5, apply_automatically = $6,
               preserve_relationships = $7, retract_automatically = $8,
               override_unattributed = $9, updated_at = now()
           WHERE id = $1"#,
        id,
        new.name,
        new.url,
        new.priority,
        new.list_action,
        new.apply_automatically,
        new.preserve_relationships,
        new.retract_automatically,
        new.override_unattributed,
    )
    .execute(&state.db)
    .await?;
    log_for_subscription(
        &state.db,
        auth.account_id,
        "update",
        &Target::moderation_subscription(id, &new.name),
        None,
    )
    .await?;
    Ok(Json(found(&state, id).await?.into()))
}

/// `DELETE /api/v1/admin/moderation_subscriptions/:id`.
pub async fn delete_moderation_subscription(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    gate(&state, &auth, "admin:write").await?;
    let subscription = found(&state, id).await?;
    lists::destroy(&state, &subscription, auth.account_id)
        .await
        .map_err(AppError::Internal)?;
    Ok(Json(json!({})))
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    pub page: Option<i64>,
}

fn offset(query: &PageQuery) -> i64 {
    query
        .page
        .unwrap_or(1)
        .max(1)
        .saturating_sub(1)
        .saturating_mul(PAGE)
}

/// `GET /api/v1/admin/moderation_subscriptions/:id`: with a page of its
/// advisories.
pub async fn get_moderation_subscription(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Value>> {
    gate(&state, &auth, "admin:read").await?;
    let subscription = found(&state, id).await?;
    let advisories = sqlx::query!(
        r#"SELECT id, target_key, action FROM moderation_subscription_advisories
           WHERE moderation_subscription_id = $1 ORDER BY id ASC LIMIT $2 OFFSET $3"#,
        id,
        PAGE,
        offset(&query),
    )
    .fetch_all(&state.db)
    .await?;
    let total = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM moderation_subscription_advisories
           WHERE moderation_subscription_id = $1"#,
        id,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(json!({
        "subscription": ModerationSubscription::from(subscription),
        "advisories": advisories.into_iter().map(|a| json!({
            "id": a.id.to_string(),
            "target_key": a.target_key,
            "action": lists::action_name(a.action),
        })).collect::<Vec<_>>(),
        "advisories_count": total,
        "page_size": PAGE,
    })))
}

/// `GET /api/v1/admin/moderation_subscriptions/logs`.
pub async fn list_moderation_subscription_logs(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<Vec<super::action_logs::ActionLog>>> {
    gate(&state, &auth, "admin:read").await?;
    let rows = sqlx::query_as!(
        super::action_logs::LogRow,
        r#"SELECT id, account_id, action, target_type, target_id, human_identifier, route_param,
                  permalink, recorded_changes, moderation_subscription_id, created_at
           FROM admin_action_logs WHERE moderation_subscription_id IS NOT NULL
           ORDER BY id DESC LIMIT $1 OFFSET $2"#,
        PAGE,
        offset(&query),
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(super::action_logs::render_all(&state, rows).await?))
}

#[derive(Debug, Serialize, Clone)]
pub struct SubscriptionRef {
    pub id: String,
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SuggestionEntry {
    pub id: String,
    /// `retract`, `accept`, `reject` or `limit`.
    pub action: &'static str,
    /// `new`, `mailed`, `applied` or `dismissed`.
    pub state: &'static str,
    pub subscription: Option<SubscriptionRef>,
}

#[derive(Debug, Serialize)]
pub struct AdvisoryEntry {
    pub action: &'static str,
    pub subscription: SubscriptionRef,
}

/// What the suggestions page shows of one domain.
#[derive(Debug, Serialize)]
pub struct SuggestionTarget {
    pub target_type: &'static str,
    pub target_key: String,
    /// `accept`, `limit` or `reject`.
    pub current_state: &'static str,
    pub suggestions: Vec<SuggestionEntry>,
    pub advisories: Vec<AdvisoryEntry>,
}

/// The suggestions and advisories on these domains, by subscription priority.
pub(super) async fn targets(
    state: &AppState,
    domains: Vec<String>,
) -> AppResult<Vec<SuggestionTarget>> {
    let current = lists::current_states(state, &domains)
        .await
        .map_err(AppError::Internal)?;
    let suggestions = sqlx::query!(
        r#"SELECT g.id, g.target_key, g.action, g.state, s.id AS "subscription_id?",
                  s.name AS subscription_name
           FROM moderation_suggestions g
           LEFT JOIN moderation_subscriptions s ON s.id = g.moderation_subscription_id
           WHERE g.target_type = $1 AND g.target_key = ANY($2)
           ORDER BY g.id"#,
        lists::TARGET_DOMAIN,
        &domains,
    )
    .fetch_all(&state.db)
    .await?;
    let advisories = sqlx::query!(
        r#"SELECT a.target_key, a.action, s.id AS subscription_id, s.name AS subscription_name
           FROM moderation_subscription_advisories a
           JOIN moderation_subscriptions s ON s.id = a.moderation_subscription_id
           WHERE a.target_type = $1 AND a.target_key = ANY($2)
           ORDER BY s.priority ASC, a.id ASC"#,
        lists::TARGET_DOMAIN,
        &domains,
    )
    .fetch_all(&state.db)
    .await?;
    let mut by_target: BTreeMap<String, SuggestionTarget> = domains
        .into_iter()
        .map(|domain| {
            let current_state =
                lists::action_name(current.get(&domain).copied().unwrap_or(lists::ACCEPT));
            (
                domain.clone(),
                SuggestionTarget {
                    target_type: "domain",
                    target_key: domain,
                    current_state,
                    suggestions: vec![],
                    advisories: vec![],
                },
            )
        })
        .collect();
    for row in suggestions {
        if let Some(target) = by_target.get_mut(&row.target_key) {
            target.suggestions.push(SuggestionEntry {
                id: row.id.to_string(),
                action: lists::action_name(row.action),
                state: match row.state {
                    lists::STATE_NEW => "new",
                    lists::STATE_MAILED => "mailed",
                    lists::STATE_APPLIED => "applied",
                    _ => "dismissed",
                },
                subscription: row.subscription_id.map(|id| SubscriptionRef {
                    id: id.to_string(),
                    name: row.subscription_name.clone(),
                }),
            });
        }
    }
    for row in advisories {
        if let Some(target) = by_target.get_mut(&row.target_key) {
            target.advisories.push(AdvisoryEntry {
                action: lists::action_name(row.action),
                subscription: SubscriptionRef {
                    id: row.subscription_id.to_string(),
                    name: row.subscription_name,
                },
            });
        }
    }
    Ok(by_target.into_values().collect())
}

/// `GET /api/v1/admin/moderation_suggestions`: the domains with a suggestion
/// waiting for review.
pub async fn list_moderation_suggestions(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
) -> AppResult<Json<Vec<SuggestionTarget>>> {
    gate(&state, &auth, "admin:read").await?;
    let domains = sqlx::query_scalar!(
        r#"SELECT DISTINCT target_key FROM moderation_suggestions
           WHERE target_type = $1 AND state = ANY($2) ORDER BY target_key"#,
        lists::TARGET_DOMAIN,
        &[lists::STATE_NEW, lists::STATE_MAILED],
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(targets(&state, domains).await?))
}

async fn found_suggestion(state: &AppState, id: i64) -> AppResult<lists::Suggestion> {
    lists::find_suggestion(state, id)
        .await
        .map_err(AppError::Internal)?
        .ok_or(AppError::NotFound)
}

/// `DELETE /api/v1/admin/moderation_suggestions/:id`: dismisses every
/// suggestion for the same target.
pub async fn dismiss_moderation_suggestion(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
) -> AppResult<Json<Value>> {
    gate(&state, &auth, "admin:write").await?;
    let suggestion = found_suggestion(&state, id).await?;
    sqlx::query!(
        r#"UPDATE moderation_suggestions SET state = $1, updated_at = now()
           WHERE target_type = $2 AND target_key = $3"#,
        lists::STATE_DISMISSED,
        lists::TARGET_DOMAIN,
        suggestion.target_key,
    )
    .execute(&state.db)
    .await?;
    Ok(Json(json!({})))
}

#[derive(Debug, Deserialize)]
pub struct ApplyForm {
    pub confirm_downgrade: Option<FlexBool>,
    pub confirm: Option<FlexBool>,
}

/// `POST /api/v1/admin/moderation_suggestions/:id/apply`: `result` says what
/// came of it, or which confirmation to ask for and send back.
pub async fn apply_moderation_suggestion(
    state: AppState,
    Extension(auth): Extension<AuthenticatedUser>,
    Path(id): Path<i64>,
    Params(form): Params<ApplyForm>,
) -> AppResult<Json<Value>> {
    gate(&state, &auth, "admin:write").await?;
    let suggestion = found_suggestion(&state, id).await?;
    let result = lists::apply(
        &state,
        &suggestion,
        auth.account_id,
        form.confirm_downgrade.is_some_and(|b| b.0),
        form.confirm.is_some_and(|b| b.0),
    )
    .await
    .map_err(AppError::Internal)?;
    Ok(Json(json!({
        "result": result.name(),
        "target_key": suggestion.target_key,
    })))
}
